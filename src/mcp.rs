//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0) for Jira & Confluence.
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

use std::{
    collections::{BTreeSet, HashMap},
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::Result;
use futures_util::{stream, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    atlassian::{
        body::{BodyEdit, SectionReplacement, TextReplacement},
        client::{error_payload, is_not_found_error},
        AtlassianClient, SiteClientPool,
    },
    config::{self, AppConfig},
    img_preview,
    models::{
        parse_offset_cursor, slice_page, FederatedSearchHit, IssueUpdate, OutgoingIssue,
        OutgoingPage,
    },
    resolver::resolve_rejected_assignee,
    serve::{
        clamp_limit, escape_jql_str, issue_to_hit, looks_like_cql_expr, looks_like_jql_expr,
        page_to_hit,
    },
    storage::Storage,
    sync,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "atlassian";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// Exact set of read-only query tools exposed when `--read-only` is active.
/// Shared by both `tools/list` and `tools/call`.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "get_myself",
    "list_projects",
    "list_boards",
    "list_sprints",
    "get_board_issues",
    "search_jira_issues",
    "get_jira_issue",
    "list_jira_transitions",
    "list_confluence_spaces",
    "list_space_pages",
    "list_child_pages",
    "get_confluence_page",
    "search_confluence_pages",
    "list_sites",
    "search_all",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpOptions {
    pub read_only: bool,
}

impl McpOptions {
    pub const FULL: Self = Self { read_only: false };
    pub const READ_ONLY: Self = Self { read_only: true };

    pub fn allows_tool(self, name: &str) -> bool {
        !self.read_only || READ_ONLY_TOOLS.contains(&name)
    }
}

/// Run the stdio MCP server until stdin reaches EOF.
pub async fn run(config: AppConfig, client: AtlassianClient, storage: Storage) -> Result<()> {
    run_with_options(config, client, storage, McpOptions::FULL).await
}

/// Run the stdio MCP server with explicit options until stdin reaches EOF.
pub async fn run_with_options(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    options: McpOptions,
) -> Result<()> {
    run_with_storage(&config, &client, options, |_| Ok(&storage)).await
}

pub(crate) async fn run_deferred(
    config: AppConfig,
    client: AtlassianClient,
    options: McpOptions,
) -> Result<()> {
    let cache = LazyLock::new(|| Storage::open(&config::cache_db_path(&config.profile)));
    let preview = LazyLock::new(Storage::open_in_memory);
    run_with_storage(&config, &client, options, |dry_run| {
        let storage = if dry_run { &*preview } else { &*cache };
        storage.as_ref().map_err(|error| error.to_string())
    })
    .await
}

async fn run_with_storage<'a>(
    config: &AppConfig,
    client: &AtlassianClient,
    options: McpOptions,
    mut get_storage: impl FnMut(bool) -> Result<&'a Storage, String>,
) -> Result<()> {
    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, mode={}, deployment={}, site={})",
        if options.read_only { "read-only" } else { "full" },
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
            options,
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

/// True when the serving site has a URL and nothing of it is cached yet. The client's own
/// URL counts, not the legacy top-level site_url, which "[[sites]]"-only configs leave empty.
fn needs_bootstrap(client: &AtlassianClient, storage: &Storage) -> bool {
    let site = client.site_id();
    let projects_empty = storage
        .list_projects_for_site(site)
        .unwrap_or_default()
        .is_empty();
    let spaces_empty = storage
        .list_spaces_for_site(site)
        .unwrap_or_default()
        .is_empty();
    projects_empty && spaces_empty && !client.site_url().trim().is_empty()
}

async fn bootstrap_if_empty(_config: &AppConfig, client: &AtlassianClient, storage: &Storage) {
    let site = client.site_id();
    if needs_bootstrap(client, storage) {
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
            .is_ok_and(|scope| scope.is_none_or(|site| site.eq_ignore_ascii_case(client.site_id())))
}

type BodyInputs = (Option<String>, Option<String>, Option<TextReplacement>);

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    config: &AppConfig,
    client: &AtlassianClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    handle_rpc_line_with_options(config, client, storage, line, McpOptions::FULL).await
}

/// Single-line JSON-RPC handler with explicit server options (e.g. read-only mode).
pub async fn handle_rpc_line_with_options(
    config: &AppConfig,
    client: &AtlassianClient,
    storage: &Storage,
    line: &str,
    options: McpOptions,
) -> Option<Value> {
    handle_rpc_line_with_bootstrap(config, client, options, |_| Ok(storage), line, &mut false).await
}

async fn handle_rpc_line_with_bootstrap<'a>(
    config: &AppConfig,
    client: &AtlassianClient,
    options: McpOptions,
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
        "initialize" => Some(success_response(id, initialize_result(&message, options))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(
            id,
            json!({ "tools": tool_catalog_for(options) }),
        )),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = if !options.allows_tool(name) {
                Err(format!("tool '{name}' is not available in --read-only mode"))
            } else {
                match validate_tool_args(name, &arguments).and_then(|()| {
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
                            Box::pin(bootstrap_if_empty(config, client, storage)).await;
                        }
                        Box::pin(call_tool(config, client, storage, name, &arguments)).await
                    }
                    Err(error) => Err(error),
                }
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

/// Site scope of a tool call: Ok(None) means every site. See [AppConfig::resolve_site_scope].
fn resolve_mcp_site_id(
    config: &AppConfig,
    fallback_client: &AtlassianClient,
    raw_site: Option<&str>,
) -> Result<Option<String>, String> {
    config.resolve_site_scope(raw_site, fallback_client.site_id())
}

const OFFLINE_JQL_ERROR: &str = "jql cannot be evaluated offline; pass query (a full-text search of the cache) and/or project, or leave offline unset";
const ASSIGNEE_REQUIRED: &str =
    "assignee_id is required: pass an account id / username, \"me\", or null to unassign";

/// Tools that change Atlassian data; they need one named site, never "all".
const WRITE_TOOLS: &[&str] = &[
    "create_jira_issue",
    "update_jira_issue",
    "transition_jira_issue",
    "assign_jira_issue",
    "add_jira_comment",
    "set_jira_parent",
    "update_jira_custom_fields",
    "create_jira_issue_link",
    "move_jira_issues_to_sprint",
    "add_jira_worklog",
    "upload_jira_attachment",
    "delete_jira_issue",
    "create_confluence_page",
    "update_confluence_page",
    "add_confluence_comment",
    "upload_confluence_attachment",
    "move_confluence_page",
    "add_confluence_labels",
    "remove_confluence_label",
    "delete_confluence_page",
];

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
    if let Ok((mut detail, meta)) = client
        .get_page_with_meta_in_cache(page_id, site_id, storage)
        .await
    {
        let _ = storage.upsert_page_detail_for_site_with_meta(site_id, &mut detail, meta);
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
    let scoped_site = resolve_mcp_site_id(config, client, site_arg.as_deref())?;
    if WRITE_TOOLS.contains(&name) {
        config.reject_all_sites_for_write(site_arg.as_deref())?;
    }

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
            let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_projects_for_site(&target_site)
                .map_err(format_error)?;
            let all = if refresh || cached.is_empty() {
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
            let page = slice_page(all, offset, limit);
            Ok(json!({
                "site_id": target_site,
                "projects": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "list_boards" => {
            let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
            let project = args.optional_str("project")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_boards_for_site(&target_site, project.as_deref())
                .map_err(format_error)?;
            let all = if refresh || cached.is_empty() {
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
            let page = slice_page(all, offset, limit);
            Ok(json!({
                "site_id": target_site,
                "boards": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "list_sprints" => {
            let board_id = args.required_i64("board_id")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let page = if offline {
                let rows = storage
                    .list_sprints_for_site(&target_site, board_id)
                    .map_err(format_error)?;
                slice_page(rows, offset, limit)
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_board_sprints_page(board_id, limit, offset)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_sprints_for_site(&target_site, &fetched.items);
                fetched
            };
            Ok(json!({
                "site_id": target_site,
                "board_id": board_id,
                "sprints": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "get_board_issues" => {
            let board_id = args.required_i64("board_id")?;
            let sprint_id = args.optional_i64("sprint_id")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let cursor = args.optional_str("cursor")?;
            let offset = parse_offset_cursor(cursor.as_deref())?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;

            let page = if offline {
                let rows = storage
                    .list_board_issues_for_site(
                        &target_site,
                        board_id,
                        sprint_id,
                        offset.saturating_add(limit).saturating_add(1),
                    )
                    .map_err(format_error)?;
                slice_page(rows, offset, limit)
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let fetched = target_client
                    .list_board_issues_page(board_id, sprint_id, limit, offset)
                    .await
                    .map_err(format_error)?;
                let _ = storage.record_board_issues_page_for_site(
                    &target_site,
                    board_id,
                    sprint_id,
                    &fetched,
                    cursor.is_none(),
                );
                fetched
            };
            Ok(json!({
                "site_id": target_site,
                "board_id": board_id,
                "sprint_id": sprint_id,
                "issues": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "search_jira_issues" => {
            let jql = args.optional_str("jql")?;
            let query = args.optional_str("query")?;
            let project = args.optional_str("project")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let page_token = args.optional_str("page_token")?;
            let start_at = args.optional_nonneg_usize("start_at")?;
            if page_token.is_some() && start_at.is_some() {
                return Err("specify either 'page_token' or 'start_at', not both".to_string());
            }
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            if offline {
                if let Some(q) = query.as_deref() {
                    // Only the text reaches the full-text index; project narrows its hits, so
                    // ask for the most hits a call allows and cut to limit after filtering.
                    let key_prefix = project
                        .as_deref()
                        .map(|p| format!("{}-", p.to_ascii_lowercase()));
                    let fetch = if key_prefix.is_some() { clamp_limit(usize::MAX) } else { limit };
                    let hits: Vec<FederatedSearchHit> = storage
                        .search_issues_scoped(q, scoped_site.as_deref(), fetch)
                        .map_err(format_error)?
                        .into_iter()
                        .filter(|(_, hit)| {
                            key_prefix.as_deref().is_none_or(|prefix| {
                                hit.id_or_key.to_ascii_lowercase().starts_with(prefix)
                            })
                        })
                        .take(limit)
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
                if jql.is_some() {
                    return Err(OFFLINE_JQL_ERROR.to_string());
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
                if looks_like_jql_expr(&q) {
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
                .search_issues_jql(&effective_jql, limit, page_token.as_deref(), start_at)
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
            let fields = args.optional_str_array("fields")?;
            let hint = "get_jira_issue with fields:[...] or compact:false";
            let format_issue = |detail: &crate::models::IssueDetail| -> Value {
                if !fields.is_empty() {
                    detail.projected_output(&fields)
                } else if compact {
                    detail.compact_output(hint)
                } else {
                    detail.output(false)
                }
            };
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
                    "issue": format_issue(&detail),
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
                "issue": format_issue(&fetched),
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
            let raw_assignee = args.optional_str("assignee_id")?;
            let labels = args.optional_str_array("labels")?;
            let parent_key = args.optional_str("parent_key")?;
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (assignee_id, raw_for_fallback) = match raw_assignee.as_deref() {
                Some(v) if v.eq_ignore_ascii_case("me") => {
                    let me = target_client
                        .get_myself_cached()
                        .await
                        .map_err(format_error)?;
                    (Some(me.id), None)
                }
                Some(v) => (Some(v.to_string()), Some(v.to_string())),
                None => (None, None),
            };
            let mut req = OutgoingIssue {
                project_key,
                issue_type,
                summary,
                description_markdown,
                priority,
                assignee_id,
                labels,
                parent_key,
            };
            let mut resolved_assignee = None;
            let detail = match Box::pin(target_client.create_issue(&req)).await {
                Ok(detail) => detail,
                Err(err) => {
                    if let Some(raw) = raw_for_fallback.as_deref() {
                        match Box::pin(resolve_rejected_assignee(
                            &target_client,
                            &req.project_key,
                            raw,
                            &err,
                            true,
                        ))
                        .await
                        {
                            Some(Ok(user)) => {
                                req.assignee_id = Some(user.id.clone());
                                resolved_assignee = Some(user);
                                Box::pin(target_client.create_issue(&req))
                                    .await
                                    .map_err(format_error)?
                            }
                            Some(Err(ambig)) => return Err(format_error(ambig)),
                            None => return Err(format_error(err)),
                        }
                    } else {
                        return Err(format_error(err));
                    }
                }
            };
            let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
            let mut payload = json!({
                "ok": true,
                "site_id": target_site,
                "issue": if compact { detail.write_output() } else { detail.output(false) },
            });
            if let Some(user) = resolved_assignee {
                payload["resolved_assignee"] = json!({
                    "id": user.id,
                    "display_name": user.display_name,
                });
            }
            Ok(payload)
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
            .map_err(|e| format_error(e.into()))?;
            let priority = args.optional_str("priority")?;
            let raw_assignee = args.optional_nullable_str("assignee_id")?;
            let labels = args.optional_str_array_opt("labels")?;
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (assignee_id, raw_for_fallback) = match raw_assignee {
                Some(Some(v)) if v.eq_ignore_ascii_case("me") => {
                    let me = target_client
                        .get_myself_cached()
                        .await
                        .map_err(format_error)?;
                    (Some(Some(me.id)), None)
                }
                Some(Some(v)) => (Some(Some(v.clone())), Some(v)),
                other => (other, None),
            };
            let mut patch = IssueUpdate {
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
            let mut resolved_assignee = None;
            let detail = match Box::pin(target_client.edit_issue(&key, &patch, edit)).await {
                Ok(detail) => detail,
                Err(err) => {
                    if let Some(raw) = raw_for_fallback.as_deref() {
                        match Box::pin(resolve_rejected_assignee(
                            &target_client,
                            &key,
                            raw,
                            &err,
                            true,
                        ))
                        .await
                        {
                            Some(Ok(user)) => {
                                patch.assignee_id = Some(Some(user.id.clone()));
                                resolved_assignee = Some(user);
                                Box::pin(target_client.edit_issue(&key, &patch, edit))
                                    .await
                                    .map_err(format_error)?
                            }
                            Some(Err(ambig)) => return Err(format_error(ambig)),
                            None => return Err(format_error(err)),
                        }
                    } else {
                        return Err(format_error(err));
                    }
                }
            };
            let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
            let mut payload = json!({
                "ok": true,
                "site_id": target_site,
                "issue": if compact { detail.write_output() } else { detail.output(false) },
            });
            if let Some(user) = resolved_assignee {
                payload["resolved_assignee"] = json!({
                    "id": user.id,
                    "display_name": user.display_name,
                });
            }
            Ok(payload)
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
            let executed = target_client
                .transition_issue(&key, &transition, comment.as_deref())
                .await
                .map_err(format_error)?;
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            let transition_id = executed
                .as_ref()
                .map(|t| t.id.as_str())
                .unwrap_or(transition.as_str());
            let mut payload = json!({
                "ok": true,
                "site_id": target_site,
                "key": key,
                "transition": transition,
                "transition_id": transition_id,
            });
            if let Some(tr) = executed {
                payload["to_status"] = json!(tr.to_status.name);
            }
            Ok(payload)
        }

        "assign_jira_issue" => {
            let key = args.required_str("key")?;
            // Absent is an error, not "unassign": only an explicit null unassigns.
            let raw_assignee = args
                .optional_nullable_str("assignee_id")?
                .ok_or_else(|| ASSIGNEE_REQUIRED.to_string())?;
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (mut resolved, raw_for_fallback) = match raw_assignee.as_deref() {
                None => (None, None),
                Some(v) if v.eq_ignore_ascii_case("me") => {
                    let me = target_client
                        .get_myself_cached()
                        .await
                        .map_err(format_error)?;
                    (Some(me.id), None)
                }
                Some(v) => (Some(v.to_string()), Some(v.to_string())),
            };
            let mut resolved_assignee = None;
            if let Err(err) = Box::pin(target_client.assign_issue(&key, resolved.as_deref())).await
            {
                if let Some(raw) = raw_for_fallback.as_deref() {
                    match Box::pin(resolve_rejected_assignee(
                        &target_client,
                        &key,
                        raw,
                        &err,
                        false,
                    ))
                    .await
                    {
                        Some(Ok(user)) => {
                            resolved = Some(user.id.clone());
                            resolved_assignee = Some(user);
                            Box::pin(target_client.assign_issue(&key, resolved.as_deref()))
                                .await
                                .map_err(format_error)?;
                        }
                        Some(Err(ambig)) => return Err(format_error(ambig)),
                        None => return Err(format_error(err)),
                    }
                } else {
                    return Err(format_error(err));
                }
            }
            refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            let mut payload = json!({
                "ok": true,
                "site_id": target_site,
                "key": key,
                "assignee_id": resolved,
            });
            if let Some(user) = resolved_assignee {
                payload["resolved_assignee"] = json!({
                    "id": user.id,
                    "display_name": user.display_name,
                });
            }
            Ok(payload)
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
            if storage
                .get_issue_detail_for_site(&target_site, &key)
                .ok()
                .flatten()
                .is_some()
            {
                let _ = storage.append_issue_comment_for_site(&target_site, &key, &comment);
            } else {
                refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
            }
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
            Box::pin(async {
                tokio::join!(
                    refresh_mcp_issue_cache(&client, storage, &site, &inward),
                    refresh_mcp_issue_cache(&client, storage, &site, &outward),
                );
            })
            .await;
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
            Box::pin(
                stream::iter(&confirmed)
                    .map(|key| refresh_mcp_issue_cache(&client, storage, &site, key))
                    .buffer_unordered(4)
                    .for_each(|()| async {}),
            )
            .await;
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
            let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let cached = storage
                .list_spaces_for_site(&target_site)
                .map_err(format_error)?;
            let all = if refresh || cached.is_empty() {
                if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
                    cached
                } else {
                    let (target_site, target_client) =
                        resolve_mcp_client(config, client, site_arg.as_deref())?;
                    let fetched = target_client
                        .list_all_spaces()
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
            let page = slice_page(all, offset, limit);
            Ok(json!({
                "site_id": target_site,
                "spaces": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "list_space_pages" => {
            let space_key = args.required_str("space_key")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
            let cursor = args.optional_str("cursor")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let page = if offline {
                let offset = parse_offset_cursor(cursor.as_deref())?;
                let rows = storage
                    .list_space_root_pages_for_site(&target_site, &space_key)
                    .map_err(format_error)?;
                slice_page(rows, offset, limit)
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let mut fetched = target_client
                    .list_space_root_pages_page(&space_key, limit, cursor.as_deref())
                    .await
                    .map_err(format_error)?;
                let _ = storage.record_root_pages_page_for_site(
                    &target_site,
                    &space_key,
                    &fetched,
                    cursor.is_none(),
                );
                let _ = storage.overlay_children_state_for_site(&target_site, &mut fetched.items);
                fetched
            };
            Ok(json!({
                "site_id": target_site,
                "space_key": space_key,
                "pages": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
        }

        "list_child_pages" => {
            let page_id = args.required_str("page_id")?;
            let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(100));
            let cursor = args.optional_str("cursor")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
            let page = if offline {
                let offset = parse_offset_cursor(cursor.as_deref())?;
                let rows = storage
                    .list_child_pages_for_site(&target_site, &page_id)
                    .map_err(format_error)?;
                slice_page(rows, offset, limit)
            } else {
                let (target_site, target_client) =
                    resolve_mcp_client(config, client, site_arg.as_deref())?;
                let mut fetched = target_client
                    .list_child_pages_page(&page_id, limit, cursor.as_deref())
                    .await
                    .map_err(format_error)?;
                let _ = storage.record_child_pages_page_for_site(
                    &target_site,
                    &page_id,
                    &fetched,
                    cursor.is_none(),
                );
                let _ = storage.overlay_children_state_for_site(&target_site, &mut fetched.items);
                fetched
            };
            Ok(json!({
                "site_id": target_site,
                "parent_id": page_id,
                "pages": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))
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
                    "page": if compact {
                        detail.compact_output("get_confluence_page with compact:false")
                    } else {
                        detail.output(false)
                    },
                }));
            }
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let (mut fetched, meta) = match Box::pin(
                target_client.get_page_with_meta_in_cache(&page_id, &target_site, storage),
            )
            .await
            {
                Ok(res) => res,
                Err(err) => {
                    if is_not_found_error(&err) {
                        let _ = storage.delete_page_for_site(&target_site, &page_id);
                    }
                    return Err(format_error(err));
                }
            };
            let _ = storage.upsert_page_detail_for_site_with_meta(
                &target_site,
                &mut fetched,
                meta,
            );
            Ok(json!({
                "site_id": target_site,
                "detail_stale": false,
                "page": if compact {
                    fetched.compact_output("get_confluence_page with compact:false")
                } else {
                    fetched.output(false)
                },
            }))
        }

        "create_confluence_page" => {
            let space_id_or_key = args.required_str("space_id_or_key")?;
            let parent_id = args.optional_str("parent_id")?;
            let title = args.required_str("title")?;
            let body_markdown = args.optional_text("body_markdown")?.unwrap_or_default();
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let req = OutgoingPage {
                space_id_or_key,
                parent_id,
                title,
                body_markdown,
            };
            let detail = Box::pin(target_client.create_page(&req))
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
            Ok(json!({
                "ok": true,
                "site_id": target_site,
                "page": if compact { detail.write_output() } else { detail.output(false) },
            }))
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
            .map_err(|e| format_error(e.into()))?;
            let prev_version = args.page_version()?;
            let version_message = args.optional_str("version_message")?;
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            if args.optional_bool("dry_run")?.unwrap_or(false) {
                let preview = Box::pin(target_client.preview_page_edit(
                    &page_id,
                    title,
                    edit,
                    prev_version,
                    version_message,
                ))
                .await
                .map_err(format_error)?;
                return Ok(
                    json!({ "ok": true, "site_id": target_site, "dry_run": true, "preview": preview }),
                );
            }
            let detail = Box::pin(target_client.edit_page(
                &page_id,
                title,
                edit,
                prev_version,
                version_message,
            ))
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
            Ok(json!({
                "ok": true,
                "site_id": target_site,
                "page": if compact { detail.write_output() } else { detail.output(false) },
            }))
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
            if storage
                .get_page_detail_for_site(&target_site, &page_id)
                .ok()
                .flatten()
                .is_some()
            {
                let _ = storage.append_page_comment_for_site(&target_site, &page_id, &comment);
            } else {
                refresh_mcp_page_cache(&target_client, storage, &target_site, &page_id).await;
            }
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
                        if looks_like_cql_expr(q) {
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
            if !attachments.is_empty()
                && storage
                    .get_page_detail_for_site(&site, &page_id)
                    .ok()
                    .flatten()
                    .is_some()
            {
                let _ = storage.append_page_attachments_for_site(&site, &page_id, &attachments);
            } else {
                refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
            }
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
                let want_jira = scope == "all" || scope == "jira";
                let want_conf = scope == "all" || scope == "confluence";
                let (jira_res, conf_res) = Box::pin(async {
                    let jira_fut = async {
                        if !want_jira {
                            return Ok(None);
                        }
                        let jql = if looks_like_jql_expr(&query) {
                            query.clone()
                        } else {
                            format!("text ~ \"{}\"", escape_jql_str(&query))
                        };
                        target_client
                            .search_issues_jql(&jql, limit, None, None)
                            .await
                            .map(Some)
                            .map_err(format_error)
                    };
                    let conf_fut = async {
                        if !want_conf {
                            return Ok(None);
                        }
                        let cql = if looks_like_cql_expr(&query) {
                            query.clone()
                        } else {
                            format!("text ~ \"{}\"", escape_jql_str(&query))
                        };
                        target_client
                            .search_confluence_cql(&cql, limit)
                            .await
                            .map(Some)
                            .map_err(format_error)
                    };
                    tokio::join!(jira_fut, conf_fut)
                })
                .await;
                if let Some(page) = jira_res? {
                    let _ = storage.upsert_issues_for_site(&target_site, &page.issues);
                    combined.extend(page.issues.iter().map(|i| {
                        FederatedSearchHit::new(
                            target_site.clone(),
                            target_name.clone(),
                            issue_to_hit(i),
                        )
                    }));
                }
                if let Some(pages) = conf_res? {
                    let _ = storage.upsert_pages_for_site(&target_site, &pages);
                    combined.extend(pages.iter().map(|p| {
                        FederatedSearchHit::new(
                            target_site.clone(),
                            target_name.clone(),
                            page_to_hit(p),
                        )
                    }));
                }
                crate::storage::sort_federated_hits(&mut combined);
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
            // No site (and no --site) or "site=all": every enabled site; a named site: that one.
            if scoped_site.is_none() {
                if let Ok(pool) = SiteClientPool::from_config(config) {
                    let targets = sync::sync_targets(&pool);
                    if targets.len() > 1 {
                        let mut breakers = sync::CircuitBreakerMap::new();
                        let outcome = Box::pin(sync::run_multi_site_sync(
                            &targets,
                            storage,
                            &mut breakers,
                            sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                            None,
                        ))
                        .await;
                        let counts = storage.cache_counts().map_err(format_error)?;
                        let payload = json!({
                            "ok": outcome.succeeded_sites > 0,
                            "summary": outcome.summary,
                            "sites": outcome.per_site,
                            "counts": counts,
                        });
                        if outcome.total_sites > 0 && outcome.succeeded_sites == 0 {
                            let text = serde_json::to_string_pretty(&payload)
                                .unwrap_or_else(|_| payload.to_string());
                            return Err(text);
                        }
                        return Ok(payload);
                    }
                }
            }
            let (target_site, target_client) =
                resolve_mcp_client(config, client, scoped_site.as_deref())?;
            let summary = Box::pin(sync::bootstrap_workspace_for_site(
                &target_site,
                &target_client,
                storage,
            ))
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
                let atts = target_client
                    .list_issue_attachments(&key)
                    .await
                    .map_err(format_error)?;
                (format!("issue {key}"), atts)
            } else if let Some(pid) = page_id {
                let atts = target_client
                    .list_page_attachments(&pid)
                    .await
                    .map_err(format_error)?;
                (format!("page {pid}"), atts)
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

/// Argument names a tool accepts: the properties of its own input schema in [tool_catalog],
/// the one place they are declared. None for a tool the catalog does not list.
fn allowed_tool_keys(name: &str) -> Option<&'static BTreeSet<String>> {
    static ALLOWED: LazyLock<HashMap<String, BTreeSet<String>>> = LazyLock::new(|| {
        tool_catalog()
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let properties = tool.pointer("/inputSchema/properties")?.as_object()?;
                Some((name, properties.keys().cloned().collect()))
            })
            .collect()
    });
    ALLOWED.get(name)
}

/// Validates tool arguments before dispatching to Atlassian API or SQLite storage.
pub(crate) fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);
    let _ = args.optional_str("site")?;
    // Every schema says additionalProperties: false: reject an undeclared (often misspelled)
    // argument instead of dropping it silently.
    if let (Some(allowed), Some(given)) = (allowed_tool_keys(name), arguments.as_object()) {
        if let Some(key) = given.keys().find(|key| !allowed.contains(key.as_str())) {
            let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
            return Err(format!(
                "unknown argument '{key}' for {name}; allowed: {}",
                allowed.join(", ")
            ));
        }
    }

    match name {
        "list_sites" | "get_myself" | "sync_workspace" => Ok(()),
        "list_projects" => {
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_boards" => {
            let _ = args.optional_str("project")?;
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_sprints" => {
            let _ = args.required_i64("board_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_board_issues" => {
            let _ = args.required_i64("board_id")?;
            let _ = args.optional_i64("sprint_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
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
            let page_token = args.optional_str("page_token")?;
            let start_at = args.optional_nonneg_usize("start_at")?;
            if page_token.is_some() && start_at.is_some() {
                return Err("specify either 'page_token' or 'start_at', not both".to_string());
            }
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_jira_issue" | "list_jira_transitions" => {
            let _ = args.required_str("key")?;
            if name == "get_jira_issue" {
                args.validate_detail()?;
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
            let _ = args.optional_bool("compact")?;
            Ok(())
        }
        "update_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.optional_bool("dry_run")?;
            let _ = args.optional_bool("compact")?;
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
            if args.optional_nullable_str("assignee_id")?.is_none() {
                return Err(ASSIGNEE_REQUIRED.to_string());
            }
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
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_space_pages" => {
            let _ = args.required_str("space_key")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_confluence_page" => {
            let _ = args.required_str("page_id")?;
            args.validate_detail()
        }
        "list_child_pages" => {
            let _ = args.required_str("page_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "create_confluence_page" => {
            let _ = args.required_str("space_id_or_key")?;
            let _ = args.required_str("title")?;
            let _ = args.optional_str("parent_id")?;
            let _ = args.optional_text("body_markdown")?;
            let _ = args.optional_bool("compact")?;
            Ok(())
        }
        "update_confluence_page" => {
            let _ = args.required_str("page_id")?;
            let _ = args.optional_bool("dry_run")?;
            let _ = args.optional_bool("compact")?;
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
        Err(err) => {
            let is_structured_error = serde_json::from_str::<Value>(&err)
                .ok()
                .and_then(|v| {
                    v.as_object()
                        .and_then(|o| o.get("ok"))
                        .and_then(Value::as_bool)
                })
                == Some(false);
            let text = if is_structured_error {
                err
            } else {
                let payload = json!({
                    "ok": false,
                    "error": {
                        "code": "error",
                        "message": err,
                    },
                });
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
            };
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": true,
            })
        }
    }
}

fn format_error(err: anyhow::Error) -> String {
    let payload = json!({
        "ok": false,
        "error": error_payload(&err),
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
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

#[cfg(test)]
thread_local! {
    /// Argument names read through [Args::get] while a test records them (Some).
    static ARGS_READ: std::cell::RefCell<Option<BTreeSet<String>>> =
        const { std::cell::RefCell::new(None) };
}

struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Self {
        Self { value }
    }

    fn get(&self, key: &str) -> Result<Option<&'a Value>, String> {
        #[cfg(test)]
        ARGS_READ.with(|read| {
            if let Some(keys) = read.borrow_mut().as_mut() {
                keys.insert(key.to_string());
            }
        });
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

    fn validate_detail(&self) -> Result<(), String> {
        let _ = self.optional_bool("offline")?;
        let _ = self.optional_bool("compact")?;
        Ok(())
    }

    fn body_inputs(&self, full: &str) -> Result<BodyInputs, String> {
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

    fn optional_nonneg_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("'{key}' must be a non-negative integer"))?;
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

fn initialize_result(message: &Value, options: McpOptions) -> Value {
    let requested = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let instructions = if options.read_only {
        "Atlassian Jira & Confluence read-only tools supporting both Atlassian Cloud and Data Center. Use offline=true for local SQLite cache reads. Write, sync, and attachment download tools are disabled in --read-only mode."
    } else {
        "Atlassian Jira & Confluence tools supporting both Atlassian Cloud and Data Center. Use offline=true for local SQLite cache reads. Destructive operations (delete_jira_issue, delete_confluence_page) require confirm: true."
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": instructions
    })
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

fn tool_catalog_for(options: McpOptions) -> Vec<Value> {
    let mut tools = tool_catalog();
    if options.read_only {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| options.allows_tool(name))
        });
    }
    tools
}

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

/// Cost note for the catalog tools whose refresh re-downloads every page of the catalog.
fn catalog_refresh_cost(noun: &str) -> String {
    format!("refresh: true re-downloads the full catalog (one request per page of results; hundreds of {noun} means several requests and seconds of latency). Default reads the local cache with no network call; only an empty cache is fetched.")
}

fn tool_catalog() -> Vec<Value> {
    let refresh_property = json!({
        "type": "boolean",
        "default": false,
        "description": "Re-download the full catalog, one request per page of results. Default false reads the local cache."
    });
    let text_replacement = json!({
        "type": "object",
        "description": "Replace one unique occurrence within a single source text node.",
        "properties": {
            "old_text": { "type": "string", "minLength": 1, "description": "Exact unique text snippet to replace within one text node." },
            "new_text": { "type": "string", "description": "Replacement text that substitutes for old_text." }
        },
        "required": ["old_text", "new_text"],
        "additionalProperties": false
    });
    let offline_property = json!({
        "type": "boolean",
        "default": false,
        "description": "Read from local SQLite cache only; default false calls the live API."
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
            "description": format!("List accessible Jira projects from SQLite cache or refresh from Jira API. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("projects")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of projects to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_boards",
            "description": format!("List Jira Agile boards, optionally filtered by project key. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("boards")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Optional Jira project key (e.g. ENG)." },
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of boards to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_sprints",
            "description": "List sprints for a Jira Agile board. Returns active and future sprints only. Pass next_cursor as cursor to continue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer", "description": "Numeric Jira Agile board ID from list_boards." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of sprints to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." },
                    "offline": offline_property.clone()
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_board_issues",
            "description": "Fetch issues on a Jira Agile board (optionally filtered by sprint_id). A live call returns at most 100 issues; pass next_cursor as cursor to continue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer", "description": "Numeric Jira Agile board ID from list_boards." },
                    "sprint_id": { "type": "integer", "description": "Optional numeric sprint ID from list_sprints to filter issues." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of issues to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." },
                    "offline": offline_property.clone()
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_jira_issues",
            "description": "Search Jira-only issues using explicit JQL (jql), plain text (query), project filter, or offline SQLite cache. Use search_all for cross-product Jira + Confluence search; pass jql explicitly for custom fields (e.g. `customField = \"foo\"`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "jql": { "type": "string", "description": "Explicit raw JQL expression (e.g. `customField = \"value\" ORDER BY updated DESC`). Takes precedence over query." },
                    "query": { "type": "string", "description": "Plain text search query (or compatible standard-field JQL expression)." },
                    "project": { "type": "string", "description": "Optional Jira project key filter (e.g. ENG)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of issues to return (1–1000, default 50)." },
                    "page_token": { "type": "string", "description": "Cloud pagination cursor (`next_page_token`). Mutually exclusive with `start_at`." },
                    "start_at": { "type": "integer", "minimum": 0, "description": "Data Center 0-based pagination offset (`next_start_at`). Mutually exclusive with `page_token`." },
                    "offline": { "type": "boolean", "default": false, "description": "Search local SQLite cache only (plain query/project); default false calls Jira." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_jira_issue",
            "description": "Get the Jira issue key, summary, status, and full Markdown description. The omitted object lists skipped counts, standard fields, and populated custom fields (null means count unknown). Pass fields: [...] to fetch only specific omitted or custom fields by key (by field name, customfield_<id>, or custom field display name), or compact: false for the full ticket.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to fetch (e.g. ENG-42)." },
                    "offline": offline_property.clone(),
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Optional list of omitted standard fields, customfield_<id> keys, or custom field display names to project without loading the full ticket."
                    }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_jira_issue",
            "description": "Create a new Jira issue from a Markdown description and return the created issue key and summary (set compact: false for full details).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_key": { "type": "string", "minLength": 1, "description": "Target Jira project key (e.g. ENG)." },
                    "issue_type": { "type": "string", "default": "Task", "description": "Jira issue type name, such as Task, Bug, or Story (default Task)." },
                    "summary": { "type": "string", "minLength": 1, "description": "Concise single-line Jira issue title." },
                    "description_markdown": { "type": "string", "description": "Optional issue description formatted in Markdown." },
                    "priority": { "type": "string", "description": "Optional Jira priority name (e.g. High, Medium, Low)." },
                    "assignee_id": { "type": "string", "description": "Account ID or 'me'; a display name or email is resolved when the ID is rejected." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Optional list of Jira label strings to attach." },
                    "parent_key": { "type": "string", "description": "Optional parent or epic issue key (e.g. ENG-10)." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["project_key", "summary"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_jira_issue",
            "description": "Update standard Jira issue fields and Markdown description; use update_jira_custom_fields for customfield_<digits> keys. Choose one body operation per request; set dry_run: true to preview changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to update (e.g. ENG-42)." },
                    "summary": { "type": "string", "description": "New single-line title to replace the issue summary." },
                    "description_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "priority": { "type": "string", "description": "New Jira priority name (e.g. High, Medium, Low)." },
                    "assignee_id": { "type": ["string", "null"], "description": "Account ID or 'me'; a display name or email is resolved when the ID is rejected." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Replaces all labels; pass [] to clear." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
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
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to inspect transitions for (e.g. ENG-42)." }
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
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to transition (e.g. ENG-42)." },
                    "transition": { "type": "string", "minLength": 1, "description": "Transition ID or target status/transition name." },
                    "comment_markdown": { "type": "string", "description": "Optional Markdown comment posted alongside the workflow transition." }
                },
                "required": ["key", "transition"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "assign_jira_issue",
            "description": "Assign a Jira issue. assignee_id is required: an account id (Cloud) or username (Data Center), \"me\", or null to unassign.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to assign or unassign (e.g. ENG-42)." },
                    "assignee_id": { "type": ["string", "null"], "description": "Account ID (Cloud), username (Data Center), or 'me'; a display name or email is resolved when the ID is rejected. null unassigns." }
                },
                "required": ["key", "assignee_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_jira_comment",
            "description": "Add a Markdown comment to a Jira issue and return the created comment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to comment on (e.g. ENG-42)." },
                    "body_markdown": { "type": "string", "minLength": 1, "description": "Comment content formatted in Markdown." }
                },
                "required": ["key", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "upload_jira_attachment",
            "description": "Upload a local file as an attachment on a Jira issue and return the created attachment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to attach the file to (e.g. ENG-42)." },
                    "path": { "type": "string", "minLength": 1, "description": "Local filesystem path of the file to upload." }
                },
                "required": ["key", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_jira_issue",
            "description": "Permanently delete a Jira issue and remove it from the local cache. Destructive: requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to permanently delete (e.g. ENG-42)." },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["key", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("set_jira_parent",
            "Set the parent of an existing Jira Cloud issue. On Data Center, assign an eligible non-subtask issue to an epic.",
            json!({
                "key": {"type": "string", "description": "Child Jira issue key to reparent (e.g. ENG-42)."},
                "parent_key": {"type": "string", "description": "Target parent or epic issue key (e.g. ENG-10)."}
            }),
            &["key", "parent_key"]),
        workflow_tool("update_jira_custom_fields",
            "Set explicit customfield_<digits> values on a Jira issue and return the read-back values. Use update_jira_issue for standard fields or description edits.",
            json!({
                "key": {"type": "string", "description": "Jira issue key to update (e.g. ENG-42)."},
                "fields": {
                    "type": "object",
                    "minProperties": 1,
                    "propertyNames": {"pattern": "^customfield_[0-9]+$"},
                    "description": "Non-empty map of customfield_<digits> keys to native JSON values."
                }
            }),
            &["key", "fields"]),
        workflow_tool("create_jira_issue_link",
            "Link two existing issues with a configured Jira link type name and return the link ID.",
            json!({
                "inward_key": {"type": "string", "description": "Source issue key on inward end (for \"Blocks\", the blocking issue)."},
                "outward_key": {"type": "string", "description": "Target issue key on outward end (for \"Blocks\", the blocked issue)."},
                "link_type": {"type": "string", "description": "Configured Jira link type name (e.g. \"Blocks\", \"Relates\", \"Duplicate\")."}
            }),
            &["inward_key", "outward_key", "link_type"]),
        workflow_tool("move_jira_issues_to_sprint",
            "Move existing issues to an open or active sprint and confirm membership.",
            json!({
                "sprint_id": {"type": "integer", "minimum": 1, "description": "Numeric target open or active sprint ID from list_sprints."},
                "issue_keys": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 50,
                    "items": {"type": "string"},
                    "description": "Non-empty list of 1–50 Jira issue keys to move."
                }
            }),
            &["sprint_id", "issue_keys"]),
        workflow_tool("add_jira_worklog",
            "Log work in positive seconds and retain the remaining time estimate. Start time is RFC 3339 when supplied.",
            json!({
                "key": {"type": "string", "description": "Jira issue key to log work against (e.g. ENG-42)."},
                "seconds": {"type": "integer", "minimum": 1, "maximum": i32::MAX, "description": "Positive integer duration spent in seconds (e.g. 1800 for 30m)."},
                "started": {"type": "string", "format": "date-time", "description": "Optional worklog start timestamp in RFC 3339 format."},
                "comment_markdown": {"type": "string", "description": "Optional Markdown description of the logged work."}
            }),
            &["key", "seconds"]),
        // --- Confluence ---
        json!({
            "name": "list_confluence_spaces",
            "description": format!("List accessible Confluence spaces from cache or refresh from Confluence API. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("spaces")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of spaces to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_space_pages",
            "description": "List top-level root pages in a Confluence space (use list_child_pages for direct children of a page). A live call returns at most 250 pages; pass next_cursor as cursor to continue. has_children is null when unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_key": { "type": "string", "minLength": 1, "description": "Confluence space key from list_confluence_spaces (e.g. ENG)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of pages to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Value from next_cursor. Valid only with the same arguments." },
                    "offline": offline_property.clone()
                },
                "required": ["space_key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_child_pages",
            "description": "List direct child pages of a Confluence page (use list_space_pages for top-level space roots). A live call returns at most 250 pages; pass next_cursor as cursor to continue. has_children is null when unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Parent Confluence page ID whose direct children to list." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100, "description": "Maximum number of child pages to return (1–1000, default 100)." },
                    "cursor": { "type": "string", "description": "Value from next_cursor. Valid only with the same arguments." },
                    "offline": offline_property.clone()
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_confluence_page",
            "description": "Get page identifiers, version, and full Markdown. Use compact: false for native body, blocks, TOC, comments, and attachments. The omitted object lists what was left out; null means the count is unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to fetch." },
                    "offline": offline_property.clone(),
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_confluence_page",
            "description": "Create a new Confluence page from Markdown and return the created page ID, version, and title (set compact: false for full details).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_id_or_key": { "type": "string", "minLength": 1, "description": "Target Confluence space ID or space key (e.g. ENG)." },
                    "parent_id": { "type": "string", "description": "Optional parent Confluence page ID to nest under." },
                    "title": { "type": "string", "minLength": 1, "description": "Unique page title within the target Confluence space." },
                    "body_markdown": { "type": "string", "description": "Optional initial page body formatted in Markdown." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
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
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to update." },
                    "title": { "type": "string", "description": "Optional new title to rename the Confluence page." },
                    "body_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "replace_section": {
                        "type": "object",
                        "description": "Replace content under an exact top-level or layout-column heading on a Confluence Cloud ADF page.",
                        "properties": {
                            "heading": { "type": "string", "minLength": 1, "description": "Exact heading text identifying the section to replace." },
                            "markdown": { "type": "string", "description": "Replacement Markdown body for the matched section." }
                        },
                        "required": ["heading", "markdown"],
                        "additionalProperties": false
                    },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "prev_version": { "type": "integer", "minimum": 1, "description": "Current integer version_number from get_confluence_page (required for body edits)." },
                    "version_message": { "type": "string", "description": "Optional short summary recorded in page version history." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_confluence_comment",
            "description": "Add a Markdown footer comment to a Confluence page and return the created comment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to comment on." },
                    "body_markdown": { "type": "string", "minLength": 1, "description": "Footer comment content formatted in Markdown." }
                },
                "required": ["page_id", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_confluence_pages",
            "description": "Search Confluence-only pages locally (SQLite FTS5) or remotely via CQL. Use search_all for cross-product Jira + Confluence search; pass cql explicitly for custom expressions (e.g. `customMeta = \"value\"`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Plain text search query (or compatible standard-field CQL expression)." },
                    "cql": { "type": "string", "description": "Explicit raw CQL expression (e.g. `space = ENG AND title ~ \"Spec\"`). Takes precedence over query." },
                    "remote": { "type": "boolean", "default": false, "description": "Search live Confluence API via CQL; default false searches local cache." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30, "description": "Maximum number of pages to return (1–1000, default 30)." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_confluence_page",
            "description": "Delete a Confluence page and remove it from the local cache. Destructive: requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to permanently delete." },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["page_id", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("upload_confluence_attachment",
            "Upload a local file as an attachment on a Confluence page and return the created attachment metadata.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to attach the file to."},
                "path": {"type": "string", "description": "Local filesystem path of the file to upload."}
            }),
            &["page_id", "path"]),
        workflow_tool("move_confluence_page",
            "Move a page under a parent. Data Center requires the current version and a parent in the same space.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to move."},
                "parent_id": {"type": "string", "description": "Destination parent Confluence page ID."},
                "prev_version": {"type": "integer", "minimum": 1, "description": "Current integer version_number from get_confluence_page (required on Data Center)."}
            }),
            &["page_id", "parent_id"]),
        workflow_tool("add_confluence_labels",
            "Add global labels to an existing page and keep its other labels.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to add labels to."},
                "labels": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": "string"},
                    "description": "Non-empty list of global label names to add."
                }
            }),
            &["page_id", "labels"]),
        workflow_tool("remove_confluence_label",
            "Remove one named label from an existing page.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to remove the label from."},
                "label": {"type": "string", "description": "Exact Confluence label name to remove."}
            }),
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
            "description": "Unified search across Jira issues and Confluence pages (local SQLite FTS5 or remote JQL/CQL). Prefer search_jira_issues for Jira-only JQL/pagination or search_confluence_pages for Confluence-only CQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "description": "Search query text (or JQL/CQL expression when remote is true)." },
                    "scope": { "type": "string", "enum": ["all", "jira", "confluence"], "default": "all", "description": "Products to search: all (default), jira, or confluence." },
                    "remote": { "type": "boolean", "default": false, "description": "Query live Jira/Confluence APIs; default false searches local SQLite cache." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30, "description": "Maximum number of combined hits to return (1–1000, default 30)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "sync_workspace",
            "description": "Synchronize Jira projects, boards, sprints, recent issues, and Confluence spaces & pages into the local SQLite cache. Cost: walks every project, board, sprint, your open issues and every Confluence space (paginated) for each enabled site (or only the site named by site) — tens to hundreds of API requests and typically 10–120 s on large Data Center instances. Call once per session or when the cache is known stale; prefer per-item tools or refresh: true on a single list.",
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

    // Every tool accepts the optional `site` selector; `list_sites` validates it but always
    // lists every site.
    for tool in &mut tools {
        let is_list_sites = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n == "list_sites");
        let description = if is_list_sites {
            "Accepted for a uniform call shape and checked against the configured sites; every site is listed regardless."
        } else {
            "Optional target Atlassian site ID, name, or URL."
        };
        if let Some(props) = tool.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut) {
            props.insert(
                "site".to_string(),
                json!({ "type": "string", "description": description }),
            );
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

    fn site_scope_config() -> AppConfig {
        let site = |id: &str| crate::config::SiteProfile {
            id: id.into(),
            name: id.into(),
            site_url: format!("https://{id}.atlassian.net"),
            account_id: id.into(),
            ..Default::default()
        };
        AppConfig {
            sites: vec![site("acme"), site("beta")],
            ..AppConfig::default()
        }
    }

    #[test]
    fn mcp_site_resolution_falls_back_to_active_site() {
        let mut cfg = site_scope_config();
        let client = AtlassianClient::new(cfg.clone(), AuthManager::new("scope-test")).unwrap();
        assert_eq!(resolve_mcp_site_id(&cfg, &client, None).unwrap(), None);
        cfg.active_site = Some("beta".into());
        assert_eq!(resolve_mcp_site_id(&cfg, &client, None).unwrap().as_deref(), Some("beta"));
        // A per-call site still wins.
        assert_eq!(resolve_mcp_site_id(&cfg, &client, Some("ACME")).unwrap().as_deref(), Some("acme"));
    }

    #[test]
    fn mcp_unknown_site_is_an_error() {
        let cfg = site_scope_config();
        let client = AtlassianClient::new(cfg.clone(), AuthManager::new("scope-test")).unwrap();
        let err = resolve_mcp_site_id(&cfg, &client, Some("acmee")).unwrap_err();
        assert!(err.contains("unknown site 'acmee'") && err.contains("acme, beta"), "{err}");
        // The legacy "default" id is always accepted.
        assert_eq!(resolve_mcp_site_id(&cfg, &client, Some("default")).unwrap().as_deref(), Some("default"));
    }

    #[test]
    fn mcp_bootstrap_runs_for_site_without_legacy_site_url() {
        let cfg = site_scope_config();
        assert!(cfg.site_url.is_empty(), "a [[sites]]-only config has no legacy site_url");
        let pool = crate::atlassian::SiteClientPool::from_config(&cfg).unwrap();
        let client = pool.client_for("acme").unwrap();
        let storage = Storage::open_in_memory().unwrap();
        assert!(needs_bootstrap(&client, &storage));
        storage
            .upsert_spaces_for_site(
                "acme",
                &[crate::models::SpaceSummary {
                    id: "1".into(),
                    key: "ENG".into(),
                    name: "Eng".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        assert!(!needs_bootstrap(&client, &storage), "a cached site is not bootstrapped again");
    }

    #[tokio::test]
    async fn assign_requires_explicit_assignee_id() {
        let (cfg, client, storage) = test_ctx();
        let missing = json!({ "key": "ENG-1" });
        assert_eq!(validate_tool_args("assign_jira_issue", &missing).unwrap_err(), ASSIGNEE_REQUIRED);

        // The call fails before storage or the network is touched.
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "assign_jira_issue", "arguments": missing }
        })
        .to_string();
        let resp = handle_rpc_line_with_bootstrap(
            &cfg,
            &client,
            McpOptions::FULL,
            |_| panic!("storage must not be opened for an invalid assign"),
            &line,
            &mut true,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true, "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("assignee_id is required"), "{text}");

        // The handler refuses it as well, for a caller that skips validation.
        let err = Box::pin(call_tool(&cfg, &client, &storage, "assign_jira_issue", &missing))
            .await
            .unwrap_err();
        assert_eq!(err, ASSIGNEE_REQUIRED);

        let schema = tool_catalog()
            .into_iter()
            .find(|tool| tool["name"] == "assign_jira_issue")
            .unwrap();
        assert_eq!(schema["inputSchema"]["required"], json!(["key", "assignee_id"]));
        assert!(schema["description"].as_str().unwrap().contains("null to unassign"));
    }

    #[tokio::test]
    async fn assign_null_unassigns() {
        use axum::{http::StatusCode, routing::put, Json, Router};
        use std::sync::{Arc, Mutex};
        use tokio::net::TcpListener;

        let unassign = json!({ "key": "ENG-1", "assignee_id": null });
        assert_eq!(validate_tool_args("assign_jira_issue", &unassign), Ok(()));

        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorder = Arc::clone(&bodies);
        let app = Router::new().route(
            "/rest/api/3/issue/ENG-1/assignee",
            put(move |Json(body): Json<Value>| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().unwrap().push(body);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
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

        let line = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "assign_jira_issue", "arguments": unassign }
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &line).await.unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["assignee_id"], Value::Null);
        assert_eq!(*bodies.lock().unwrap(), vec![json!({ "accountId": null })]);
    }

    #[test]
    fn assign_misspelled_key_is_rejected() {
        let err = validate_tool_args(
            "assign_jira_issue",
            &json!({ "key": "ENG-1", "assignee": "alice" }),
        )
        .unwrap_err();
        assert!(
            err.starts_with("unknown argument 'assignee' for assign_jira_issue; allowed: "),
            "{err}"
        );
        assert!(err.contains("assignee_id") && err.contains("site"), "{err}");
    }

    /// A value one schema property accepts, for driving validators and handlers.
    fn sample_argument(key: &str, schema: &Value) -> Value {
        if let Some(value) = schema.get("const") {
            return value.clone();
        }
        if let Some(first) = schema.pointer("/enum/0") {
            return first.clone();
        }
        match key {
            "fields" if schema.get("type").and_then(Value::as_str) == Some("object") => {
                return json!({ "customfield_10000": 1 })
            }
            "fields" => return json!(["comments"]),
            "replace_text" => return json!({ "old_text": "a", "new_text": "b" }),
            "replace_section" => return json!({ "heading": "h", "markdown": "m" }),
            _ => {}
        }
        let kind = match &schema["type"] {
            Value::Array(kinds) => kinds.first().and_then(Value::as_str),
            other => other.as_str(),
        };
        match kind {
            Some("integer") => json!(1),
            Some("boolean") => json!(false),
            Some("array") => json!(["ENG-1"]),
            _ => json!("ENG-1"),
        }
    }

    #[tokio::test]
    async fn every_tool_schema_declares_every_argument_its_validator_reads() {
        // No site URL: a handler that reaches for the network fails at once, after it has read
        // its arguments.
        let cfg = AppConfig::default();
        let client = AtlassianClient::new(cfg.clone(), AuthManager::new("schema-audit")).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let catalog = tool_catalog();
        assert_eq!(catalog.len(), 37);
        for tool in &catalog {
            let name = tool["name"].as_str().unwrap();
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            let declared: BTreeSet<String> = properties.keys().cloned().collect();
            assert!(declared.contains("site"), "{name} does not declare site");
            assert_eq!(allowed_tool_keys(name), Some(&declared), "{name}");

            // The smallest valid call: required arguments plus what a few validators insist on.
            let mut base = match name {
                "update_jira_issue" => json!({ "summary": "s" }),
                "update_confluence_page" => json!({ "title": "t" }),
                "search_confluence_pages" => json!({ "query": "q" }),
                "download_attachment" => json!({ "issue_key": "ENG-1" }),
                _ => json!({}),
            };
            for key in tool["inputSchema"]["required"].as_array().into_iter().flatten() {
                let key = key.as_str().unwrap();
                assert!(declared.contains(key), "{name} requires undeclared {key}");
                base[key] = sample_argument(key, &properties[key]);
            }

            // Every argument the validator or the handler reads must be declared, or a valid
            // call would be rejected as unknown.
            ARGS_READ.with(|read| *read.borrow_mut() = Some(BTreeSet::new()));
            let validated = validate_tool_args(name, &base);
            let _ = Box::pin(call_tool(&cfg, &client, &storage, name, &base)).await;
            let read = ARGS_READ.with(|read| read.borrow_mut().take()).unwrap();
            assert_eq!(validated, Ok(()), "{name}: {base}");
            let undeclared: Vec<&String> = read.difference(&declared).collect();
            assert!(undeclared.is_empty(), "{name} reads undeclared arguments {undeclared:?}");

            for key in &declared {
                let mut args = base.clone();
                args[key.as_str()] = sample_argument(key, &properties[key.as_str()]);
                if let Err(err) = validate_tool_args(name, &args) {
                    assert!(!err.contains("unknown argument"), "{name}.{key}: {err}");
                }
            }
            let mut extra = base.clone();
            extra["not_an_argument"] = json!(true);
            let err = validate_tool_args(name, &extra).unwrap_err();
            assert!(
                err.starts_with(&format!("unknown argument 'not_an_argument' for {name}; allowed: ")),
                "{err}"
            );
        }
    }

    #[test]
    fn tool_descriptions_carry_cost_hints() {
        let catalog = tool_catalog();
        let sync = catalog.iter().find(|tool| tool["name"] == "sync_workspace").unwrap();
        assert!(sync["description"].as_str().unwrap().contains("Cost:"));
        let mut with_refresh = Vec::new();
        for tool in &catalog {
            let name = tool["name"].as_str().unwrap();
            if let Some(refresh) = tool.pointer("/inputSchema/properties/refresh") {
                with_refresh.push(name);
                assert!(
                    refresh["description"].as_str().is_some_and(|d| !d.trim().is_empty()),
                    "{name}: refresh has no description"
                );
                assert!(
                    tool["description"]
                        .as_str()
                        .unwrap()
                        .contains("refresh: true re-downloads the full catalog"),
                    "{name}"
                );
            }
        }
        assert_eq!(with_refresh, ["list_projects", "list_boards", "list_confluence_spaces"]);
    }

    #[tokio::test]
    async fn every_tool_property_in_tools_list_has_non_empty_description() {
        fn assert_properties_described(path: &str, schema: &Value) {
            let Some(props) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (prop_name, prop_schema) in props {
                let prop_path = format!("{path}.{prop_name}");
                let desc = prop_schema
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                assert!(!desc.is_empty(), "{prop_path} is missing a non-empty description");
                assert_properties_described(&prop_path, prop_schema);
            }
        }

        let (cfg, client, storage) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/list"
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &client, &storage, &req)
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 37);

        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            let tool_desc = tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            assert!(!tool_desc.is_empty(), "{name} is missing a tool description");
            assert_properties_described(name, &tool["inputSchema"]);
        }
    }

    #[tokio::test]
    async fn mcp_offline_search_filters_hits_by_project() {
        let (cfg, client, storage) = test_ctx();
        // Forty ENG hits are newer than the single OPS hit, so a project filter applied after
        // the limit would find nothing.
        let mut issues: Vec<IssueSummary> = (1..=40)
            .map(|i| IssueSummary {
                id: format!("e{i}"),
                key: format!("ENG-{i}"),
                project_key: "ENG".into(),
                summary: format!("Login timeout {i}"),
                updated_at: format!("2026-11-20T10:00:{i:02}Z"),
                ..Default::default()
            })
            .collect();
        issues.push(IssueSummary {
            id: "o1".into(),
            key: "OPS-1".into(),
            project_key: "OPS".into(),
            summary: "Login timeout on the gateway".into(),
            updated_at: "2026-11-01T00:00:00Z".into(),
            ..Default::default()
        });
        storage.upsert_issues_for_site(client.site_id(), &issues).unwrap();

        let search = |arguments: Value| {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": { "name": "search_jira_issues", "arguments": arguments }
            })
            .to_string();
            let (cfg, client, storage) = (&cfg, &client, &storage);
            async move {
                let resp = handle_rpc_line(cfg, client, storage, &line).await.unwrap();
                let text = resp["result"]["content"][0]["text"].as_str().unwrap().to_string();
                (resp["result"]["isError"] == true, serde_json::from_str::<Value>(&text).unwrap())
            }
        };
        let hit_keys = |payload: &Value| -> Vec<String> {
            payload["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["id_or_key"].as_str().unwrap().to_string())
                .collect()
        };

        let (is_error, payload) =
            search(json!({ "offline": true, "query": "timeout", "project": "ops", "limit": 5 })).await;
        assert!(!is_error, "{payload}");
        assert_eq!(hit_keys(&payload), ["OPS-1"]);

        let (_, payload) = search(json!({ "offline": true, "query": "timeout", "limit": 5 })).await;
        assert_eq!(hit_keys(&payload).len(), 5);

        // Only the text reaches the index; jql next to a query is not searched as text.
        let (is_error, payload) =
            search(json!({ "offline": true, "query": "gateway", "jql": "project = ENG" })).await;
        assert!(!is_error, "{payload}");
        assert_eq!(payload["query"], "gateway");
        assert_eq!(hit_keys(&payload), ["OPS-1"]);

        let (is_error, payload) = search(json!({ "offline": true, "jql": "project = ENG" })).await;
        assert!(is_error);
        assert_eq!(payload["error"]["message"], OFFLINE_JQL_ERROR);
    }

    #[test]
    fn mcp_site_all_means_federated() {
        let mut cfg = site_scope_config();
        cfg.active_site = Some("beta".into());
        let client = AtlassianClient::new(cfg.clone(), AuthManager::new("scope-test")).unwrap();
        assert_eq!(resolve_mcp_site_id(&cfg, &client, Some("all")).unwrap(), None);
        assert!(cfg.reject_all_sites_for_write(Some("all")).is_err());
        assert!(cfg.reject_all_sites_for_write(Some("acme")).is_ok());
        assert!(WRITE_TOOLS.contains(&"create_jira_issue") && !WRITE_TOOLS.contains(&"search_all"));
    }


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
                McpOptions::FULL,
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
            McpOptions::FULL,
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

    #[tokio::test]
    async fn get_jira_issue_surfaces_custom_fields_and_projects_omitted_fields_by_key() {
        let (cfg, client, storage) = test_ctx();
        let mut custom_fields = std::collections::BTreeMap::new();
        custom_fields.insert(
            "customfield_10042".to_string(),
            crate::models::IssueCustomField {
                name: Some("Acceptance Criteria".to_string()),
                value: json!("All smoke tests pass with zero 5xx"),
                markdown: None,
            },
        );
        storage
            .upsert_issue_detail(&IssueDetail {
                summary: IssueSummary {
                    id: "42".to_string(),
                    key: "ENG-42".to_string(),
                    project_key: "ENG".to_string(),
                    summary: "Verify AC custom field".to_string(),
                    comment_count: 1,
                    ..Default::default()
                },
                description_markdown: "Base description without AC".to_string(),
                comments: vec![crate::models::IssueComment {
                    id: "c1".to_string(),
                    body_markdown: "Confirmed".to_string(),
                    ..Default::default()
                }],
                custom_fields,
                ..Default::default()
            })
            .unwrap();

        // 1. Default compact read surfaces populated custom field name + ID in omitted.custom_fields
        let compact_req = json!({
            "jsonrpc": "2.0",
            "id": 201,
            "method": "tools/call",
            "params": {
                "name": "get_jira_issue",
                "arguments": { "key": "ENG-42", "offline": true }
            }
        })
        .to_string();
        let compact_resp = handle_rpc_line(&cfg, &client, &storage, &compact_req)
            .await
            .unwrap();
        assert_eq!(compact_resp["result"]["isError"], false);
        let compact_payload: Value = serde_json::from_str(
            compact_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            compact_payload["issue"]["omitted"]["custom_fields"],
            json!([{ "id": "customfield_10042", "name": "Acceptance Criteria" }])
        );

        // 2. Selective fields projection fetches ONLY requested fields without pulling full ticket
        let proj_req = json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "tools/call",
            "params": {
                "name": "get_jira_issue",
                "arguments": {
                    "key": "ENG-42",
                    "offline": true,
                    "fields": ["Acceptance Criteria", "comments", "missing_field"]
                }
            }
        })
        .to_string();
        let proj_resp = handle_rpc_line(&cfg, &client, &storage, &proj_req)
            .await
            .unwrap();
        assert_eq!(proj_resp["result"]["isError"], false);
        let proj_payload: Value = serde_json::from_str(
            proj_resp["result"]["content"][0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(proj_payload["issue"]["key"], "ENG-42");
        assert!(proj_payload["issue"].get("description_markdown").is_none());
        assert_eq!(
            proj_payload["issue"]["fields"]["customfield_10042"]["value"],
            "All smoke tests pass with zero 5xx"
        );
        assert_eq!(
            proj_payload["issue"]["fields"]["comments"][0]["body_markdown"],
            "Confirmed"
        );
        assert_eq!(
            proj_payload["issue"]["missing_fields"],
            json!(["missing_field"])
        );
    }

    #[tokio::test]
    async fn read_only_mode_exposes_exact_15_tools_and_blocks_all_other_tools_before_storage() {
        let (cfg, client, storage) = test_ctx();
        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        })
        .to_string();

        let full_resp = handle_rpc_line_with_options(
            &cfg,
            &client,
            &storage,
            &list_req,
            McpOptions::FULL,
        )
        .await
        .unwrap();
        let ro_resp = handle_rpc_line_with_options(
            &cfg,
            &client,
            &storage,
            &list_req,
            McpOptions::READ_ONLY,
        )
        .await
        .unwrap();

        let full_names: std::collections::BTreeSet<String> = full_resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        let ro_names: std::collections::BTreeSet<String> = ro_resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        let expected_ro: std::collections::BTreeSet<String> = READ_ONLY_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect();

        assert_eq!(full_names.len(), 37);
        assert_eq!(ro_names.len(), 15);
        assert_eq!(ro_names, expected_ro);

        // Every excluded tool (all 22) must be rejected before storage or network access.
        let excluded: Vec<String> = full_names.difference(&ro_names).cloned().collect();
        assert_eq!(excluded.len(), 22);
        for tool in &excluded {
            let arguments = match tool.as_str() {
                "update_jira_issue" => json!({ "key": "ENG-1", "summary": "x", "dry_run": true }),
                "update_confluence_page" => json!({ "page_id": "501", "title": "x", "dry_run": true }),
                "download_attachment" => json!({ "issue_key": "ENG-1", "dest_dir": "C:/nonexistent" }),
                _ => json!({}),
            };
            let call = json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            })
            .to_string();
            let mut pending = true;
            let resp = handle_rpc_line_with_bootstrap(
                &cfg,
                &client,
                McpOptions::READ_ONLY,
                |_| panic!("storage must not be accessed for blocked tool {tool}"),
                &call,
                &mut pending,
            )
            .await
            .unwrap();
            assert!(pending, "bootstrap must not run for blocked tool {tool}");
            assert_eq!(resp["result"]["isError"], true, "{tool}: {resp}");
            let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                msg.contains("--read-only"),
                "expected read-only rejection for {tool}, got: {msg}"
            );
        }

        // Unavailable/unknown tool in read-only mode is rejected without touching storage;
        // full mode keeps reporting "unknown tool".
        let unknown_call = json!({
            "jsonrpc": "2.0",
            "id": 43,
            "method": "tools/call",
            "params": { "name": "nonexistent_tool", "arguments": {} }
        })
        .to_string();
        let mut pending = true;
        let unknown_ro = handle_rpc_line_with_bootstrap(
            &cfg,
            &client,
            McpOptions::READ_ONLY,
            |_| panic!("storage must not be accessed for unavailable tool"),
            &unknown_call,
            &mut pending,
        )
        .await
        .unwrap();
        assert_eq!(unknown_ro["result"]["isError"], true);
        assert!(unknown_ro["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("--read-only"));

        let unknown_full = handle_rpc_line_with_options(
            &cfg,
            &client,
            &storage,
            &unknown_call,
            McpOptions::FULL,
        )
        .await
        .unwrap();
        assert_eq!(unknown_full["result"]["isError"], true);
        assert!(unknown_full["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));

        // Allowed read-only tools execute normally.
        let list_sites_call = json!({
            "jsonrpc": "2.0",
            "id": 44,
            "method": "tools/call",
            "params": { "name": "list_sites", "arguments": {} }
        })
        .to_string();
        let list_sites_resp = handle_rpc_line_with_options(
            &cfg,
            &client,
            &storage,
            &list_sites_call,
            McpOptions::READ_ONLY,
        )
        .await
        .unwrap();
        assert_eq!(list_sites_resp["result"]["isError"], false, "{list_sites_resp}");
    }
}
