//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0) for Jira & Confluence.
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

mod args;
mod catalog;
mod confluence;
mod jira;
mod workspace;

#[cfg(test)]
mod tests;

use std::{io::Write, path::Path, sync::LazyLock};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    atlassian::{body::TextReplacement, client::error_payload, AtlassianClient, SiteClientPool},
    config::{self, AppConfig},
    storage::Storage,
    sync,
};

#[cfg(test)]
use args::ARGS_READ;
#[cfg(test)]
use catalog::tool_catalog;

pub(crate) use args::validate_tool_args;
use args::Args;
use catalog::{initialize_result, tool_catalog_for};

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


struct McpContext<'a> {
    config: &'a AppConfig,
    client: &'a AtlassianClient,
    storage: &'a Storage,
    args: Args<'a>,
    site_arg: Option<String>,
    scoped_site: Option<String>,
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

    let ctx = McpContext {
        config,
        client,
        storage,
        args,
        site_arg,
        scoped_site,
    };

    match name {
        "list_sites" => workspace::list_sites(&ctx).await,
        "get_myself" => workspace::get_myself(&ctx).await,
        "search_all" => workspace::search_all(&ctx).await,
        "sync_workspace" => workspace::sync_workspace(&ctx).await,
        "download_attachment" => workspace::download_attachment(&ctx).await,
        "list_projects" => jira::list_projects(&ctx).await,
        "list_boards" => jira::list_boards(&ctx).await,
        "list_sprints" => jira::list_sprints(&ctx).await,
        "get_board_issues" => jira::get_board_issues(&ctx).await,
        "search_jira_issues" => jira::search_jira_issues(&ctx).await,
        "get_jira_issue" => jira::get_jira_issue(&ctx).await,
        "create_jira_issue" => jira::create_jira_issue(&ctx).await,
        "update_jira_issue" => jira::update_jira_issue(&ctx).await,
        "list_jira_transitions" => jira::list_jira_transitions(&ctx).await,
        "transition_jira_issue" => jira::transition_jira_issue(&ctx).await,
        "assign_jira_issue" => jira::assign_jira_issue(&ctx).await,
        "add_jira_comment" => jira::add_jira_comment(&ctx).await,
        "set_jira_parent" => jira::set_jira_parent(&ctx).await,
        "update_jira_custom_fields" => jira::update_jira_custom_fields(&ctx).await,
        "create_jira_issue_link" => jira::create_jira_issue_link(&ctx).await,
        "move_jira_issues_to_sprint" => jira::move_jira_issues_to_sprint(&ctx).await,
        "add_jira_worklog" => jira::add_jira_worklog(&ctx).await,
        "upload_jira_attachment" => jira::upload_jira_attachment(&ctx).await,
        "delete_jira_issue" => jira::delete_jira_issue(&ctx).await,
        "list_confluence_spaces" => confluence::list_confluence_spaces(&ctx).await,
        "list_space_pages" => confluence::list_space_pages(&ctx).await,
        "list_child_pages" => confluence::list_child_pages(&ctx).await,
        "get_confluence_page" => confluence::get_confluence_page(&ctx).await,
        "create_confluence_page" => confluence::create_confluence_page(&ctx).await,
        "update_confluence_page" => confluence::update_confluence_page(&ctx).await,
        "add_confluence_comment" => confluence::add_confluence_comment(&ctx).await,
        "search_confluence_pages" => confluence::search_confluence_pages(&ctx).await,
        "upload_confluence_attachment" => confluence::upload_confluence_attachment(&ctx).await,
        "move_confluence_page" => confluence::move_confluence_page(&ctx).await,
        "add_confluence_labels" => confluence::add_confluence_labels(&ctx).await,
        "remove_confluence_label" => confluence::remove_confluence_label(&ctx).await,
        "delete_confluence_page" => confluence::delete_confluence_page(&ctx).await,
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

