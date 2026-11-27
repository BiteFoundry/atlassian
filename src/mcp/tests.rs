use std::{collections::BTreeSet, path::PathBuf};

use super::{args::allowed_tool_keys, *};
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
    let docs = include_str!("../../docs/MCP.md");
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
