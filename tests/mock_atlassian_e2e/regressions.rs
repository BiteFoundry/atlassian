use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
};

use axum::{
    extract::Query,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{AppConfig, DeploymentKind},
    mcp,
    models::StatusCategory,
    serve,
    storage::Storage,
};

use crate::server::{ensure_isolated_env, spawn_server};

#[tokio::test]
async fn regression_atlassian_bug_01_cloud_footer_comment_endpoint_and_controls() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug01MockState {
        cloud_post_hits: Vec<Value>,
        cloud_wrong_post_hits: usize,
        cloud_get_hits: usize,
        cloud_comments: Vec<Value>,
        dc_post_hits: Vec<Value>,
    }

    let st = Arc::new(Mutex::new(Bug01MockState {
        cloud_comments: vec![json!({
            "id": "cm-existing",
            "version": { "authorId": "acc-alice", "createdAt": "2026-11-15T12:00:00Z" },
            "body": {
                "atlas_doc_format": {
                    "representation": "atlas_doc_format",
                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Existing comment\"}]}]}"
                }
            }
        })],
        ..Default::default()
    }));

    let router = Router::new()
        .route(
            "/wiki/api/v2/pages/200",
            get(|| async {
                Json(json!({
                    "id": "200",
                    "spaceId": "5001",
                    "spaceKey": "ENG",
                    "title": "Page 200",
                    "status": "current",
                    "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z", "authorId": "acc-alice" },
                    "body": {
                        "atlas_doc_format": {
                            "representation": "atlas_doc_format",
                            "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Body\"}]}]}"
                        }
                    },
                    "_links": { "webui": "/spaces/ENG/pages/200" }
                }))
            }),
        )
        .route(
            "/wiki/api/v2/pages/200/labels",
            get(|| async { Json(json!({ "results": [] })) }),
        )
        .route(
            "/wiki/api/v2/pages/200/attachments",
            get(|| async { Json(json!({ "results": [] })) }),
        )
        .route(
            "/wiki/api/v2/pages/200/footer-comments",
            get({
                let st = st.clone();
                move || {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.cloud_get_hits += 1;
                        Json(json!({ "results": g.cloud_comments.clone() }))
                    }
                }
            })
            .post({
                let st = st.clone();
                move || {
                    let st = st.clone();
                    async move {
                        st.lock().unwrap().cloud_wrong_post_hits += 1;
                        (
                            StatusCode::METHOD_NOT_ALLOWED,
                            Json(json!({ "message": "POST not supported on /pages/{id}/footer-comments" })),
                        )
                    }
                }
            }),
        )
        .route(
            "/wiki/api/v2/footer-comments",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.cloud_post_hits.push(body.clone());
                        let cid = format!("cm-cloud-{}", g.cloud_post_hits.len());
                        let comment = json!({
                            "id": cid,
                            "pageId": body.get("pageId").cloned().unwrap_or(Value::Null),
                            "version": {
                                "number": 1,
                                "authorId": "acc-alice",
                                "createdAt": "2026-11-15T12:10:00Z"
                            },
                            "body": {
                                "atlas_doc_format": body.get("body").cloned().unwrap_or(Value::Null)
                            }
                        });
                        g.cloud_comments.push(comment.clone());
                        (StatusCode::CREATED, Json(comment))
                    }
                }
            }),
        )
        .route(
            "/rest/api/content",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.dc_post_hits.push(body.clone());
                        let cid = format!("cm-dc-{}", g.dc_post_hits.len());
                        Json(json!({
                            "id": cid,
                            "type": "comment",
                            "version": {
                                "number": 1,
                                "when": "2026-11-15T12:20:00Z",
                                "by": { "displayName": "Alice DC" }
                            },
                            "body": body.get("body").cloned().unwrap_or(Value::Null)
                        }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let cloud_auth = AuthManager::with_credentials(
        "bug01-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth)
            .expect("cloud client");
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().expect("storage");

    // 1. CLI entry point
    let profile = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_atlassian");
    let cli_out = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "cloud-tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["wiki", "comment", "200", "CLI comment body", "--json"])
        .output()
        .await
        .expect("run CLI wiki comment");
    assert!(
        cli_out.status.success(),
        "CLI wiki comment failed: {}",
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let cli_json: Value = serde_json::from_slice(&cli_out.stdout).expect("CLI JSON");
    assert_eq!(cli_json["comment"]["id"], "cm-cloud-1");

    // 2. MCP entry point
    let mcp_resp = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "add_confluence_comment",
                "arguments": { "page_id": "200", "body_markdown": "MCP comment body" }
            }
        })
        .to_string(),
    )
    .await
    .expect("mcp rpc");
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    let mcp_payload: Value =
        serde_json::from_str(mcp_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_payload["comment"]["id"], "cm-cloud-2");

    // 3. Local HTTP entry point
    let api_router = serve::build_router(cfg.clone(), cloud_client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();
    let http_resp = http
        .post(format!("{api_base}/pages/200/comments"))
        .json(&json!({ "body_markdown": "HTTP comment body" }))
        .send()
        .await
        .expect("http post comment");
    assert_eq!(http_resp.status(), StatusCode::OK);
    let http_json: Value = http_resp.json().await.expect("http json");
    assert_eq!(http_json["comment"]["id"], "cm-cloud-3");

    // Verify all 3 Cloud POST requests hit /wiki/api/v2/footer-comments with top-level string pageId="200"
    {
        let g = st.lock().unwrap();
        assert_eq!(
            g.cloud_wrong_post_hits, 0,
            "must not POST to /wiki/api/v2/pages/200/footer-comments"
        );
        assert_eq!(g.cloud_post_hits.len(), 3);
        let expected_texts = ["CLI comment body", "MCP comment body", "HTTP comment body"];
        for (req, expected_text) in g.cloud_post_hits.iter().zip(expected_texts.iter()) {
            assert_eq!(req["pageId"], json!("200"), "payload: {req}");
            assert_eq!(req["body"]["representation"], "atlas_doc_format");
            let adf_str = req["body"]["value"].as_str().expect("stringified ADF");
            let adf: Value = serde_json::from_str(adf_str).expect("valid ADF JSON");
            assert!(
                adf.to_string().contains(expected_text),
                "expected '{expected_text}' in ADF: {adf}"
            );
        }
    }

    // 4. Regression control: GET /wiki/api/v2/pages/200/footer-comments still works
    let listed = cloud_client
        .list_page_comments("200")
        .await
        .expect("list_page_comments");
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0].id, "cm-existing");
    assert_eq!(listed[3].id, "cm-cloud-3");
    assert!(st.lock().unwrap().cloud_get_hits >= 1);

    // 5. Regression control: Data Center add_page_comment still uses POST /rest/api/content
    let dc_auth = AuthManager::with_credentials(
        "bug01-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .expect("dc client");
    let dc_comment = dc_client
        .add_page_comment("200", "DC comment body")
        .await
        .expect("dc add_page_comment");
    assert_eq!(dc_comment.id, "cm-dc-1");
    let dc_hits = &st.lock().unwrap().dc_post_hits;
    assert_eq!(dc_hits.len(), 1);
    assert_eq!(dc_hits[0]["type"], "comment");
    assert_eq!(dc_hits[0]["container"]["id"], "200");
    assert_eq!(dc_hits[0]["container"]["type"], "page");
    assert_eq!(dc_hits[0]["body"]["storage"]["representation"], "storage");
    assert!(dc_hits[0]["body"]["storage"]["value"]
        .as_str()
        .unwrap()
        .contains("DC comment body"));
}

#[tokio::test]
async fn regression_atlassian_bug_04_plain_text_vs_explicit_jql_cql() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug04State {
        jql_sent: Vec<String>,
        cql_sent: Vec<String>,
    }

    let st = Arc::new(Mutex::new(Bug04State::default()));
    let router = Router::new()
        .route(
            "/rest/api/3/search/jql",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let jql = body["jql"].as_str().unwrap_or("").to_string();
                        st.lock().unwrap().jql_sent.push(jql);
                        Json(json!({ "issues": [], "total": 0 }))
                    }
                }
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get({
                let st = st.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let st = st.clone();
                    async move {
                        let cql = q.get("cql").cloned().unwrap_or_default();
                        st.lock().unwrap().cql_sent.push(cql);
                        Json(json!({ "results": [] }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let auth = AuthManager::with_credentials(
        "bug04-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("client");
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().expect("storage");
    let api_router = serve::build_router(cfg.clone(), client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();
    let profile = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_atlassian");

    let plain_inputs = ["read and write", "alpha or beta", "timeout=30", "~user"];

    // 1. MCP search_jira_issues with project="ENG" -> all 4 plain-text queries stay inside text ~ "..." AND keep project = "ENG"
    for q in plain_inputs {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "search_jira_issues",
                    "arguments": { "query": q, "project": "ENG" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let last_jql = st.lock().unwrap().jql_sent.last().unwrap().clone();
        assert!(
            last_jql.contains("project = \"ENG\"") && last_jql.contains(&format!("text ~ \"{q}\"")),
            "MCP Jira query '{q}' must wrap text and keep project=ENG; got: {last_jql}"
        );
    }

    // 2. MCP search_confluence_pages & HTTP /search?remote=true -> all 4 plain-text queries stay inside text ~ "..."
    for q in plain_inputs {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "search_confluence_pages",
                    "arguments": { "query": q, "remote": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let last_cql = st.lock().unwrap().cql_sent.last().unwrap().clone();
        assert_eq!(
            last_cql,
            format!("text ~ \"{q}\""),
            "MCP CQL query '{q}' must wrap in text ~"
        );

        let encoded = urlencoding::encode(q);
        let http_res = http
            .get(format!("{api_base}/search?q={encoded}&scope=all&remote=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(http_res.status(), StatusCode::OK);
        let last_jql = st.lock().unwrap().jql_sent.last().unwrap().clone();
        let last_cql = st.lock().unwrap().cql_sent.last().unwrap().clone();
        assert!(
            last_jql.contains(&format!("text ~ \"{q}\"")),
            "HTTP Jira query '{q}' must wrap in text ~; got: {last_jql}"
        );
        assert_eq!(
            last_cql,
            format!("text ~ \"{q}\""),
            "HTTP CQL query '{q}' must wrap in text ~"
        );
    }

    // 3. Compatible raw expressions pass through untouched
    for raw_jql in [
        "project = FLS",
        "type = page",
        "status IN (\"To Do\", \"In Progress\")",
        "cf[10016] >= 5",
        "\"Story Points\" = 3",
        "ORDER BY updated DESC",
    ] {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "search_jira_issues",
                    "arguments": { "query": raw_jql }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(
            st.lock().unwrap().jql_sent.last().unwrap(),
            raw_jql,
            "compatible JQL '{raw_jql}' should pass through"
        );
    }
    for raw_cql in ["space = ENG", "type = page", "label NOT IN (archive)"] {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "search_confluence_pages",
                    "arguments": { "query": raw_cql, "remote": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(
            st.lock().unwrap().cql_sent.last().unwrap(),
            raw_cql,
            "compatible CQL '{raw_cql}' should pass through"
        );
    }

    // 4. Regression control: CLI --jql / --cql preserve custom fields & quoted and/or verbatim
    let custom_jql = "customFieldAlias = \"read and write\"";
    let cli_jql = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["search", custom_jql, "--jql", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_jql.status.success(), "{}", String::from_utf8_lossy(&cli_jql.stderr));
    assert_eq!(st.lock().unwrap().jql_sent.last().unwrap(), custom_jql);

    let custom_cql = "customMeta = \"alpha or beta\"";
    let cli_cql = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["wiki", "search", custom_cql, "--cql", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_cql.status.success(), "{}", String::from_utf8_lossy(&cli_cql.stderr));
    assert_eq!(st.lock().unwrap().cql_sent.last().unwrap(), custom_cql);

    // CLI plain-text remote search wraps 'read and write' in text ~ "..."
    let cli_plain = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["search", "read and write", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_plain.status.success(), "{}", String::from_utf8_lossy(&cli_plain.stderr));
    assert!(st
        .lock()
        .unwrap()
        .jql_sent
        .last()
        .unwrap()
        .contains("text ~ \"read and write\""));
}

#[tokio::test]
async fn regression_atlassian_bug_05_dc_jira_search_start_at_pagination() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug05State {
        dc_start_ats: Vec<usize>,
        cloud_tokens: Vec<Option<String>>,
    }

    let st = Arc::new(Mutex::new(Bug05State::default()));
    let all_dc_issues: Vec<Value> = (1..=7)
        .map(|i| {
            json!({
                "id": format!("1000{i}"),
                "key": format!("DC-{i}"),
                "fields": {
                    "project": { "key": "DC", "name": "DC Project" },
                    "summary": format!("DC Issue {i}"),
                    "status": { "id": "10", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "created": "2026-11-15T08:00:00.000+0000",
                    "updated": format!("2026-11-15T10:0{i}:00.000+0000")
                }
            })
        })
        .collect();

    let router = Router::new()
        .route(
            "/rest/api/2/search",
            post({
                let st = st.clone();
                let issues = all_dc_issues.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    let issues = issues.clone();
                    async move {
                        let start_at = body["startAt"].as_u64().unwrap_or(0) as usize;
                        let max_results = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        st.lock().unwrap().dc_start_ats.push(start_at);
                        let slice: Vec<Value> = issues
                            .iter()
                            .skip(start_at)
                            .take(max_results)
                            .cloned()
                            .collect();
                        Json(json!({
                            "startAt": start_at,
                            "maxResults": max_results,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            post({
                let st = st.clone();
                let issues = all_dc_issues.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    let issues = issues.clone();
                    async move {
                        let tok = body
                            .get("nextPageToken")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        st.lock().unwrap().cloud_tokens.push(tok.clone());
                        if tok.is_none() {
                            Json(json!({
                                "issues": issues[0..2].to_vec(),
                                "nextPageToken": "tok-page-2",
                                "total": 3
                            }))
                        } else {
                            Json(json!({
                                "issues": issues[2..3].to_vec(),
                                "total": 3
                            }))
                        }
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let dc_auth = AuthManager::with_credentials(
        "bug05-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .unwrap();
    let dc_cfg = AppConfig {
        site_url: base_url.clone(),
        confluence_url: base_url.clone(),
        deployment: DeploymentKind::DataCenter,
        ..Default::default()
    };
    let dc_storage = Storage::open_in_memory().unwrap();

    // 1. MCP DC pagination: 0 -> 3 -> 6 with limit 3 over 7 issues (3/3/1, 0 duplicates, last next_start_at=null)
    let mut mcp_keys = Vec::new();
    let mut next_start: Option<u64> = None;
    let mut page_sizes = Vec::new();
    for step in 0..3 {
        let mut args = json!({ "jql": "project = DC", "limit": 3 });
        if let Some(sa) = next_start {
            args["start_at"] = json!(sa);
        }
        let resp = mcp::handle_rpc_line(
            &dc_cfg,
            &dc_client,
            &dc_storage,
            &json!({
                "jsonrpc": "2.0",
                "id": step + 1,
                "method": "tools/call",
                "params": { "name": "search_jira_issues", "arguments": args }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "MCP page {step} failed: {resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let issues = payload["issues"].as_array().unwrap();
        page_sizes.push(issues.len());
        for i in issues {
            mcp_keys.push(i["key"].as_str().unwrap().to_string());
        }
        next_start = payload["next_start_at"].as_u64();
        if step < 2 {
            assert_eq!(next_start, Some(((step + 1) * 3) as u64));
        } else {
            assert_eq!(payload["next_start_at"], Value::Null);
        }
    }
    assert_eq!(page_sizes, vec![3, 3, 1]);
    assert_eq!(mcp_keys, vec!["DC-1", "DC-2", "DC-3", "DC-4", "DC-5", "DC-6", "DC-7"]);

    // 2. HTTP DC pagination: 0 -> 3 -> 6 with limit 3 over 7 issues (3/3/1, 0 duplicates, last next_start_at=null)
    let api_router = serve::build_router(dc_cfg.clone(), dc_client.clone(), dc_storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();

    let mut http_keys = Vec::new();
    let mut http_sizes = Vec::new();
    let mut next_start: Option<u64> = None;
    for step in 0..3 {
        let url = match next_start {
            Some(sa) => format!("{api_base}/issues?jql=project%20%3D%20DC&limit=3&start_at={sa}"),
            None => format!("{api_base}/issues?jql=project%20%3D%20DC&limit=3"),
        };
        let res = http.get(&url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        let issues = payload["issues"].as_array().unwrap();
        http_sizes.push(issues.len());
        for i in issues {
            http_keys.push(i["key"].as_str().unwrap().to_string());
        }
        next_start = payload["next_start_at"].as_u64();
        if step < 2 {
            assert_eq!(next_start, Some(((step + 1) * 3) as u64));
        } else {
            assert_eq!(payload["next_start_at"], Value::Null);
        }
    }
    assert_eq!(http_sizes, vec![3, 3, 1]);
    assert_eq!(http_keys, vec!["DC-1", "DC-2", "DC-3", "DC-4", "DC-5", "DC-6", "DC-7"]);
    assert_eq!(st.lock().unwrap().dc_start_ats, vec![0, 3, 6, 0, 3, 6]);

    // 3. Invalid inputs (negative, wrong type, conflicting cursors) -> MCP input error / HTTP 400 with 0 Jira calls
    let hits_before = st.lock().unwrap().dc_start_ats.len();
    for bad_args in [
        json!({ "jql": "project = DC", "start_at": -1 }),
        json!({ "jql": "project = DC", "start_at": "3" }),
        json!({ "jql": "project = DC", "start_at": 3, "page_token": "tok-1" }),
    ] {
        let resp = mcp::handle_rpc_line(
            &dc_cfg,
            &dc_client,
            &dc_storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": { "name": "search_jira_issues", "arguments": bad_args }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true, "expected MCP input error for {bad_args}: {resp}");
    }
    for bad_url in [
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=-1"),
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=abc"),
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=3&page_token=tok-1"),
    ] {
        let res = http.get(&bad_url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "expected HTTP 400 for {bad_url}");
    }
    assert_eq!(
        st.lock().unwrap().dc_start_ats.len(),
        hits_before,
        "invalid cursor inputs must make 0 Jira requests"
    );

    // 4. Regression control: Cloud page_token roundtrip and first-page shape remain unchanged
    let cloud_auth = AuthManager::with_credentials(
        "bug05-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth).unwrap();
    let cloud_cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let cloud_storage = Storage::open_in_memory().unwrap();
    let p1 = mcp::handle_rpc_line(
        &cloud_cfg,
        &cloud_client,
        &cloud_storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "search_jira_issues", "arguments": { "jql": "project = DC", "limit": 2 } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(p1["result"]["isError"], false);
    let p1_val: Value =
        serde_json::from_str(p1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p1_val["next_page_token"], "tok-page-2");
    assert_eq!(p1_val["next_start_at"], Value::Null);

    let p2 = mcp::handle_rpc_line(
        &cloud_cfg,
        &cloud_client,
        &cloud_storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "search_jira_issues", "arguments": { "jql": "project = DC", "limit": 2, "page_token": "tok-page-2" } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(p2["result"]["isError"], false);
    let p2_val: Value =
        serde_json::from_str(p2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p2_val["issues"].as_array().unwrap().len(), 1);
    assert_eq!(p2_val["next_page_token"], Value::Null);
    assert_eq!(
        st.lock().unwrap().cloud_tokens,
        vec![None, Some("tok-page-2".to_string())]
    );
}

#[tokio::test]
async fn regression_atlassian_bug_09_remote_unified_search_global_ordering() {
    // 5 Jira issues + 3 Confluence pages with interleaved timestamps AND a timestamp tie
    // Expected global order (updated_at DESC, site_id ASC, id_or_key ASC):
    //   1. "P-900" (2026-11-20T12:00:00Z)  [Confluence]
    //   2. "ENG-1" (2026-11-20T11:30:00Z)  [Jira]
    //   3. "ENG-2" (2026-11-20T11:00:00Z)  [Jira, tied with P-800 -> "ENG-2" < "P-800"]
    //   4. "P-800" (2026-11-20T11:00:00Z)  [Confluence, tied with ENG-2]
    //   5. "ENG-3" (2026-11-20T10:30:00Z)  [Jira]
    //   6. "P-700" (2026-11-20T10:00:00Z)  [Confluence]
    //   7. "ENG-4" (2026-11-20T09:30:00Z)  [Jira]
    //   8. "ENG-5" (2026-11-20T09:00:00Z)  [Jira]
    let jira_items = vec![
        ("ENG-1", "2026-11-20T11:30:00.000+0000"),
        ("ENG-2", "2026-11-20T11:00:00.000+0000"),
        ("ENG-3", "2026-11-20T10:30:00.000+0000"),
        ("ENG-4", "2026-11-20T09:30:00.000+0000"),
        ("ENG-5", "2026-11-20T09:00:00.000+0000"),
    ];
    let conf_items = vec![
        ("P-900", "2026-11-20T12:00:00Z"),
        ("P-800", "2026-11-20T11:00:00Z"),
        ("P-700", "2026-11-20T10:00:00Z"),
    ];

    let jira_all_newer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = Router::new()
        .route(
            "/rest/api/3/search/jql",
            post({
                let jira_all_newer = jira_all_newer.clone();
                move |Json(body): Json<Value>| {
                    let all_newer = jira_all_newer.load(Ordering::SeqCst);
                    let items = jira_items.clone();
                    async move {
                        let max_results = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let issues: Vec<Value> = items
                            .iter()
                            .take(max_results)
                            .enumerate()
                            .map(|(idx, (key, ts))| {
                                let updated = if all_newer {
                                    format!("2026-11-21T1{}:00:00.000+0000", 9 - idx)
                                } else {
                                    (*ts).to_string()
                                };
                                json!({
                                    "id": format!("j-{idx}"),
                                    "key": key,
                                    "fields": {
                                        "project": { "key": "ENG", "name": "Engineering" },
                                        "summary": format!("Rollout {key}"),
                                        "status": { "id": "10", "name": "Open", "statusCategory": { "key": "new" } },
                                        "issuetype": { "id": "1", "name": "Task", "subtask": false },
                                        "created": "2026-11-01T00:00:00.000+0000",
                                        "updated": updated
                                    }
                                })
                            })
                            .collect();
                        Json(json!({ "issues": issues, "total": issues.len() }))
                    }
                }
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get({
                move |Query(q): Query<HashMap<String, String>>| {
                    let items = conf_items.clone();
                    async move {
                        let limit = q
                            .get("limit")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(50);
                        let results: Vec<Value> = items
                            .iter()
                            .take(limit)
                            .map(|(id, ts)| {
                                json!({
                                    "content": {
                                        "id": id,
                                        "type": "page",
                                        "title": format!("Rollout Doc {id}"),
                                        "space": { "id": 5001, "key": "ENG" },
                                        "version": { "number": 1, "when": ts },
                                        "_links": { "webui": format!("/spaces/ENG/pages/{id}") }
                                    },
                                    "lastModified": ts
                                })
                            })
                            .collect();
                        Json(json!({ "results": results }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let auth = AuthManager::with_credentials(
        "bug09-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .unwrap();
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().unwrap();
    let api_router = serve::build_router(cfg.clone(), client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();

    // 1. Interleaved Jira + Confluence with limit=5 -> exact top 5 across MCP & HTTP including tie-break
    let expected_top5 = vec!["P-900", "ENG-1", "ENG-2", "P-800", "ENG-3"];

    let mcp_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "search_all",
                "arguments": { "query": "Rollout", "scope": "all", "remote": true, "limit": 5 }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    let mcp_payload: Value =
        serde_json::from_str(mcp_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let mcp_ids: Vec<&str> = mcp_payload["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(mcp_ids, expected_top5, "MCP remote unified search order mismatch");

    let http_payload: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=all&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let http_ids: Vec<&str> = http_payload["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(http_ids, expected_top5, "HTTP remote unified search order mismatch");

    // 2. Regression control: single-service scope still returns only that service
    let http_conf_only: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=confluence&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let conf_ids: Vec<&str> = http_conf_only["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(conf_ids, vec!["P-900", "P-800", "P-700"]);

    // 3. Regression control: when all Jira items are strictly newer, top 5 are all Jira (no artificial quota)
    jira_all_newer.store(true, Ordering::SeqCst);
    let http_all_jira: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=all&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let all_jira_ids: Vec<&str> = http_all_jira["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(all_jira_ids, vec!["ENG-1", "ENG-2", "ENG-3", "ENG-4", "ENG-5"]);
}

#[tokio::test]
async fn regression_atlassian_bug_14_cloud_page_ancestors_and_controls() {
    use std::sync::{atomic::AtomicBool, Mutex};

    #[derive(Default)]
    struct Bug14State {
        requests: Vec<String>,
        fail_ancestors_300: bool,
    }

    let st = Arc::new(Mutex::new(Bug14State::default()));
    let deep_fail = Arc::new(AtomicBool::new(false));
    let router = Router::new().fallback({
        let st = st.clone();
        let _deep_fail = deep_fail.clone();
        move |uri: axum::http::Uri| {
            let st = st.clone();
            async move {
                let path = uri.path().to_string();
                let full = uri.to_string();
                st.lock().unwrap().requests.push(full);

                // Root page 100 (no parentId)
                if path == "/wiki/api/v2/pages/100" {
                    return Json(json!({
                        "id": "100",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": Value::Null,
                        "status": "current",
                        "title": "Root Guide",
                        "version": { "number": 1, "createdAt": "2026-11-15T08:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Root\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/100" }
                    }))
                    .into_response();
                }
                // Parent page 200 (parentId=100)
                if path == "/wiki/api/v2/pages/200" {
                    return Json(json!({
                        "id": "200",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "100",
                        "status": "current",
                        "title": "Parent Section",
                        "version": { "number": 1, "createdAt": "2026-11-15T09:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Parent\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/200" }
                    }))
                    .into_response();
                }
                // Child page 300 (parentId=200)
                if path == "/wiki/api/v2/pages/300" {
                    return Json(json!({
                        "id": "300",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "200",
                        "status": "current",
                        "title": "Child Spec",
                        "version": { "number": 2, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Child\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/300" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/300/ancestors" {
                    if st.lock().unwrap().fail_ancestors_300 {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "message": "temporary ancestor error" })),
                        )
                            .into_response();
                    }
                    // Official v2 response: only id & type (no title!)
                    return Json(json!({
                        "results": [
                            { "id": "100", "type": "page" },
                            { "id": "200", "type": "page" }
                        ]
                    }))
                    .into_response();
                }

                // Forbidden ancestor test: page 400 has ancestors [401 (forbidden folder), 402 (readable folder)]
                if path == "/wiki/api/v2/pages/400" {
                    return Json(json!({
                        "id": "400",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "402",
                        "status": "current",
                        "title": "Restricted Child",
                        "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/400" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/400/ancestors" {
                    return Json(json!({
                        "results": [
                            { "id": "401", "type": "folder" },
                            { "id": "402", "type": "folder" }
                        ]
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/folders/401" {
                    return (StatusCode::FORBIDDEN, Json(json!({ "message": "Forbidden" }))).into_response();
                }
                if path == "/wiki/api/v2/folders/402" {
                    return Json(json!({ "id": "402", "title": "Team Folder" })).into_response();
                }

                // Deep tree (> 250 ancestors): page 900 -> 250 ancestors [d-2..d-251], then d-2 -> [d-0, d-1]
                if path == "/wiki/api/v2/pages/900" {
                    return Json(json!({
                        "id": "900",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "d-251",
                        "status": "current",
                        "title": "Deep Leaf",
                        "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/900" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/900/ancestors" {
                    let results: Vec<Value> = (2..=251)
                        .map(|n| json!({ "id": format!("d-{n}"), "type": "page" }))
                        .collect();
                    return Json(json!({ "results": results })).into_response();
                }
                if path == "/wiki/api/v2/pages/d-2/ancestors" {
                    return Json(json!({
                        "results": [
                            { "id": "d-0", "type": "page" },
                            { "id": "d-1", "type": "page" }
                        ]
                    }))
                    .into_response();
                }
                if let Some(did) = path.strip_prefix("/wiki/api/v2/pages/d-") {
                    if !did.contains('/') {
                        return Json(json!({ "id": format!("d-{did}"), "title": format!("Level {did}") }))
                            .into_response();
                    }
                }

                // DC control page 501
                if path == "/rest/api/content/501" {
                    return Json(json!({
                        "id": "501",
                        "type": "page",
                        "title": "DC Child",
                        "space": { "id": 1, "key": "ENG" },
                        "version": { "number": 1, "when": "2026-11-15T10:00:00Z" },
                        "ancestors": [
                            { "id": "500", "title": "DC Root" }
                        ],
                        "body": { "storage": { "value": "<p>DC</p>" } },
                        "metadata": { "labels": { "results": [] } },
                        "children": { "attachment": { "results": [] } }
                    }))
                    .into_response();
                }
                if path.ends_with("/labels")
                    || path.ends_with("/attachments")
                    || path.ends_with("/footer-comments")
                    || path.ends_with("/child/comment")
                {
                    return Json(json!({ "results": [] })).into_response();
                }
                (StatusCode::NOT_FOUND, format!("Unhandled {path}")).into_response()
            }
        }
    });

    let base_url = spawn_server(router).await;
    let cloud_auth = AuthManager::with_credentials(
        "bug14-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth).unwrap();
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().unwrap();

    // 1. Root page 100 -> empty ancestors, 0 /ancestors calls
    let root_detail = cloud_client.get_page("100").await.unwrap();
    assert!(root_detail.ancestors.is_empty());
    assert!(
        !st.lock().unwrap().requests.iter().any(|r| r.contains("/100/ancestors")),
        "root page must not query /ancestors"
    );
    storage.upsert_page_detail(&root_detail).unwrap();

    // 2. Child page 300 via MCP get_confluence_page (compact=false):
    //    100 is cached (no extra GET /pages/100!), 200 is fetched metadata-only (no /200/footer-comments or /200/attachments!)
    st.lock().unwrap().requests.clear();
    let mcp_child = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "300", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_child["result"]["isError"], false, "{mcp_child}");
    let child_payload: Value =
        serde_json::from_str(mcp_child["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let ancestors = child_payload["page"]["ancestors"].as_array().unwrap();
    assert_eq!(
        ancestors,
        &vec![
            json!({ "id": "100", "title": "Root Guide" }),
            json!({ "id": "200", "title": "Parent Section" }),
        ],
        "child ancestors must be ordered root -> parent with resolved titles"
    );
    {
        let reqs = &st.lock().unwrap().requests;
        assert!(
            !reqs.iter().any(|r| r.contains("/pages/100")),
            "cached ancestor 100 should use site cache first: {reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| r.contains("/pages/200/footer-comments") || r.contains("/pages/200/attachments")),
            "ancestor title resolution must not recursively fetch comments/attachments: {reqs:?}"
        );
    }

    // Offline re-read returns the exact same ancestors
    let offline_child = storage.get_page_detail("300").unwrap().unwrap();
    assert_eq!(offline_child.ancestors.len(), 2);
    assert_eq!(offline_child.ancestors[0].id, "100");
    assert_eq!(offline_child.ancestors[0].title, "Root Guide");
    assert_eq!(offline_child.ancestors[1].id, "200");
    assert_eq!(offline_child.ancestors[1].title, "Parent Section");

    // 3. Ancestor fetch failure on refresh must NOT overwrite cached ancestors with []
    st.lock().unwrap().fail_ancestors_300 = true;
    let mcp_refresh = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "300", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_refresh["result"]["isError"], false);
    let preserved = storage.get_page_detail("300").unwrap().unwrap();
    assert_eq!(
        preserved.ancestors.len(),
        2,
        "failed /ancestors read must preserve previously cached ancestor path"
    );

    // 4. Forbidden ancestor metadata falls back to displaying ID as title, while readable folder gets its title
    let detail_400 = cloud_client.get_page("400").await.unwrap();
    assert_eq!(detail_400.ancestors.len(), 2);
    assert_eq!(detail_400.ancestors[0].id, "401");
    assert_eq!(detail_400.ancestors[0].title, "401");
    assert_eq!(detail_400.ancestors[1].id, "402");
    assert_eq!(detail_400.ancestors[1].title, "Team Folder");

    // 5. Deep tree (>250 ancestors): merges upper segment in front without duplicates
    // Pre-cache d-0..d-251 titles in storage to keep the deep-tree test super fast and verify cache lookup
    let cached_deep: Vec<atlassian_cli::models::PageSummary> = (0..=251)
        .map(|n| atlassian_cli::models::PageSummary {
            id: format!("d-{n}"),
            space_id: "5001".into(),
            space_key: "ENG".into(),
            title: format!("Level {n}"),
            version_number: 1,
            ..Default::default()
        })
        .collect();
    storage.upsert_pages(&cached_deep).unwrap();
    let mcp_deep = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "900", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_deep["result"]["isError"], false, "{mcp_deep}");
    let deep_payload: Value =
        serde_json::from_str(mcp_deep["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let deep_ancestors = deep_payload["page"]["ancestors"].as_array().unwrap();
    assert_eq!(deep_ancestors.len(), 252, "expected 252 unique ancestors (d-0..d-251)");
    assert_eq!(deep_ancestors[0], json!({ "id": "d-0", "title": "Level 0" }));
    assert_eq!(deep_ancestors[1], json!({ "id": "d-1", "title": "Level 1" }));
    assert_eq!(deep_ancestors[2], json!({ "id": "d-2", "title": "Level 2" }));
    assert_eq!(deep_ancestors[251], json!({ "id": "d-251", "title": "Level 251" }));

    // 6. Control: Data Center detail continues to preserve its ancestors
    let dc_auth = AuthManager::with_credentials(
        "bug14-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .unwrap();
    let dc_detail = dc_client.get_page("501").await.unwrap();
    assert_eq!(dc_detail.ancestors.len(), 1);
    assert_eq!(dc_detail.ancestors[0].id, "500");
    assert_eq!(dc_detail.ancestors[0].title, "DC Root");
}

#[tokio::test]
async fn regression_atlassian_05_collection_paging_and_controls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let req_count = Arc::new(AtomicUsize::new(0));
    let projects: Vec<Value> = (1..=150)
        .map(|i| {
            json!({
                "id": format!("{i}"),
                "key": format!("P{i:03}"),
                "name": format!("Project {i:03}"),
                "projectTypeKey": "software"
            })
        })
        .collect();
    let issues: Vec<Value> = (1..=120)
        .map(|i| {
            json!({
                "id": format!("{i}"),
                "key": format!("ENG-{i:03}"),
                "fields": {
                    "project": { "key": "ENG", "name": "Engineering" },
                    "summary": format!("Board Issue {i:03}"),
                    "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "updated": format!("2026-11-20T{:02}:{:02}:00Z", i / 60, i % 60)
                }
            })
        })
        .collect();

    let router = Router::new()
        .route(
            "/rest/api/3/project/search",
            get({
                let req_count = req_count.clone();
                let projects = projects.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    let projects = projects.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("startAt").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let max: usize = q.get("maxResults").and_then(|s| s.parse().ok()).unwrap_or(100);
                        let slice: Vec<Value> = projects.iter().skip(start).take(max).cloned().collect();
                        let is_last = start + slice.len() >= projects.len();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": projects.len(),
                            "isLast": is_last,
                            "values": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/project",
            get({
                let req_count = req_count.clone();
                let projects = projects.clone();
                move || {
                    let req_count = req_count.clone();
                    let projects = projects.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!(projects))
                    }
                }
            }),
        )
        .route(
            "/rest/agile/1.0/board/7/issue",
            get({
                let req_count = req_count.clone();
                let issues = issues.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    let issues = issues.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("startAt").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let max: usize = q.get("maxResults").and_then(|s| s.parse().ok()).unwrap_or(50);
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/wiki/api/v2/pages/700/children",
            get({
                let req_count = req_count.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        if q.get("cursor").map(String::as_str) == Some("c2=") {
                            Json(json!({
                                "results": [
                                    { "id": "703", "spaceId": "77", "parentId": "700", "title": "Child 3", "status": "current", "version": { "number": 1 } }
                                ]
                            }))
                        } else {
                            Json(json!({
                                "results": [
                                    { "id": "701", "spaceId": "77", "parentId": "700", "title": "Child 1", "status": "current", "version": { "number": 1 } },
                                    { "id": "702", "spaceId": "77", "parentId": "700", "title": "Child 2", "status": "current", "version": { "number": 1 } }
                                ],
                                "_links": { "next": "/wiki/api/v2/pages/700/children?limit=2&cursor=c2%3D" }
                            }))
                        }
                    }
                }
            }),
        )
        .route(
            "/rest/api/content/700/child/page",
            get({
                let req_count = req_count.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("start").and_then(|s| s.parse().ok()).unwrap_or(0);
                        if start == 0 {
                            Json(json!({
                                "results": [
                                    { "id": "701", "title": "Child 1", "space": { "key": "ENG" }, "version": { "number": 1 } },
                                    { "id": "702", "title": "Child 2", "space": { "key": "ENG" }, "version": { "number": 1 } }
                                ],
                                "_links": { "next": "/rest/api/content/700/child/page?limit=2&start=2" }
                            }))
                        } else {
                            Json(json!({
                                "results": [
                                    { "id": "703", "title": "Child 3", "space": { "key": "ENG" }, "version": { "number": 1 } }
                                ]
                            }))
                        }
                    }
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            post({
                let issues = issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body
                            .get("nextPageToken")
                            .and_then(Value::as_str)
                            .and_then(|t| t.strip_prefix("tok:"))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        let mut resp = json!({ "issues": slice });
                        if start + slice.len() < issues.len() {
                            resp["nextPageToken"] = json!(format!("tok:{}", start + slice.len()));
                        }
                        Json(resp)
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/search",
            post({
                let issues = issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body["startAt"].as_u64().unwrap_or(0) as usize;
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;

    async fn call_mcp(
        cfg: &AppConfig,
        client: &AtlassianClient,
        storage: &Storage,
        name: &str,
        arguments: Value,
    ) -> (bool, Value) {
        let resp = mcp::handle_rpc_line(
            cfg,
            client,
            storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert!(
            resp["result"].get("structuredContent").is_none(),
            "MCP result must not add structuredContent"
        );
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        if is_error {
            (true, Value::String(text.to_string()))
        } else {
            (false, serde_json::from_str(text).unwrap())
        }
    }

    for deployment in [DeploymentKind::Cloud, DeploymentKind::DataCenter] {
        let conf_url = if deployment == DeploymentKind::Cloud {
            format!("{base_url}/wiki")
        } else {
            base_url.clone()
        };
        let auth = AuthManager::with_credentials(
            "paging-test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, &conf_url, deployment, auth).unwrap();
        let cfg = AppConfig {
            site_url: base_url.clone(),
            confluence_url: conf_url,
            deployment,
            ..Default::default()
        };
        let storage = Storage::open_in_memory().unwrap();

        // 1. get_board_issues: limit 500 -> 100 / true / "100", then cursor "100" -> 20 / false / null
        let (err, b1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 500 }),
        )
        .await;
        assert!(!err, "{b1}");
        assert_eq!(b1["returned"], 100);
        assert_eq!(b1["has_more"], true);
        assert_eq!(b1["next_cursor"], "100");
        assert_eq!(b1["issues"].as_array().unwrap().len(), 100);

        let (err, b2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 500, "cursor": "100" }),
        )
        .await;
        assert!(!err, "{b2}");
        assert_eq!(b2["returned"], 20);
        assert_eq!(b2["has_more"], false);
        assert!(b2["next_cursor"].is_null());

        // Offline continuation reads all 120 from cache in consistent order
        let (err, off1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 70, "offline": true }),
        )
        .await;
        assert!(!err, "{off1}");
        assert_eq!(off1["returned"], 70);
        assert_eq!(off1["has_more"], true);
        assert_eq!(off1["next_cursor"], "70");
        let (err, off2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 70, "cursor": "70", "offline": true }),
        )
        .await;
        assert!(!err, "{off2}");
        assert_eq!(off2["returned"], 50);
        assert_eq!(off2["has_more"], false);
        assert!(off2["next_cursor"].is_null());

        // 2. list_projects: refresh -> 50 / "50" -> 50 / "100" -> 50 / null (all 150 in both Cloud & DC)
        let mut recovered_projects = Vec::new();
        let proj_before = req_count.load(Ordering::SeqCst);
        let (err, pr1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "refresh": true }),
        )
        .await;
        assert!(!err, "{pr1}");
        let proj_calls = req_count.load(Ordering::SeqCst) - proj_before;
        assert_eq!(
            proj_calls,
            if deployment == DeploymentKind::Cloud { 2 } else { 1 },
            "Cloud follows isLast (2 requests); Data Center fetches once"
        );
        assert_eq!(pr1["returned"], 50);
        assert_eq!(pr1["has_more"], true);
        assert_eq!(pr1["next_cursor"], "50");
        recovered_projects.extend(
            pr1["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        let (err, pr2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "cursor": "50" }),
        )
        .await;
        assert!(!err, "{pr2}");
        assert_eq!(pr2["returned"], 50);
        assert_eq!(pr2["has_more"], true);
        assert_eq!(pr2["next_cursor"], "100");
        recovered_projects.extend(
            pr2["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        let (err, pr3) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "cursor": "100" }),
        )
        .await;
        assert!(!err, "{pr3}");
        assert_eq!(pr3["returned"], 50);
        assert_eq!(pr3["has_more"], false);
        assert!(pr3["next_cursor"].is_null());
        recovered_projects.extend(
            pr3["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        assert_eq!(recovered_projects.len(), 150);

        // 3. list_child_pages: limit 2 -> cursor -> remaining 1
        let (err, ch1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_child_pages",
            json!({ "page_id": "700", "limit": 2 }),
        )
        .await;
        assert!(!err, "{ch1}");
        assert_eq!(ch1["returned"], 2);
        assert_eq!(ch1["has_more"], true);
        let cursor = ch1["next_cursor"].as_str().unwrap().to_string();
        let (err, ch2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_child_pages",
            json!({ "page_id": "700", "limit": 2, "cursor": cursor }),
        )
        .await;
        assert!(!err, "{ch2}");
        assert_eq!(ch2["returned"], 1);
        assert_eq!(ch2["has_more"], false);
        assert!(ch2["next_cursor"].is_null());

        // 4. Invalid offset cursor rejects with zero upstream calls
        let before = req_count.load(Ordering::SeqCst);
        let (is_err, err_msg) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "cursor": "abc" }),
        )
        .await;
        assert!(is_err);
        let parsed_err: Value = serde_json::from_str(err_msg.as_str().unwrap()).unwrap();
        assert_eq!(parsed_err["ok"], false);
        assert_eq!(
            parsed_err["error"]["message"].as_str().unwrap(),
            "cursor must be a non-negative integer offset taken from next_cursor"
        );
        assert_eq!(
            req_count.load(Ordering::SeqCst),
            before,
            "invalid cursor must not trigger upstream requests"
        );

        // 5. Control: search_jira_issues preserves v0.5.1 keys and 2-page continuation
        let (err, s1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "search_jira_issues",
            json!({ "jql": "project = ENG", "limit": 100 }),
        )
        .await;
        assert!(!err, "{s1}");
        let mut s1_keys: Vec<&str> = s1.as_object().unwrap().keys().map(String::as_str).collect();
        s1_keys.sort();
        assert_eq!(
            s1_keys,
            vec!["issues", "jql", "next_page_token", "next_start_at", "site_id", "total"]
        );
        assert_eq!(s1["issues"].as_array().unwrap().len(), 100);
        let next_args = if deployment == DeploymentKind::Cloud {
            assert!(s1["total"].is_null());
            json!({ "jql": "project = ENG", "limit": 100, "page_token": s1["next_page_token"] })
        } else {
            assert_eq!(s1["total"], 120);
            json!({ "jql": "project = ENG", "limit": 100, "start_at": s1["next_start_at"] })
        };
        let (err, s2) = call_mcp(&cfg, &client, &storage, "search_jira_issues", next_args).await;
        assert!(!err, "{s2}");
        assert_eq!(s2["issues"].as_array().unwrap().len(), 20);
        assert!(s2["next_page_token"].is_null());
        assert!(s2["next_start_at"].is_null());
    }
}

#[tokio::test]
async fn http_offline_issues_total_is_cache_count() {
    ensure_isolated_env();
    let storage = Storage::open_in_memory().expect("storage");
    let issues: Vec<atlassian_cli::models::IssueSummary> = (1..=120)
        .map(|i| atlassian_cli::models::IssueSummary {
            id: format!("{i}"),
            key: format!("ENG-{i}"),
            project_key: "ENG".into(),
            summary: format!("Cached issue {i}"),
            updated_at: format!("2026-11-20T{:02}:{:02}:00Z", i / 60, i % 60),
            ..Default::default()
        })
        .collect();
    storage.upsert_issues(&issues).unwrap();
    storage
        .upsert_issue_detail(&atlassian_cli::models::IssueDetail {
            summary: issues[0].clone(),
            description_raw: "raw body".into(),
            description_markdown: "md body".into(),
            ..Default::default()
        })
        .unwrap();

    let cfg = AppConfig::default();
    let auth = AuthManager::new("http-offline-total");
    let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
    let serve_url = spawn_server(serve::build_router(cfg, client, storage, serve::ServeOptions::default())).await;

    let http = reqwest::Client::new();
    let r2: Value = http
        .get(format!("{serve_url}/issues?offline=true&limit=2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r2["issues"].as_array().unwrap().len(), 2);
    assert_eq!(r2["total"], 120);

    let r1000: Value = http
        .get(format!("{serve_url}/issues?offline=true&limit=1000"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r1000["issues"].as_array().unwrap().len(), 120);
    assert_eq!(r1000["total"], 120);

    // Control: HTTP detail still defaults to full
    let detail: Value = http
        .get(format!("{serve_url}/issues/ENG-1?offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["issue"]["description_raw"], "raw body");
    assert!(detail["issue"].get("omitted").is_none());
}

#[tokio::test]
async fn http_offline_issues_filters_status_before_limit() {
    ensure_isolated_env();
    let storage = Storage::open_in_memory().expect("storage");
    let issue = |key: &str, project: &str, done: bool, updated_at: String| {
        atlassian_cli::models::IssueSummary {
            id: key.into(),
            key: key.into(),
            project_key: project.into(),
            summary: format!("Cached issue {key}"),
            status: atlassian_cli::models::IssueStatus {
                id: if done { "3" } else { "2" }.into(),
                name: if done { "Done" } else { "In Progress" }.into(),
                category: if done { StatusCategory::Done } else { StatusCategory::InProgress },
            },
            updated_at,
            ..Default::default()
        }
    };
    // The 50 newest ENG issues are Done; ten older ones are still in progress. OPS-1 is the
    // newest row of all and belongs to another project.
    let mut issues: Vec<_> = (1..=60)
        .map(|i| issue(&format!("ENG-{i}"), "ENG", i > 10, format!("2026-11-20T10:00:{i:02}Z")))
        .collect();
    issues.push(issue("OPS-1", "OPS", false, "2026-11-21T00:00:00Z".into()));
    storage.upsert_issues(&issues).unwrap();

    let cfg = AppConfig {
        default_project: Some("ENG".into()),
        ..AppConfig::default()
    };
    let auth = AuthManager::new("http-offline-status");
    let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
    let serve_url =
        spawn_server(serve::build_router(cfg, client, storage, serve::ServeOptions::default())).await;
    let http = reqwest::Client::new();
    let get = |query: &str| http.get(format!("{serve_url}/issues?offline=true&{query}")).send();

    let open: Value = get("status=progress&limit=50").await.unwrap().json().await.unwrap();
    let open_issues = open["issues"].as_array().unwrap();
    assert_eq!(open_issues.len(), 10, "{open}");
    assert!(open_issues
        .iter()
        .all(|i| i["status"]["name"] == "In Progress" && i["project_key"] == "ENG"));
    assert_eq!(open["total"], 10);

    // The default project scopes offline lists the way it scopes live ones.
    let all: Value = get("limit=1000").await.unwrap().json().await.unwrap();
    assert_eq!(all["issues"].as_array().unwrap().len(), 60);
    assert_eq!(all["total"], 60);
    let ops: Value = get("project=OPS&status=progress").await.unwrap().json().await.unwrap();
    assert_eq!(ops["issues"][0]["key"], "OPS-1");
    assert_eq!(ops["total"], 1);

    let jql = get("jql=project%20%3D%20ENG").await.unwrap();
    assert_eq!(jql.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = jql.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("jql cannot be evaluated offline"), "{body}");
}
