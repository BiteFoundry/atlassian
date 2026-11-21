//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0) for Jira & Confluence.
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::Result;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    atlassian::{
        body::{BodyEdit, SectionReplacement, TextReplacement},
        client::is_not_found_error,
        AtlassianClient, SiteClientPool,
    },
    config::{self, AppConfig},
    img_preview,
    models::{FederatedSearchHit, IssueUpdate, OutgoingIssue, OutgoingPage},
    serve::{clamp_limit, escape_jql_str, issue_to_hit, looks_like_query_expr, page_to_hit},
    storage::Storage,
    sync::{self, SiteSyncTarget},
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "atlassian";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// Run the stdio MCP server until stdin reaches EOF.
pub async fn run(config: AppConfig, client: AtlassianClient, storage: Storage) -> Result<()> {
    run_with_storage(&config, &client, |_| Ok(&storage)).await
}

pub(crate) async fn run_deferred(config: AppConfig, client: AtlassianClient) -> Result<()> {
    let cache = LazyLock::new(|| Storage::open(&config::cache_db_path(&config.profile)));
    let preview = LazyLock::new(Storage::open_in_memory);
    run_with_storage(&config, &client, |dry_run| {
        let storage = if dry_run { &*preview } else { &*cache };
        storage.as_ref().map_err(|error| error.to_string())
    })
    .await
}

async fn run_with_storage<'a>(
    config: &AppConfig,
    client: &AtlassianClient,
    mut get_storage: impl FnMut(bool) -> Result<&'a Storage, String>,
) -> Result<()> {
    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, deployment={}, site={})",
        config.deployment.as_str(),
        if config.site_url.is_empty() {
            "unconfigured"
        } else {
            &config.site_url
        }
    );

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = std::io::stdout();
    let mut bootstrap_pending = true;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(response) = handle_rpc_line_with_bootstrap(
            config,
            client,
            &mut get_storage,
            trimmed,
            &mut bootstrap_pending,
        )
        .await
        {
            stdout.write_all(encode_message(&response).as_bytes())?;
            stdout.flush()?;
        }
    }

    eprintln!("{SERVER_NAME} MCP server: stdin closed, shutting down");
    Ok(())
}

async fn bootstrap_if_empty(config: &AppConfig, client: &AtlassianClient, storage: &Storage) {
    let site = client.site_id();
    let projects_empty = storage
        .list_projects_for_site(site)
        .unwrap_or_default()
        .is_empty();
    let spaces_empty = storage
        .list_spaces_for_site(site)
        .unwrap_or_default()
        .is_empty();
    if projects_empty && spaces_empty && !config.site_url.trim().is_empty() {
        if let Err(err) = sync::bootstrap_workspace_for_site(site, client, storage).await {
            eprintln!("{SERVER_NAME} MCP warning: initial workspace bootstrap failed: {err:#}");
        }
    }
}

fn tool_uses_current_site_cache(
    config: &AppConfig,
    client: &AtlassianClient,
    name: &str,
    arguments: &Value,
) -> bool {
    let enabled = |key| arguments.get(key).and_then(Value::as_bool) == Some(true);
    let reads_cache = match name {
        "list_sites" => return true,
        "list_projects" | "list_boards" | "list_confluence_spaces" => !enabled("refresh"),
        "list_sprints"
        | "get_board_issues"
        | "search_jira_issues"
        | "get_jira_issue"
        | "list_space_pages"
        | "list_child_pages"
        | "get_confluence_page" => enabled("offline"),
        "search_confluence_pages" => {
            !enabled("remote") && Args::new(arguments).optional_str("cql").ok().flatten().is_none()
        }
        "search_all" => !enabled("remote"),
        _ => false,
    };
    reads_cache
        && resolve_mcp_site_id(config, client, arguments.get("site").and_then(Value::as_str))
            .map_or(true, |site| site.eq_ignore_ascii_case(client.site_id()))
}

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    config: &AppConfig,
    client: &AtlassianClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    handle_rpc_line_with_bootstrap(config, client, |_| Ok(storage), line, &mut false).await
}

async fn handle_rpc_line_with_bootstrap<'a>(
    config: &AppConfig,
    client: &AtlassianClient,
    get_storage: impl FnOnce(bool) -> Result<&'a Storage, String>,
    line: &str,
    bootstrap_pending: &mut bool,
) -> Option<Value> {
    let message = match parse_message(line) {
        Ok(msg) => msg,
        Err(err_resp) => return Some(err_resp),
    };

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let has_id = message.get("id").is_some();

    // Notifications (including `notifications/initialized`) never receive a response.
    if method.starts_with("notifications/") || !has_id {
        return None;
    }

    match method {
        "initialize" => Some(success_response(id, initialize_result(&message))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = match validate_tool_args(name, &arguments).and_then(|()| {
                let is_edit_preview =
                    matches!(name, "update_jira_issue" | "update_confluence_page")
                        && arguments.get("dry_run").and_then(Value::as_bool) == Some(true);
                get_storage(is_edit_preview)
            }) {
                Ok(storage) => {
                    if *bootstrap_pending
                        && tool_uses_current_site_cache(config, client, name, &arguments)
                    {
                        *bootstrap_pending = false;
                        bootstrap_if_empty(config, client, storage).await;
                    }
                    call_tool(config, client, storage, name, &arguments).await
                }
                Err(error) => Err(error),
            };

            Some(success_response(id, tool_result(outcome)))
        }
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

fn resolve_mcp_site_id(
    config: &AppConfig,
    fallback_client: &AtlassianClient,
    raw_site: Option<&str>,
) -> Option<String> {
    let trimmed = raw_site.map(str::trim).filter(|s| !s.is_empty())?;
    if trimmed.eq_ignore_ascii_case("all") {
        return None;
    }
    if let Some(found) = config.find_effective_site(trimmed) {
        return Some(found.id);
    }
    if trimmed.eq_ignore_ascii_case(fallback_client.site_id()) {
        return Some(fallback_client.site_id().to_string());
    }
    Some(trimmed.to_string())
}

fn resolve_mcp_single_site_id(
    config: &AppConfig,
    fallback_client: &AtlassianClient,
    raw_site: Option<&str>,
) -> Result<String, String> {
    let Some(trimmed) = raw_site.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(fallback_client.site_id().to_string());
    };
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(fallback_client.site_id().to_string());
    }
    if let Some(found) = config.find_effective_site(trimmed) {
        return Ok(found.id);
    }
    if trimmed.eq_ignore_ascii_case(fallback_client.site_id()) {
        return Ok(fallback_client.site_id().to_string());
    }
    Err(format!("site not found: '{trimmed}'"))
}

fn resolve_mcp_client(
    config: &AppConfig,
    fallback_client: &AtlassianClient,
    raw_site: Option<&str>,
) -> Result<(String, AtlassianClient), String> {
    let Some(trimmed) = raw_site.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((
            fallback_client.site_id().to_string(),
            fallback_client.clone(),
        ));
    };
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok((
            fallback_client.site_id().to_string(),
            fallback_client.clone(),
        ));
    }
    let canonical = config
        .find_effective_site(trimmed)
        .map(|s| s.id)
        .unwrap_or_else(|| trimmed.to_string());
    if canonical.eq_ignore_ascii_case(fallback_client.site_id()) {
        return Ok((
            fallback_client.site_id().to_string(),
            fallback_client.clone(),
        ));
    }
    if let Ok(pool) = SiteClientPool::from_config(config) {
        if let Ok(c) = pool.client_for(&canonical) {
            return Ok((c.site_id().to_string(), c));
        }
    }
    Err(format!("site not found: '{trimmed}'"))
}

async fn refresh_mcp_issue_cache(
    client: &AtlassianClient,
    storage: &Storage,
    site_id: &str,
    key: &str,
) {
    if let Ok((mut detail, meta)) = client.get_issue_with_meta(key).await {
        let _ = storage.upsert_issue_detail_for_site_merging(
            site_id,
            &mut detail,
            !meta.remote_links_ok,
        );
    }
}

async fn refresh_mcp_page_cache(
    client: &AtlassianClient,
    storage: &Storage,
    site_id: &str,
    page_id: &str,
) {
    if let Ok((mut detail, meta)) = client.get_page_with_meta(page_id).await {
        let _ = storage.upsert_page_detail_for_site_merging(
            site_id,
            &mut detail,
            !meta.labels_ok,
            !meta.attachments_ok,
            !meta.comments_ok,
        );
    }
}

async fn call_tool(
    config: &AppConfig,
    client: &AtlassianClient,
    storage: &Storage,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let args = Args::new(arguments);
    let site_arg = args.optional_str("site")?;
    let scoped_site = resolve_mcp_site_id(config, client, site_arg.as_deref());

    match name {
        // -------------------------------------------------------------------
        // Multi-site registry (1)
        // -------------------------------------------------------------------
        "list_sites" => {
            let mut sites_out = Vec::new();
            let mut seen_ids = std::collections::HashSet::new();
            for site in config.effective_sites() {
                seen_ids.insert(site.id.to_ascii_lowercase());
                let counts = storage
                    .cache_counts_for_site(&site.id)
                    .unwrap_or_default();
                let db_rec = storage.get_site(&site.id).ok().flatten();
                sites_out.push(json!({
                    "id": site.id,
                    "name": site.display_name(),
                    "site_url": site.site_url,
                    "confluence_url": site.effective_confluence_root(),
                    "deployment": site.deployment.as_str(),
                    "cloud_id": site.cloud_id,
                    "account_id": site.account_id,
                    "enabled": site.enabled,
                    "issue_count": counts.issues,
                    "page_count": counts.pages,
                    "last_synced_at": db_rec.as_ref().and_then(|r| r.last_synced_at.clone()),
                    "last_error": db_rec.as_ref().and_then(|r| r.last_error.clone()),
                }));
            }
            if let Ok(db_sites) = storage.list_sites() {
                for rec in db_sites {
                    if seen_ids.insert(rec.site_id.to_ascii_lowercase()) {
                        let counts = storage
                            .cache_counts_for_site(&rec.site_id)
                            .unwrap_or_default();
                        sites_out.push(json!({
                            "id": rec.site_id,
                            "name": rec.name,
                            "site_url": rec.site_url,
                            "confluence_url": "",
                            "deployment": rec.deployment,
                            "cloud_id": null,
                            "account_id": "default",
                            "enabled": rec.enabled,
                            "issue_count": counts.issues,
                            "page_count": counts.pages,
                            "last_synced_at": rec.last_synced_at,
                            "last_error": rec.last_error,
                        }));
                    }
                }
            }
            Ok(json!({
                "active_site": config.active_site,
                "default_site": config.default_site,
                "sites": sites_out,
            }))
        }

        // -------------------------------------------------------------------
        // Jira
        // -------------------------------------------------------------------
        "get_myself" => {
            let (site_id, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let me = target_client.get_myself().await.map_err(format_error)?;
            target_client.set_cached_me(me.clone());
            Ok(json!({ "site_id": site_id, "user": me }))
        }

        "list_projects" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_projects_for_site(&target_site)
                .map_err(format_error)?;
            let projects = if refresh || cached.is_empty() {
                if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
                    cached
                } else {
                    let (target_site, target_client) =
                        resolve_mcp_client(config, client, site_arg.as_deref())?;
                    let fetched = target_client.list_projects().await.map_err(format_error)?;
                    let _ = storage.upsert_projects_for_site(&target_site, &fetched);
                    let stored = storage
                        .list_projects_for_site(&target_site)
                        .map_err(format_error)?;
                    if stored.is_empty() {
                        fetched
                    } else {
                        stored
                    }
                }
            } else {
                cached
            };
            Ok(json!({ "site_id": target_site, "projects": projects }))
        }

        "list_boards" => {
            let project = args.optional_str("project")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_boards_for_site(&target_site, project.as_deref())
                .map_err(format_error)?;
            let boards = if refresh || cached.is_empty() {
                if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
                    cached
                } else {
                    let (target_site, target_client) =
                        resolve_mcp_client(config, client, site_arg.as_deref())?;
                    let fetched = target_client
                        .list_boards(project.as_deref())
                        .await
                        .map_err(format_error)?;
                    let _ = storage.upsert_boards_for_site(&target_site, &fetched);
                    let stored = storage
                        .list_boards_for_site(&target_site, project.as_deref())
                        .map_err(format_error)?;
                    if stored.is_empty() {
                        fetched
                    } else {
                        stored
                    }
                }
            } else {
                cached
            };
            Ok(json!({ "site_id": target_site, "boards": boards }))
        }

        "list_sprints" => {
            let board_id = args.required_i64("board_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let sprints = if offline {
                storage
                    .list_sprints_for_site(&target_site, board_id)
                    .map_err(format_error)?
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_board_sprints(board_id)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_sprints_for_site(&target_site, &fetched);
                fetched
            };
            Ok(json!({ "site_id": target_site, "board_id": board_id, "sprints": sprints }))
        }

        "get_board_issues" => {
            let board_id = args.required_i64("board_id")?;
            let sprint_id = args.optional_i64("sprint_id")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;

            let issues = if offline {
                storage
                    .list_board_issues_for_site(&target_site, board_id, sprint_id, limit)
                    .map_err(format_error)?
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_board_issues(board_id, sprint_id, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_board_issues_for_site(
                    &target_site,
                    board_id,
                    sprint_id,
                    &fetched,
                );
                if fetched.len() < limit {
                    let keep: Vec<String> = fetched.iter().map(|i| i.key.clone()).collect();
                    let _ = storage.prune_board_issues_for_site(
                        &target_site,
                        board_id,
                        sprint_id,
                        &keep,
                    );
                }
                fetched
            };
            Ok(json!({
                "site_id": target_site,
                "board_id": board_id,
                "sprint_id": sprint_id,
                "issues": issues,
            }))
        }

        "search_jira_issues" => {
            let jql = args.optional_str("jql")?;
            let query = args.optional_str("query")?;
            let project = args.optional_str("project")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let page_token = args.optional_str("page_token")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            if offline {
                if let Some(q) = query.as_deref().or(jql.as_deref()) {
                    let hits: Vec<FederatedSearchHit> = storage
                        .search_issues_scoped(q, scoped_site.as_deref(), limit)
                        .map_err(format_error)?
                        .into_iter()
                        .map(|(sid, hit)| {
                            let sname = config
                                .find_effective_site(&sid)
                                .map(|s| s.display_name().to_string());
                            FederatedSearchHit::new(sid, sname, hit)
                        })
                        .collect();
                    return Ok(json!({
                        "offline": true,
                        "site_id": scoped_site,
                        "query": q,
                        "hits": hits,
                    }));
                }
                let target_site =
                    resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
                let issues = storage
                    .list_issues_for_site(&target_site, project.as_deref(), limit)
                    .map_err(format_error)?;
                return Ok(json!({
                    "offline": true,
                    "site_id": target_site,
                    "issues": issues,
                }));
            }

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let effective_jql = if let Some(raw_jql) = jql {
                raw_jql
            } else if let Some(q) = query {
                if looks_like_query_expr(&q) {
                    q
                } else if let Some(proj) = project.as_deref() {
                    format!(
                        "project = \"{}\" AND text ~ \"{}\" ORDER BY updated DESC",
                        escape_jql_str(proj),
                        escape_jql_str(&q)
                    )
                } else {
                    format!("text ~ \"{}\" ORDER BY updated DESC", escape_jql_str(&q))
                }
            } else if let Some(proj) = project.as_deref().or(config.default_project.as_deref()) {
                format!(
                    "project = \"{}\" ORDER BY updated DESC",
                    escape_jql_str(proj.trim())
                )
            } else {
                AtlassianClient::my_open_issues_jql().to_string()
            };

            let page = target_client
                .search_issues_jql(&effective_jql, limit, page_token.as_deref(), None)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_issues_for_site(&target_site, &page.issues);

            Ok(json!({
                "site_id": target_site,
                "jql": effective_jql,
                "issues": page.issues,
                "next_page_token": page.next_page_token,
                "next_start_at": page.next_start_at,
                "total": page.total,
            }))
        }

        "get_jira_issue" => {
            let key = args.required_str("key")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            if offline {
                let target_site =
                    resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
                let detail = storage
                    .get_issue_detail_for_site(&target_site, &key)
                    .map_err(format_error)?
                    .ok_or_else(|| format!("issue not found in cache: {key}"))?;
                let stale = storage
                    .is_issue_detail_stale_for_site(&target_site, &key)
                    .unwrap_or(false);
                return Ok(json!({
                    "site_id": target_site,
                    "detail_stale": stale,
                    "issue": detail.output(compact),
                }));
            }
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (mut fetched, meta) = match target_client.get_issue_with_meta(&key).await {
                Ok(res) => res,
                Err(err) => {
                    if is_not_found_error(&err) {
                        let _ = storage.delete_issue_for_site(&target_site, &key);
                    }
                    return Err(format_error(err));
                }
            };
            let _ = storage.upsert_issue_detail_for_site_merging(
                &target_site,
                &mut fetched,
                !meta.remote_links_ok,
            );
            Ok(json!({
                "site_id": target_site,
                "detail_stale": false,
                "issue": fetched.output(compact),
            }))
        }

        "create_jira_issue" => {
            let project_key = args.required_str("project_key")?;
            let summary = args.required_str("summary")?;
            let issue_type = args
                .optional_str("issue_type")?
                .unwrap_or_else(|| "Task".to_string());
            let description_markdown = args.optional_text("description_markdown")?.unwrap_or_default();
            let priority = args.optional_str("priority")?;
            let assignee_id = args.optional_str("assignee_id")?;
            let labels = args.optional_str_array("labels")?;
            let parent_key = args.optional_str("parent_key")?;

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let req = OutgoingIssue {
                project_key,
                issue_type,
                summary,
                description_markdown,
                priority,
                assignee_id,
                labels,
                parent_key,
            };
            let detail = target_client
                .create_issue(&req)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
            Ok(json!({ "ok": true, "site_id": target_site, "issue": detail }))
        }

        "update_jira_issue" => {
            let key = args.required_str("key")?;
            let summary = args.optional_str("summary")?;
            let (full, append, replacement) = args.body_inputs("description_markdown")?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                None,
            )
            .map_err(|e| e.to_string())?;
            let priority = args.optional_str("priority")?;
            let assignee_id = args.optional_nullable_str("assignee_id")?;
            let labels = args.optional_str_array_opt("labels")?;

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let patch = IssueUpdate {
                summary,
                description_markdown: None,
                priority,
                assignee_id,
                labels,
            };
            if args.optional_bool("dry_run")?.unwrap_or(false) {
                let preview = target_client
                    .preview_issue_edit(&key, &patch, edit)
                    .await
                    .map_err(format_error)?;
                return Ok(
                    json!({ "ok": true, "site_id": target_site, "dry_run": true, "preview": preview }),
                );
            }
            let detail = target_client
                .edit_issue(&key, &patch, edit)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
            Ok(json!({ "ok": true, "site_id": target_site, "issue": detail }))
        }

        "list_jira_transitions" => {
            let key = args.required_str("key")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let transitions = target_client
                .list_transitions(&key)
                .await
                .map_err(format_error)?;
            Ok(json!({ "site_id": target_site, "key": key, "transitions": transitions }))
        }

        "transition_jira_issue" => {
            let key = args.required_str("key")?;
            let transition = args.required_str("transition")?;
            let comment = args.optional_text("comment_markdown")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            target_client
                .transition_issue(&key, &transition, comment.as_deref())
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            Ok(json!({
                "ok": true,
                "site_id": target_site,
                "key": key,
                "transition": transition,
            }))
        }

        "assign_jira_issue" => {
            let key = args.required_str("key")?;
            let raw_assignee = args.optional_str("assignee_id")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let resolved = match raw_assignee.as_deref() {
                None => None,
                Some(v) if v.eq_ignore_ascii_case("me") => {
                    let me = target_client.get_myself().await.map_err(format_error)?;
                    Some(me.id)
                }
                Some(v) => Some(v.to_string()),
            };
            target_client
                .assign_issue(&key, resolved.as_deref())
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            Ok(json!({
                "ok": true,
                "site_id": target_site,
                "key": key,
                "assignee_id": resolved,
            }))
        }

        "add_jira_comment" => {
            let key = args.required_str("key")?;
            let body_markdown = args.required_text("body_markdown")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let comment = target_client
                .add_issue_comment(&key, &body_markdown)
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            Ok(json!({ "ok": true, "site_id": target_site, "comment": comment }))
        }

        "set_jira_parent" => {
            let key = args.required_str("key")?;
            let parent = args.required_str("parent_key")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let parent = client
                .set_issue_parent(&key, &parent)
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&client, storage, &site, &key).await;
            Ok(json!({"ok": true, "site_id": site, "key": key, "parent_key": parent}))
        }

        "update_jira_custom_fields" => {
            let key = args.required_str("key")?;
            let fields = args.custom_fields()?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let updated = client
                .edit_issue_custom_fields(&key, fields)
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&client, storage, &site, &key).await;
            Ok(json!({"ok": true, "site_id": site, "key": key, "custom_fields": updated}))
        }

        "create_jira_issue_link" => {
            let inward = args.required_str("inward_key")?;
            let outward = args.required_str("outward_key")?;
            let link_type = args.required_str("link_type")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let link = client
                .create_issue_link(&inward, &outward, &link_type)
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&client, storage, &site, &inward).await;
            refresh_mcp_issue_cache(&client, storage, &site, &outward).await;
            Ok(json!({"ok": true, "site_id": site, "link": link}))
        }

        "move_jira_issues_to_sprint" => {
            let sprint_id = args.required_i64("sprint_id")?;
            let keys = args.required_str_array("issue_keys")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let confirmed = client
                .move_issues_to_sprint(sprint_id, &keys)
                .await
                .map_err(format_error)?;
            for key in &confirmed {
                refresh_mcp_issue_cache(&client, storage, &site, key).await;
            }
            Ok(
                json!({"ok": true, "site_id": site, "sprint_id": sprint_id, "issue_keys": confirmed}),
            )
        }

        "add_jira_worklog" => {
            let key = args.required_str("key")?;
            let seconds = args.worklog_seconds()?;
            let started = args.optional_str("started")?;
            let comment = args.optional_text("comment_markdown")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let worklog = client
                .log_issue_work(&key, seconds, started.as_deref(), comment.as_deref())
                .await
                .map_err(format_error)?;
            Ok(json!({"ok": true, "site_id": site, "key": key, "worklog": worklog}))
        }

        "upload_jira_attachment" => {
            let key = args.required_str("key")?;
            let path = args.required_str("path")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let uploaded = target_client
                .upload_issue_attachment(&key, Path::new(&path))
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            Ok(json!({ "ok": true, "site_id": target_site, "key": key, "attachments": uploaded }))
        }

        "delete_jira_issue" => {
            let key = args.required_str("key")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            target_client
                .delete_issue(&key)
                .await
                .map_err(format_error)?;
            let _ = storage.delete_issue_for_site(&target_site, &key);
            Ok(json!({ "ok": true, "site_id": target_site, "key": key }))
        }

        // -------------------------------------------------------------------
        // Confluence
        // -------------------------------------------------------------------
        "list_confluence_spaces" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_spaces_for_site(&target_site)
                .map_err(format_error)?;
            let mut spaces = if refresh || cached.is_empty() {
                if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
                    cached
                } else {
                    let (target_site, target_client) =
                        resolve_mcp_client(config, client, site_arg.as_deref())?;
                    let fetched = target_client
                        .list_spaces(limit)
                        .await
                        .map_err(format_error)?;
                    let _ = storage.upsert_spaces_for_site(&target_site, &fetched);
                    let stored = storage
                        .list_spaces_for_site(&target_site)
                        .map_err(format_error)?;
                    if stored.is_empty() {
                        fetched
                    } else {
                        stored
                    }
                }
            } else {
                cached
            };
            spaces.truncate(limit);
            Ok(json!({ "site_id": target_site, "spaces": spaces }))
        }

        "list_space_pages" => {
            let space_key = args.required_str("space_key")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let mut pages = if offline {
                storage
                    .list_space_root_pages_for_site(&target_site, &space_key)
                    .map_err(format_error)?
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_space_root_pages(&space_key, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_pages_for_site(&target_site, &fetched);
                if fetched.len() < limit {
                    let keep: Vec<String> = fetched.iter().map(|p| p.id.clone()).collect();
                    let _ =
                        storage.prune_space_root_pages_for_site(&target_site, &space_key, &keep);
                }
                fetched
            };
            pages.truncate(limit);
            Ok(json!({ "site_id": target_site, "space_key": space_key, "pages": pages }))
        }

        "list_child_pages" => {
            let page_id = args.required_str("page_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let pages = if offline {
                storage
                    .list_child_pages_for_site(&target_site, &page_id)
                    .map_err(format_error)?
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_child_pages(&page_id)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_pages_for_site(&target_site, &fetched);
                if fetched.len() < 100 {
                    let keep: Vec<String> = fetched.iter().map(|p| p.id.clone()).collect();
                    let _ = storage.prune_child_pages_for_site(&target_site, &page_id, &keep);
                }
                fetched
            };
            Ok(json!({ "site_id": target_site, "parent_id": page_id, "pages": pages }))
        }

        "get_confluence_page" => {
            let page_id = args.required_str("page_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            if offline {
                let target_site =
                    resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
                let detail = storage
                    .get_page_detail_for_site(&target_site, &page_id)
                    .map_err(format_error)?
                    .ok_or_else(|| format!("page not found in cache: {page_id}"))?;
                let stale = storage
                    .is_page_detail_stale_for_site(&target_site, &page_id)
                    .unwrap_or(false);
                return Ok(json!({
                    "site_id": target_site,
                    "detail_stale": stale,
                    "page": detail.output(compact),
                }));
            }
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (mut fetched, meta) = match target_client.get_page_with_meta(&page_id).await {
                Ok(res) => res,
                Err(err) => {
                    if is_not_found_error(&err) {
                        let _ = storage.delete_page_for_site(&target_site, &page_id);
                    }
                    return Err(format_error(err));
                }
            };
            let _ = storage.upsert_page_detail_for_site_merging(
                &target_site,
                &mut fetched,
                !meta.labels_ok,
                !meta.attachments_ok,
                !meta.comments_ok,
            );
            Ok(json!({
                "site_id": target_site,
                "detail_stale": false,
                "page": fetched.output(compact),
            }))
        }

        "create_confluence_page" => {
            let space_id_or_key = args.required_str("space_id_or_key")?;
            let parent_id = args.optional_str("parent_id")?;
            let title = args.required_str("title")?;
            let body_markdown = args.optional_text("body_markdown")?.unwrap_or_default();

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let req = OutgoingPage {
                space_id_or_key,
                parent_id,
                title,
                body_markdown,
            };
            let detail = target_client
                .create_page(&req)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
            Ok(json!({ "ok": true, "site_id": target_site, "page": detail }))
        }

        "update_confluence_page" => {
            let page_id = args.required_str("page_id")?;
            let title = args.optional_str("title")?;
            let (full, append, replacement) = args.body_inputs("body_markdown")?;
            let section = args.section_replacement()?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                section.as_ref(),
            )
            .map_err(|e| e.to_string())?;
            let prev_version = args.page_version()?;
            let version_message = args.optional_str("version_message")?;

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            if args.optional_bool("dry_run")?.unwrap_or(false) {
                let preview = target_client
                    .preview_page_edit(&page_id, title, edit, prev_version, version_message)
                    .await
                    .map_err(format_error)?;
                return Ok(
                    json!({ "ok": true, "site_id": target_site, "dry_run": true, "preview": preview }),
                );
            }
            let detail = target_client
                .edit_page(&page_id, title, edit, prev_version, version_message)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
            Ok(json!({ "ok": true, "site_id": target_site, "page": detail }))
        }

        "add_confluence_comment" => {
            let page_id = args.required_str("page_id")?;
            let body_markdown = args.required_text("body_markdown")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let comment = target_client
                .add_page_comment(&page_id, &body_markdown)
                .await
                .map_err(format_error)?;
            refresh_mcp_page_cache(&target_client, storage, &target_site, &page_id).await;
            Ok(json!({ "ok": true, "site_id": target_site, "comment": comment }))
        }

        "search_confluence_pages" => {
            let query = args.optional_str("query")?;
            let cql = args.optional_str("cql")?;
            let remote = args.optional_bool("remote")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(30));

            if remote || cql.is_some() {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let effective_cql = match cql {
                    Some(raw) => raw,
                    None => {
                        let q = query.as_deref().unwrap_or_default();
                        if looks_like_query_expr(q) {
                            q.to_string()
                        } else {
                            format!("text ~ \"{}\"", escape_jql_str(q))
                        }
                    }
                };
                let pages = target_client
                    .search_confluence_cql(&effective_cql, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_pages_for_site(&target_site, &pages);
                return Ok(json!({
                    "site_id": target_site,
                    "remote": true,
                    "cql": effective_cql,
                    "pages": pages,
                }));
            }

            let q = query.unwrap_or_default();
            let hits: Vec<FederatedSearchHit> = storage
                .search_pages_scoped(&q, scoped_site.as_deref(), limit)
                .map_err(format_error)?
                .into_iter()
                .map(|(sid, hit)| {
                    let sname = config
                        .find_effective_site(&sid)
                        .map(|s| s.display_name().to_string());
                    FederatedSearchHit::new(sid, sname, hit)
                })
                .collect();
            Ok(json!({
                "site_id": scoped_site,
                "remote": false,
                "query": q,
                "hits": hits,
            }))
        }

        "upload_confluence_attachment" => {
            let page_id = args.required_str("page_id")?;
            let path = args.required_str("path")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let attachments = client
                .upload_page_attachment(&page_id, Path::new(&path))
                .await
                .map_err(format_error)?;
            refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
            Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "attachments": attachments}))
        }

        "move_confluence_page" => {
            let page_id = args.required_str("page_id")?;
            let parent_id = args.required_str("parent_id")?;
            let version = args.page_version()?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let page = client
                .move_page(&page_id, &parent_id, version)
                .await
                .map_err(format_error)?;
            refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
            Ok(json!({"ok": true, "site_id": site, "page": page}))
        }

        "add_confluence_labels" => {
            let page_id = args.required_str("page_id")?;
            let labels = args.required_str_array("labels")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            let added = client
                .add_page_labels(&page_id, &labels)
                .await
                .map_err(format_error)?;
            refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
            Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "added": added}))
        }

        "remove_confluence_label" => {
            let page_id = args.required_str("page_id")?;
            let label = args.required_str("label")?;
            let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
            client
                .remove_page_label(&page_id, &label)
                .await
                .map_err(format_error)?;
            refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
            Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "removed": label}))
        }

        "delete_confluence_page" => {
            let page_id = args.required_str("page_id")?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            target_client
                .delete_page(&page_id)
                .await
                .map_err(format_error)?;
            let _ = storage.delete_page_for_site(&target_site, &page_id);
            Ok(json!({ "ok": true, "site_id": target_site, "page_id": page_id }))
        }

        // -------------------------------------------------------------------
        // Unified
        // -------------------------------------------------------------------
        "search_all" => {
            let query = args.required_str("query")?;
            let scope = args
                .optional_str("scope")?
                .unwrap_or_else(|| "all".to_string())
                .to_ascii_lowercase();
            let remote = args.optional_bool("remote")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(30));

            let hits: Vec<FederatedSearchHit> = if !remote {
                let raw_hits = match scope.as_str() {
                    "jira" => storage
                        .search_issues_scoped(&query, scoped_site.as_deref(), limit)
                        .map_err(format_error)?,
                    "confluence" => storage
                        .search_pages_scoped(&query, scoped_site.as_deref(), limit)
                        .map_err(format_error)?,
                    _ => storage
                        .unified_search_scoped(&query, scoped_site.as_deref(), limit)
                        .map_err(format_error)?,
                };
                raw_hits
                    .into_iter()
                    .map(|(sid, hit)| {
                        let sname = config
                            .find_effective_site(&sid)
                            .map(|s| s.display_name().to_string());
                        FederatedSearchHit::new(sid, sname, hit)
                    })
                    .collect()
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let target_name = config
                    .find_effective_site(&target_site)
                    .map(|s| s.display_name().to_string());
                let mut combined = Vec::new();
                if scope == "all" || scope == "jira" {
                    let jql = if looks_like_query_expr(&query) {
                        query.clone()
                    } else {
                        format!("text ~ \"{}\"", escape_jql_str(&query))
                    };
                    let page = target_client
                        .search_issues_jql(&jql, limit, None, None)
                        .await
                        .map_err(format_error)?;
                    let _ = storage.upsert_issues_for_site(&target_site, &page.issues);
                    combined.extend(page.issues.iter().map(|i| {
                        FederatedSearchHit::new(
                            target_site.clone(),
                            target_name.clone(),
                            issue_to_hit(i),
                        )
                    }));
                }
                if scope == "all" || scope == "confluence" {
                    let cql = if looks_like_query_expr(&query) {
                        query.clone()
                    } else {
                        format!("text ~ \"{}\"", escape_jql_str(&query))
                    };
                    let pages = target_client
                        .search_confluence_cql(&cql, limit)
                        .await
                        .map_err(format_error)?;
                    let _ = storage.upsert_pages_for_site(&target_site, &pages);
                    combined.extend(pages.iter().map(|p| {
                        FederatedSearchHit::new(
                            target_site.clone(),
                            target_name.clone(),
                            page_to_hit(p),
                        )
                    }));
                }
                combined.truncate(limit);
                combined
            };

            Ok(json!({
                "query": query,
                "scope": scope,
                "site_id": scoped_site,
                "remote": remote,
                "hits": hits,
            }))
        }

        "sync_workspace" => {
            if scoped_site.is_none() {
                if let Ok(pool) = SiteClientPool::from_config(config) {
                    let enabled = pool.enabled_clients();
                    if enabled.len() > 1 {
                        let targets: Vec<SiteSyncTarget> = enabled
                            .into_iter()
                            .map(|(site, c)| SiteSyncTarget {
                                site_id: site.id.clone(),
                                site_name: site.display_name().to_string(),
                                client: c,
                            })
                            .collect();
                        let mut breakers = sync::CircuitBreakerMap::new();
                        let outcome = sync::run_multi_site_sync(
                            &targets,
                            storage,
                            &mut breakers,
                            sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                            None,
                        )
                        .await;
                        let counts = storage.cache_counts().map_err(format_error)?;
                        return Ok(json!({
                            "ok": outcome.succeeded_sites > 0,
                            "summary": outcome.summary,
                            "sites": outcome.per_site,
                            "counts": counts,
                        }));
                    }
                }
            }
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let summary = sync::bootstrap_workspace_for_site(&target_site, &target_client, storage)
                .await
                .map_err(format_error)?;
            let counts = storage.cache_counts().map_err(format_error)?;
            Ok(json!({
                "ok": true,
                "site_id": target_site,
                "summary": summary,
                "counts": counts,
            }))
        }

        "download_attachment" => {
            let issue_key = args.optional_str("issue_key")?;
            let page_id = args.optional_str("page_id")?;
            let attachment_id = args.optional_str("attachment_id")?;
            let dest_dir_opt = args.optional_str("dest_dir")?;

            let (_target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;

            let (owner_label, attachments) = if let Some(key) = issue_key {
                let detail = target_client.get_issue(&key).await.map_err(format_error)?;
                (format!("issue {key}"), detail.attachments)
            } else if let Some(pid) = page_id {
                let detail = target_client.get_page(&pid).await.map_err(format_error)?;
                (format!("page {pid}"), detail.attachments)
            } else {
                return Err("either issue_key or page_id is required".to_string());
            };

            let selected = if let Some(aid) = attachment_id.as_deref() {
                attachments
                    .iter()
                    .find(|a| a.id == aid || a.filename.eq_ignore_ascii_case(aid))
            } else {
                attachments.first()
            }
            .ok_or_else(|| format!("attachment not found on {owner_label}"))?;

            let dest_dir = dest_dir_opt
                .map(PathBuf::from)
                .unwrap_or_else(|| config.resolved_download_dir());
            let downloaded = target_client
                .download_attachment(selected, &dest_dir)
                .await
                .map_err(format_error)?;

            let abs_path = normalize_abs_path(&downloaded.path);
            let is_image =
                img_preview::is_image_attachment(&downloaded.mime_type, &downloaded.filename);

            Ok(json!({
                "ok": true,
                "path": abs_path,
                "file_name": downloaded.filename,
                "mime_type": downloaded.mime_type,
                "size_bytes": downloaded.size,
                "is_image": is_image,
            }))
        }

        _ => Err(format!("unknown tool: {name}")),
    }
}

/// Validates tool arguments before dispatching to Atlassian API or SQLite storage.
pub(crate) fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);
    let _ = args.optional_str("site")?;

    match name {
        "list_sites" | "get_myself" | "sync_workspace" => Ok(()),
        "list_projects" => {
            let _ = args.optional_bool("refresh")?;
            Ok(())
        }
        "list_boards" => {
            let _ = args.optional_str("project")?;
            let _ = args.optional_bool("refresh")?;
            Ok(())
        }
        "list_sprints" => {
            let _ = args.required_i64("board_id")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_board_issues" => {
            let _ = args.required_i64("board_id")?;
            let _ = args.optional_i64("sprint_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "search_jira_issues" => {
            let _ = args.optional_str("jql")?;
            let _ = args.optional_str("query")?;
            let _ = args.optional_str("project")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("page_token")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_jira_issue" | "list_jira_transitions" => {
            let _ = args.required_str("key")?;
            if name == "get_jira_issue" {
                args.validate_detail("key")?;
            }
            Ok(())
        }
        "create_jira_issue" => {
            let _ = args.required_str("project_key")?;
            let _ = args.required_str("summary")?;
            let _ = args.optional_str("issue_type")?;
            let _ = args.optional_text("description_markdown")?;
            let _ = args.optional_str("priority")?;
            let _ = args.optional_str("assignee_id")?;
            let _ = args.optional_str_array("labels")?;
            let _ = args.optional_str("parent_key")?;
            Ok(())
        }
        "update_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.optional_bool("dry_run")?;
            if args.get("replace_section")?.is_some() {
                return Err("section replacement requires a Confluence Cloud ADF page".to_string());
            }
            let summary = args.optional_str("summary")?;
            let (full, append, replacement) = args.body_inputs("description_markdown")?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                None,
            )
            .map_err(|e| e.to_string())?;
            let priority = args.optional_str("priority")?;
            let assignee = args.optional_nullable_str("assignee_id")?;
            let labels = args.optional_str_array_opt("labels")?;
            if summary.is_none()
                && edit.is_none()
                && priority.is_none()
                && assignee.is_none()
                && labels.is_none()
            {
                return Err("at least one Jira issue change is required".to_string());
            }
            Ok(())
        }
        "transition_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("transition")?;
            let _ = args.optional_text("comment_markdown")?;
            Ok(())
        }
        "assign_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.optional_nullable_str("assignee_id")?;
            Ok(())
        }
        "add_jira_comment" => {
            let _ = args.required_str("key")?;
            let _ = args.required_text("body_markdown")?;
            Ok(())
        }
        "upload_jira_attachment" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("path")?;
            Ok(())
        }
        "delete_jira_issue" => {
            let _ = args.required_str("key")?;
            args.require_confirm_true()?;
            Ok(())
        }
        "list_confluence_spaces" => {
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            Ok(())
        }
        "list_space_pages" => {
            let _ = args.required_str("space_key")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_confluence_page" => {
            let _ = args.required_str("page_id")?;
            args.validate_detail("page_id")
        }
        "list_child_pages" => {
            let _ = args.required_str("page_id")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "create_confluence_page" => {
            let _ = args.required_str("space_id_or_key")?;
            let _ = args.required_str("title")?;
            let _ = args.optional_str("parent_id")?;
            let _ = args.optional_text("body_markdown")?;
            Ok(())
        }
        "update_confluence_page" => {
            let _ = args.required_str("page_id")?;
            let _ = args.optional_bool("dry_run")?;
            let title = args.optional_str("title")?;
            let (full, append, replacement) = args.body_inputs("body_markdown")?;
            let section = args.section_replacement()?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                section.as_ref(),
            )
            .map_err(|e| e.to_string())?;
            let version = args.page_version()?;
            let message = args.optional_str("version_message")?;
            if title.is_none() && edit.is_none() && message.is_none() {
                return Err("at least one Confluence page change is required".to_string());
            }
            if edit.is_some() && version.unwrap_or(0) <= 0 {
                return Err("prev_version (>= 1) is required for body edits. Read the current page and use its version_number.".to_string());
            }
            Ok(())
        }
        "add_confluence_comment" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_text("body_markdown")?;
            Ok(())
        }
        "search_confluence_pages" => {
            let q = args.optional_str("query")?;
            let cql = args.optional_str("cql")?;
            let _ = args.optional_bool("remote")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            if q.is_none() && cql.is_none() {
                return Err("either query or cql is required".to_string());
            }
            Ok(())
        }
        "delete_confluence_page" => {
            let _ = args.required_str("page_id")?;
            args.require_confirm_true()?;
            Ok(())
        }
        "search_all" => {
            let _ = args.required_str("query")?;
            if let Some(scope) = args.optional_str("scope")? {
                if !matches!(scope.to_ascii_lowercase().as_str(), "all" | "jira" | "confluence") {
                    return Err(format!(
                        "invalid scope '{scope}' (expected all, jira, or confluence)"
                    ));
                }
            }
            let _ = args.optional_bool("remote")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            Ok(())
        }
        "set_jira_parent" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("parent_key")?;
            Ok(())
        }
        "update_jira_custom_fields" => {
            let _ = args.required_str("key")?;
            let _ = args.custom_fields()?;
            Ok(())
        }
        "create_jira_issue_link" => {
            let _ = args.required_str("inward_key")?;
            let _ = args.required_str("outward_key")?;
            let _ = args.required_str("link_type")?;
            Ok(())
        }
        "move_jira_issues_to_sprint" => {
            let _ = args.required_i64("sprint_id")?;
            let _ = args.required_str_array("issue_keys")?;
            Ok(())
        }
        "add_jira_worklog" => {
            let _ = args.required_str("key")?;
            let _ = args.worklog_seconds()?;
            let _ = args.optional_str("started")?;
            let _ = args.optional_text("comment_markdown")?;
            Ok(())
        }
        "upload_confluence_attachment" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("path")?;
            Ok(())
        }
        "move_confluence_page" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("parent_id")?;
            let _ = args.page_version()?;
            Ok(())
        }
        "add_confluence_labels" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str_array("labels")?;
            Ok(())
        }
        "remove_confluence_label" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("label")?;
            Ok(())
        }
        "download_attachment" => {
            let issue_key = args.optional_str("issue_key")?;
            let page_id = args.optional_str("page_id")?;
            let _ = args.optional_str("attachment_id")?;
            let _ = args.optional_str("dest_dir")?;
            match (issue_key, page_id) {
                (None, None) => Err("either issue_key or page_id is required".to_string()),
                (Some(_), Some(_)) => {
                    Err("specify either issue_key or page_id, not both".to_string())
                }
                _ => Ok(()),
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC framing & argument helpers
// ---------------------------------------------------------------------------

fn parse_message(line: &str) -> Result<Value, Value> {
    let trimmed = line.trim();
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return Err(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {error}"),
            ));
        }
    };

    let Some(object) = value.as_object() else {
        return Err(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected a JSON object",
        ));
    };

    let id = object.get("id").cloned().unwrap_or(Value::Null);

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }

    if !object.get("method").is_some_and(Value::is_string) {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: method must be a string",
        ));
    }

    Ok(value)
}

fn encode_message(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| {
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialization failed\"}}"
            .to_string()
    });
    format!("{body}\n")
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        Err(err) => json!({
            "content": [{ "type": "text", "text": err }],
            "isError": true,
        }),
    }
}

fn format_error(err: anyhow::Error) -> String {
    format!("{err:#}")
}

pub(crate) fn normalize_abs_path(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    });
    let raw = abs.to_string_lossy();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
}

fn validate_range(field: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if value < min || value > max {
        Err(format!("{field} must be between {min} and {max}"))
    } else {
        Ok(value)
    }
}

struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Self {
        Self { value }
    }

    fn get(&self, key: &str) -> Result<Option<&'a Value>, String> {
        let Some(obj) = self.value.as_object() else {
            return Err("arguments must be a JSON object".to_string());
        };
        Ok(obj.get(key))
    }

    fn required_str(&self, key: &str) -> Result<String, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        let raw = val
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string"))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("'{key}' must not be empty"));
        }
        Ok(trimmed.to_string())
    }

    fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("'{key}' must be a string")),
        }
    }

    fn optional_nullable_str(&self, key: &str) -> Result<Option<Option<String>>, String> {
        match self.get(key)? {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(Some(None))
                } else {
                    Ok(Some(Some(trimmed.to_string())))
                }
            }
            Some(_) => Err(format!("'{key}' must be a string or null")),
        }
    }

    fn required_text(&self, key: &str) -> Result<String, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        let raw = val
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string"))?;
        if raw.trim().is_empty() {
            return Err(format!("'{key}' must not be empty"));
        }
        Ok(raw.to_string())
    }

    fn optional_text(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("'{key}' must be a string")),
        }
    }

    fn validate_detail(&self, id: &str) -> Result<(), String> {
        let _ = self.optional_bool("offline")?;
        let _ = self.optional_bool("compact")?;
        let object = self
            .value
            .as_object()
            .ok_or("arguments must be a JSON object")?;
        if let Some(key) = object
            .keys()
            .find(|key| ![id, "site", "offline", "compact"].contains(&key.as_str()))
        {
            return Err(format!("unsupported detail argument '{key}'"));
        }
        Ok(())
    }

    fn body_inputs(
        &self,
        full: &str,
    ) -> Result<(Option<String>, Option<String>, Option<TextReplacement>), String> {
        let replacement = self
            .get("replace_text")?
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|e| format!("invalid replace_text: {e}"))
            })
            .transpose()?;
        Ok((
            self.optional_text(full)?,
            self.optional_text("append_markdown")?,
            replacement,
        ))
    }

    fn section_replacement(&self) -> Result<Option<SectionReplacement>, String> {
        self.get("replace_section")?
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("invalid replace_section: {error}"))
            })
            .transpose()
    }

    fn page_version(&self) -> Result<Option<i32>, String> {
        self.optional_i64("prev_version")?
            .map(|version| {
                i32::try_from(version)
                    .map_err(|_| "prev_version is outside the supported version range".to_string())
            })
            .transpose()
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("'{key}' must be a boolean")),
        }
    }

    fn required_i64(&self, key: &str) -> Result<i64, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        val.as_i64()
            .ok_or_else(|| format!("'{key}' must be an integer"))
    }

    fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .map(Some)
                .ok_or_else(|| format!("'{key}' must be an integer")),
        }
    }

    fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("'{key}' must be a positive integer"))?;
                usize::try_from(n)
                    .map(Some)
                    .map_err(|_| format!("'{key}' is too large"))
            }
        }
    }

    fn optional_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        Ok(self.optional_str_array_opt(key)?.unwrap_or_default())
    }

    fn optional_str_array_opt(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let s = item
                        .as_str()
                        .ok_or_else(|| format!("'{key}' items must be strings"))?;
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("'{key}' must be an array of strings")),
        }
    }

    fn custom_fields(&self) -> Result<&'a Map<String, Value>, String> {
        self.get("fields")?
            .and_then(Value::as_object)
            .filter(|fields| !fields.is_empty())
            .ok_or_else(|| {
                "fields must be a nonempty JSON object keyed by custom field ID".to_string()
            })
    }

    fn worklog_seconds(&self) -> Result<i32, String> {
        i32::try_from(self.required_i64("seconds")?)
            .ok()
            .filter(|&seconds| seconds > 0)
            .ok_or_else(|| "seconds must be a positive 32-bit integer".to_string())
    }

    fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .get(key)?
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| format!("'{key}' must be a nonempty array of strings"))?;
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("'{key}' items must be nonempty strings"))
            })
            .collect()
    }

    fn require_confirm_true(&self) -> Result<(), String> {
        match self.get("confirm")? {
            Some(Value::Bool(true)) => Ok(()),
            _ => Err("destructive operation requires confirm: true".to_string()),
        }
    }
}

fn initialize_result(message: &Value) -> Value {
    let requested = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Atlassian Jira & Confluence tools supporting both Atlassian Cloud and Data Center. Use offline=true for local SQLite cache reads. Destructive operations (delete_jira_issue, delete_confluence_page) require confirm: true."
    })
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

fn workflow_tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

fn tool_catalog() -> Vec<Value> {
    let text_replacement = json!({
        "type": "object",
        "description": "Replace one unique occurrence within a single source text node.",
        "properties": {
            "old_text": { "type": "string", "minLength": 1 },
            "new_text": { "type": "string" }
        },
        "required": ["old_text", "new_text"],
        "additionalProperties": false
    });
    let mut tools = vec![
        // --- Jira ---
        json!({
            "name": "get_myself",
            "description": "Return the currently authenticated Atlassian user profile.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_projects",
            "description": "List accessible Jira projects from SQLite cache or refresh from Jira API.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_boards",
            "description": "List Jira Agile boards, optionally filtered by project key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Optional Jira project key (e.g. ENG)." },
                    "refresh": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_sprints",
            "description": "List sprints for a Jira Agile board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer" },
                    "offline": { "type": "boolean", "default": false }
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_board_issues",
            "description": "Fetch issues on a Jira Agile board (optionally filtered by sprint_id).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer" },
                    "sprint_id": { "type": "integer" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 },
                    "offline": { "type": "boolean", "default": false }
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_jira_issues",
            "description": "Search Jira issues using JQL, plain query text, project filter, or offline SQLite cache.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "jql": { "type": "string" },
                    "query": { "type": "string" },
                    "project": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 },
                    "page_token": { "type": "string" },
                    "offline": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_jira_issue",
            "description": "Get the Jira issue key, summary, status, and full Markdown description. Use compact: false for links, attachments, comments, and the full issue structure.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "offline": { "type": "boolean", "default": false },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_jira_issue",
            "description": "Create a new Jira issue from Markdown description.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_key": { "type": "string", "minLength": 1 },
                    "issue_type": { "type": "string", "default": "Task" },
                    "summary": { "type": "string", "minLength": 1 },
                    "description_markdown": { "type": "string" },
                    "priority": { "type": "string" },
                    "assignee_id": { "type": "string" },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "parent_key": { "type": "string" }
                },
                "required": ["project_key", "summary"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_jira_issue",
            "description": "Update Jira issue fields. Choose one body operation per request. Set dry_run to preview the changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "summary": { "type": "string" },
                    "description_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "priority": { "type": "string" },
                    "assignee_id": { "type": ["string", "null"] },
                    "labels": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_jira_transitions",
            "description": "List available workflow transitions for a Jira issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "transition_jira_issue",
            "description": "Transition a Jira issue to a new workflow status with an optional Markdown comment.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "transition": { "type": "string", "minLength": 1, "description": "Transition ID or target status/transition name." },
                    "comment_markdown": { "type": "string" }
                },
                "required": ["key", "transition"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "assign_jira_issue",
            "description": "Assign a Jira issue to a user (or 'me' for current user, or null to unassign).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "assignee_id": { "type": ["string", "null"] }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_jira_comment",
            "description": "Add a Markdown comment to a Jira issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "body_markdown": { "type": "string", "minLength": 1 }
                },
                "required": ["key", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "upload_jira_attachment",
            "description": "Upload a local file as an attachment on a Jira issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "path": { "type": "string", "minLength": 1 }
                },
                "required": ["key", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_jira_issue",
            "description": "Permanently delete a Jira issue. Requires confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1 },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["key", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("set_jira_parent",
            "Set the parent of an existing Jira Cloud issue. On Data Center, assign an eligible non-subtask issue to an epic.",
            json!({"key": {"type": "string"}, "parent_key": {"type": "string"}}),
            &["key", "parent_key"]),
        workflow_tool("update_jira_custom_fields",
            "Set explicit customfield_<digits> values and return the values read back from Jira.",
            json!({"key": {"type": "string"}, "fields": {"type": "object", "minProperties": 1,
                "propertyNames": {"pattern": "^customfield_[0-9]+$"}}}),
            &["key", "fields"]),
        workflow_tool("create_jira_issue_link",
            "Link two existing issues with a configured Jira link type name and return the link ID.",
            json!({"inward_key": {"type": "string"}, "outward_key": {"type": "string"},
                "link_type": {"type": "string"}}),
            &["inward_key", "outward_key", "link_type"]),
        workflow_tool("move_jira_issues_to_sprint",
            "Move existing issues to an open or active sprint and confirm membership.",
            json!({"sprint_id": {"type": "integer", "minimum": 1}, "issue_keys": {
                "type": "array", "minItems": 1, "maxItems": 50, "items": {"type": "string"}}}),
            &["sprint_id", "issue_keys"]),
        workflow_tool("add_jira_worklog",
            "Log work in positive seconds and retain the remaining time estimate. Start time is RFC 3339 when supplied.",
            json!({"key": {"type": "string"}, "seconds": {"type": "integer", "minimum": 1, "maximum": i32::MAX},
                "started": {"type": "string", "format": "date-time"}, "comment_markdown": {"type": "string"}}),
            &["key", "seconds"]),
        // --- Confluence ---
        json!({
            "name": "list_confluence_spaces",
            "description": "List accessible Confluence spaces from cache or refresh from Confluence API.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "default": false },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_space_pages",
            "description": "List root pages in a Confluence space.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_key": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 },
                    "offline": { "type": "boolean", "default": false }
                },
                "required": ["space_key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_child_pages",
            "description": "List direct child pages of a Confluence page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1 },
                    "offline": { "type": "boolean", "default": false }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_confluence_page",
            "description": "Get page identifiers, version, and full Markdown. Use compact: false for native body, blocks, TOC, comments, and attachments.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1 },
                    "offline": { "type": "boolean", "default": false },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_confluence_page",
            "description": "Create a new Confluence page from Markdown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_id_or_key": { "type": "string", "minLength": 1 },
                    "parent_id": { "type": "string" },
                    "title": { "type": "string", "minLength": 1 },
                    "body_markdown": { "type": "string" }
                },
                "required": ["space_id_or_key", "title"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_confluence_page",
            "description": "Update a Confluence page. Choose one body operation per request and pass its current prev_version. Set dry_run to preview the changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1 },
                    "title": { "type": "string" },
                    "body_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "replace_section": {
                        "type": "object",
                        "description": "Replace content under an exact top-level heading on a Confluence Cloud ADF page.",
                        "properties": {
                            "heading": { "type": "string", "minLength": 1 },
                            "markdown": { "type": "string" }
                        },
                        "required": ["heading", "markdown"],
                        "additionalProperties": false
                    },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "prev_version": { "type": "integer", "minimum": 1 },
                    "version_message": { "type": "string" }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_confluence_comment",
            "description": "Add a footer comment (Markdown) to a Confluence page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1 },
                    "body_markdown": { "type": "string", "minLength": 1 }
                },
                "required": ["page_id", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_confluence_pages",
            "description": "Search Confluence pages locally (FTS5) or remotely via CQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "cql": { "type": "string" },
                    "remote": { "type": "boolean", "default": false },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30 }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_confluence_page",
            "description": "Delete a Confluence page. Requires confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1 },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["page_id", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("upload_confluence_attachment",
            "Create an attachment from a local file using the same upload as the CLI attach command.",
            json!({"page_id": {"type": "string"}, "path": {"type": "string"}}),
            &["page_id", "path"]),
        workflow_tool("move_confluence_page",
            "Move a page under a parent. Data Center requires the current version and a parent in the same space.",
            json!({"page_id": {"type": "string"}, "parent_id": {"type": "string"},
                "prev_version": {"type": "integer", "minimum": 1}}),
            &["page_id", "parent_id"]),
        workflow_tool("add_confluence_labels",
            "Add global labels to an existing page and keep its other labels.",
            json!({"page_id": {"type": "string"}, "labels": {"type": "array", "minItems": 1,
                "items": {"type": "string"}}}),
            &["page_id", "labels"]),
        workflow_tool("remove_confluence_label",
            "Remove one named label from an existing page.",
            json!({"page_id": {"type": "string"}, "label": {"type": "string"}}),
            &["page_id", "label"]),
        // --- Unified & Multi-Site ---
        json!({
            "name": "list_sites",
            "description": "List all registered Atlassian Cloud and Data Center sites with deployment, enabled state, and SQLite cache counts.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_all",
            "description": "Unified search across both Jira issues and Confluence pages (local SQLite FTS5 or remote JQL/CQL).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "enum": ["all", "jira", "confluence"], "default": "all" },
                    "remote": { "type": "boolean", "default": false },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30 }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "sync_workspace",
            "description": "Synchronize Jira projects, boards, sprints, recent issues, and Confluence spaces & pages into the local SQLite cache.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download an attachment from a Jira issue or Confluence page to disk and return its absolute path, metadata, and image flag for direct multimodal agent inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "minLength": 1, "description": "Jira issue key (e.g. ENG-42). Provide either issue_key or page_id." },
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID. Provide either issue_key or page_id." },
                    "attachment_id": { "type": "string", "minLength": 1, "description": "Optional attachment ID or filename to download (defaults to first attachment)." },
                    "dest_dir": { "type": "string", "minLength": 1, "description": "Optional destination directory (defaults to configured download_dir)." }
                },
                "additionalProperties": false
            }
        }),
    ];

    // Add optional `site` selector to every tool except `list_sites`.
    for tool in &mut tools {
        let is_list_sites = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n == "list_sites");
        if !is_list_sites {
            if let Some(props) = tool.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut) {
                props.insert(
                    "site".to_string(),
                    json!({
                        "type": "string",
                        "description": "Optional target Atlassian site ID, name, or URL."
                    }),
                );
            }
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::DeploymentKind,
        models::{IssueDetail, IssueSummary, PageDetail, PageSummary},
    };

    fn test_ctx() -> (AppConfig, AtlassianClient, Storage) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".to_string(),
            deployment: DeploymentKind::Cloud,
            ..AppConfig::default()
        };
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        (cfg, client, storage)
    }

    #[tokio::test]
    async fn bootstrap_is_deferred_until_a_valid_current_site_cache_request() {
        use crate::models::ProjectSummary;

        let (cfg, client, storage) = test_ctx();
        storage
            .upsert_projects_for_site(
                client.site_id(),
                &[ProjectSummary {
                    id: "1".into(),
                    key: "ATL".into(),
                    name: "Atlassian".into(),
                    ..Default::default()
                }],
            )
            .unwrap();

        let mut pending = true;
        for (method, params) in [
            ("initialize", json!({ "protocolVersion": "2025-03-26" })),
            ("tools/list", json!({})),
            ("tools/call", json!({ "name": "search_all" })),
            (
                "tools/call",
                json!({
                    "name": "search_all",
                    "arguments": { "query": "ATL", "site": "another-site" }
                }),
            ),
        ] {
            let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
            assert!(handle_rpc_line_with_bootstrap(
                &cfg,
                &client,
                |_| Ok(&storage),
                &line.to_string(),
                &mut pending,
            )
            .await
            .is_some());
            assert!(pending, "bootstrap was consumed by {line}");
        }
        assert!(!tool_uses_current_site_cache(
            &cfg,
            &client,
            "list_projects",
            &json!({ "refresh": true }),
        ));
        assert!(!tool_uses_current_site_cache(
            &cfg,
            &client,
            "search_confluence_pages",
            &json!({ "cql": "type = page" }),
        ));

        let response = handle_rpc_line_with_bootstrap(
            &cfg,
            &client,
            |_| Ok(&storage),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_projects"}}"#,
            &mut pending,
        )
        .await
        .unwrap();
        assert!(!pending);
        let payload: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["projects"][0]["key"], "ATL");
    }

    #[tokio::test]
    async fn initialize_ping_and_notification_silence() {
        let (cfg, client, storage) = test_ctx();

        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" }
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &init).await.unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "atlassian");
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );

        // Unknown protocol version falls back to latest supported
        let init_unknown = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" }
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &init_unknown)
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);

        // Notifications emit None
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
        .to_string();
        assert!(handle_rpc_line(&cfg, &client, &storage, &notif)
            .await
            .is_none());

        // Request without id emits None
        let no_id = json!({
            "jsonrpc": "2.0",
            "method": "ping"
        })
        .to_string();
        assert!(handle_rpc_line(&cfg, &client, &storage, &no_id)
            .await
            .is_none());

        // Ping with id returns {}
        let ping = json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" }).to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &ping)
            .await
            .unwrap();
        assert_eq!(resp["id"], "p1");
        assert_eq!(resp["result"], json!({}));

        // Parse error and unknown method
        let bad = handle_rpc_line(&cfg, &client, &storage, "{not-json")
            .await
            .unwrap();
        assert_eq!(bad["error"]["code"], PARSE_ERROR);

        let unknown = handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" }).to_string(),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_list_includes_workflows_and_matches_documentation() {
        let (cfg, client, storage) = test_ctx();
        let line = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/list"
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &line)
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let docs = include_str!("../docs/MCP.md");
        let documented: std::collections::BTreeSet<_> = docs
            .lines()
            .filter_map(|line| line.strip_prefix("| `"))
            .filter_map(|line| line.split_once("` |").map(|(name, _)| name))
            .collect();
        let catalog: std::collections::BTreeSet<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        assert_eq!(catalog.len(), tools.len(), "duplicate MCP tool name");
        assert_eq!(catalog.len(), 37);
        assert_eq!(catalog, documented);
        assert!(
            tools.iter().any(|t| t["name"] == "list_sites"),
            "expected list_sites in MCP tool catalog"
        );
        assert!(
            tools.iter().any(|t| t["name"] == "download_attachment"),
            "expected download_attachment in MCP tool catalog"
        );
    }

    #[tokio::test]
    async fn download_attachment_tool_downloads_image_and_returns_structured_metadata() {
        use axum::{routing::get, Json, Router};
        use tokio::net::TcpListener;

        let png_bytes = b"\x89PNG\r\n\x1a\nMOCKPNG".to_vec();
        let png_clone = png_bytes.clone();
        let app = Router::new()
            .route(
                "/rest/api/3/issue/ENG-42",
                get(|| async {
                    Json(json!({
                        "id": "10042",
                        "key": "ENG-42",
                        "fields": {
                            "summary": "Screenshot issue",
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "1", "name": "Task", "subtask": false },
                            "project": { "key": "ENG" },
                            "attachment": [{
                                "id": "att-77",
                                "filename": "architecture.png",
                                "mimeType": "image/png",
                                "size": 15,
                                "created": "2026-11-19T00:00:00Z",
                                "content": "/rest/api/3/attachment/content/att-77"
                            }]
                        }
                    }))
                }),
            )
            .route(
                "/rest/api/3/attachment/content/att-77",
                get(move || {
                    let data = png_clone.clone();
                    async move { data }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base_url = format!("http://{addr}");

        let cfg = AppConfig {
            site_url: base_url.clone(),
            deployment: DeploymentKind::Cloud,
            ..AppConfig::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            crate::auth::StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let dest = tempfile::tempdir().unwrap();

        // Missing both issue_key and page_id fails validation
        assert!(validate_tool_args("download_attachment", &json!({})).is_err());

        let call = json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "tools/call",
            "params": {
                "name": "download_attachment",
                "arguments": {
                    "issue_key": "ENG-42",
                    "attachment_id": "att-77",
                    "dest_dir": dest.path().to_string_lossy()
                }
            }
        })
        .to_string();

        let resp = handle_rpc_line(&cfg, &client, &storage, &call)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["file_name"], "architecture.png");
        assert_eq!(payload["mime_type"], "image/png");
        assert_eq!(payload["size_bytes"], png_bytes.len() as u64);
        assert_eq!(payload["is_image"], true);
        let dl_path = PathBuf::from(payload["path"].as_str().unwrap());
        assert!(dl_path.is_absolute());
        assert_eq!(std::fs::read(&dl_path).unwrap(), png_bytes);
    }

    #[tokio::test]
    async fn destructive_tools_require_confirm_true() {
        let (cfg, client, storage) = test_ctx();

        for (tool, id_key, id_val) in [
            ("delete_jira_issue", "key", "ENG-1"),
            ("delete_confluence_page", "page_id", "9001"),
        ] {
            for bad_confirm in [
                json!({}),
                json!({ "confirm": false }),
                json!({ "confirm": "true" }),
                json!({ "confirm": 1 }),
                json!({ "confirm": null }),
            ] {
                let mut args = bad_confirm;
                args.as_object_mut()
                    .unwrap()
                    .insert(id_key.to_string(), json!(id_val));
                let line = json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "method": "tools/call",
                    "params": {
                        "name": tool,
                        "arguments": args,
                    }
                })
                .to_string();
                let resp = handle_rpc_line(&cfg, &client, &storage, &line)
                    .await
                    .unwrap();
                assert_eq!(resp["result"]["isError"], true, "{tool}: {resp}");
                let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
                assert!(msg.contains("confirm"), "{tool}: {msg}");
            }

            let mut ok_args = json!({ "confirm": true });
            ok_args
                .as_object_mut()
                .unwrap()
                .insert(id_key.to_string(), json!(id_val));
            assert!(validate_tool_args(tool, &ok_args).is_ok(), "{tool}");
        }
    }

    #[tokio::test]
    async fn offline_sqlite_tools_execute_cleanly() {
        let (cfg, client, storage) = test_ctx();
        let issue = IssueSummary {
            id: "1".to_string(),
            key: "ENG-7".to_string(),
            project_key: "ENG".to_string(),
            summary: "Implement offline MCP".to_string(),
            ..Default::default()
        };
        let _ = storage.upsert_issue_detail(&IssueDetail {
            summary: issue,
            description_markdown: "Details".to_string(),
            ..Default::default()
        });
        let page = PageSummary {
            id: "501".to_string(),
            space_id: "sp1".to_string(),
            space_key: "ENG".to_string(),
            title: "Runbook".to_string(),
            version_number: 3,
            ..Default::default()
        };
        let _ = storage.upsert_page_detail(&PageDetail {
            summary: page,
            body_markdown: "# Runbook".to_string(),
            ..Default::default()
        });

        for (tool, args) in [
            ("search_jira_issues", json!({ "offline": true })),
            ("get_jira_issue", json!({ "key": "ENG-7", "offline": true })),
            ("list_space_pages", json!({ "space_key": "ENG", "offline": true })),
            ("get_confluence_page", json!({ "page_id": "501", "offline": true })),
            ("list_child_pages", json!({ "page_id": "501", "offline": true })),
            ("search_confluence_pages", json!({ "query": "Runbook", "remote": false })),
            ("search_all", json!({ "query": "MCP", "scope": "all", "remote": false })),
            ("list_sprints", json!({ "board_id": 1, "offline": true })),
            ("get_board_issues", json!({ "board_id": 1, "offline": true })),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 200,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &client, &storage, &line)
                .await
                .unwrap();
            assert_eq!(resp["result"]["isError"], false, "{tool}: {resp}");
            let payload: Value =
                serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            match tool {
                "get_jira_issue" => assert_eq!(payload["issue"]["key"], "ENG-7"),
                "get_confluence_page" => assert_eq!(payload["page"]["id"], "501"),
                "search_confluence_pages" => {
                    assert_eq!(payload["hits"][0]["id_or_key"], "501")
                }
                "search_all" => assert_eq!(payload["hits"][0]["id_or_key"], "ENG-7"),
                _ => {}
            }
        }
    }
}
