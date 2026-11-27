use std::sync::{atomic::Ordering, Arc, RwLock};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{AppConfig, DeploymentKind},
    img_preview, mcp,
    models::{
        DocBlock, OutgoingIssue, OutgoingPage, PageUpdate, SearchHitKind, StatusCategory,
    },
    serve,
    storage::Storage,
    updater,
};

use crate::server::{
    build_mock_atlassian_router, check_bearer_auth, ensure_isolated_env, generate_test_png_bytes,
    spawn_server, MockAtlassianState,
};

#[tokio::test]
async fn e2e_cloud_jira_agile_and_sqlite_fts() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let creds = StoredCredentials::ApiToken {
        site_url: base_url.clone(),
        email: "alice@acme.com".into(),
        api_token: "cloud-api-tok-123".into(),
        deployment: DeploymentKind::Cloud,
    };
    assert!(creds.authorization_header().starts_with("Basic "));

    let auth = AuthManager::with_credentials("cloud-e2e", creds);
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("build cloud client");

    // 1. Myself, projects, agile boards/columns/sprints/issues
    let me = client.get_myself().await.expect("get_myself");
    assert_eq!(me.id, "acc-alice");
    assert_eq!(me.display_name, "Alice Kim");

    let projects = client.list_projects().await.expect("list_projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].key, "PROJ");

    let boards = client.list_boards(Some("PROJ")).await.expect("list_boards");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].id, 11);

    let cols = client.get_board_columns(11).await.expect("get_board_columns");
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[1].category_hint, StatusCategory::InProgress);

    let sprints = client.list_board_sprints(11).await.expect("list_board_sprints");
    assert_eq!(sprints.len(), 1);
    assert_eq!(sprints[0].id, 101);

    let board_issues = client.list_board_issues(11, Some(101), 50).await.expect("list_board_issues");
    assert!(board_issues.iter().any(|i| i.key == "PROJ-101"));

    // 2. Cloud POST /rest/api/3/search/jql with nextPageToken pagination
    let page1 = client
        .search_issues_jql("project = PROJ ORDER BY key ASC", 1, None, None)
        .await
        .expect("search_issues_jql page 1");
    assert_eq!(page1.issues.len(), 1);
    assert_eq!(page1.next_page_token.as_deref(), Some("page-2-token"));

    let page2 = client
        .search_issues_jql(
            "project = PROJ ORDER BY key ASC",
            1,
            page1.next_page_token.as_deref(),
            None,
        )
        .await
        .expect("search_issues_jql page 2");
    assert!(!page2.issues.is_empty());
    assert!(page2.next_page_token.is_none());
    assert_eq!(state.captured_cloud_jql_requests.read().unwrap().len(), 2);

    // 3. create_issue (Markdown -> ADF JSON wire verification)
    let created = client
        .create_issue(&OutgoingIssue {
            project_key: "PROJ".into(),
            issue_type: "Task".into(),
            summary: "신규 배포 체크".into(),
            description_markdown: "# 배포 확인\n\n- **스테이징** 검증".into(),
            priority: Some("High".into()),
            assignee_id: Some("acc-alice".into()),
            labels: vec!["deploy".into()],
            parent_key: None,
        })
        .await
        .expect("create_issue");
    assert!(created.summary.key.starts_with("PROJ-"));
    {
        let bodies = state.captured_cloud_create_issue_bodies.read().unwrap();
        let desc = &bodies[0]["fields"]["description"];
        assert_eq!(desc["type"], "doc");
        assert_eq!(desc["version"], 1);
        assert!(desc["content"].is_array());
    }

    // 4. get_issue with remote Confluence link + subtasks
    let detail = client.get_issue("PROJ-101").await.expect("get_issue PROJ-101");
    assert_eq!(detail.subtasks.len(), 1);
    assert_eq!(detail.subtasks[0].key, "PROJ-102");
    assert!(detail.linked_pages.iter().any(|p| p.page_id == "2001"));

    // 5. transition_issue, assign_issue, add_issue_comment
    client
        .transition_issue("PROJ-101", "Done", Some("배포 완료 확인"))
        .await
        .expect("transition_issue");
    client
        .assign_issue(&created.summary.key, Some("acc-alice"))
        .await
        .expect("assign_issue");
    let comment = client
        .add_issue_comment("PROJ-101", "체크 완료했습니다.")
        .await
        .expect("add_issue_comment");
    assert!(!comment.id.is_empty());

    let updated_101 = client.get_issue("PROJ-101").await.expect("re-fetch PROJ-101");
    assert_eq!(updated_101.summary.status.name, "Done");
    assert_eq!(updated_101.comments.len(), 2);

    // 6. upload_issue_attachment (X-Atlassian-Token: no-check) & download_attachment
    let tmp = tempfile::tempdir().expect("tempdir");
    let png_path = tmp.path().join("preview.png");
    let png_bytes = generate_test_png_bytes();
    std::fs::write(&png_path, &png_bytes).expect("write test png");

    let uploaded = client
        .upload_issue_attachment("PROJ-101", &png_path)
        .await
        .expect("upload_issue_attachment");
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0].filename, "preview.png");

    let dl_dir = tmp.path().join("downloads");
    let downloaded = client
        .download_attachment(&uploaded[0], &dl_dir)
        .await
        .expect("download_attachment");
    assert_eq!(std::fs::read(&downloaded.path).unwrap(), png_bytes);

    // 7. SQLite Storage + issues_fts (including 2-char Korean search "배포")
    let storage = Storage::open_in_memory().expect("open in-memory storage");
    storage.upsert_projects(&projects).expect("upsert_projects");
    storage.upsert_boards(&boards).expect("upsert_boards");
    storage.upsert_sprints(&sprints).expect("upsert_sprints");
    storage.upsert_issues(&board_issues).expect("upsert_issues");
    storage.upsert_issue_detail(&updated_101).expect("upsert_issue_detail");

    let ko_hits = storage.search_issues("배포", 10).expect("search_issues 배포");
    assert!(
        ko_hits.iter().any(|h| h.id_or_key == "PROJ-101"),
        "expected 2-char Korean query '배포' to match PROJ-101, got {ko_hits:?}"
    );
}

#[tokio::test]
async fn e2e_cloud_confluence_v2_and_unified_search() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let auth = AuthManager::with_credentials(
        "conf-e2e",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("build cloud client");

    // 1. Spaces & page tree
    let spaces = client.list_spaces(20).await.expect("list_spaces");
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0].key, "ENG");

    let roots = client
        .list_space_root_pages("ENG", 20)
        .await
        .expect("list_space_root_pages");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, "2001");

    let children = client.list_child_pages("2001").await.expect("list_child_pages");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, "2002");

    // 2. get_page (ADF + TOC + cross-ref PROJ-101 extraction)
    let page_2001 = client.get_page("2001").await.expect("get_page 2001");
    assert_eq!(page_2001.body_representation, "atlas_doc_format");
    assert!(page_2001.toc.len() >= 2, "expected TOC headings, got {:?}", page_2001.toc);
    assert!(
        page_2001.linked_issue_keys.contains(&"PROJ-101".to_string()),
        "expected PROJ-101 cross-ref, got {:?}",
        page_2001.linked_issue_keys
    );

    // 3. create_page (stringified ADF atlas_doc_format on wire)
    let created_page = client
        .create_page(&OutgoingPage {
            space_id_or_key: "ENG".into(),
            parent_id: Some("2001".into()),
            title: "신규 배포 런북".into(),
            body_markdown: "# 배포 절차\n\nPROJ-101 확인 필요".into(),
        })
        .await
        .expect("create_page");
    assert_eq!(created_page.summary.version_number, 1);
    {
        let bodies = state.captured_cloud_create_page_bodies.read().unwrap();
        assert_eq!(bodies[0]["body"]["representation"], "atlas_doc_format");
        let adf_str = bodies[0]["body"]["value"].as_str().expect("stringified ADF");
        let parsed: Value = serde_json::from_str(adf_str).expect("valid ADF JSON");
        assert_eq!(parsed["type"], "doc");
    }

    // 4. update_page (version.number increments 1 -> 2)
    let updated_page = client
        .update_page(
            &created_page.summary.id,
            &PageUpdate {
                title: "신규 배포 런북 v2".into(),
                body_markdown: "# 배포 절차 (업데이트)\n\n검증 완료".into(),
                prev_version: 1,
                version_message: Some("2차 수정".into()),
                ..Default::default()
            },
        )
        .await
        .expect("update_page");
    assert_eq!(updated_page.summary.version_number, 2);

    // 5. add_page_comment & CQL search
    let page_comment = client
        .add_page_comment(&created_page.summary.id, "검토 완료")
        .await
        .expect("add_page_comment");
    assert!(!page_comment.id.is_empty());

    let cql_hits = client
        .search_confluence_cql("text ~ \"배포\"", 20)
        .await
        .expect("search_confluence_cql");
    assert!(!cql_hits.is_empty());

    // 6. SQLite pages_fts + search_unified across BOTH Jira & Confluence
    let storage = Storage::open_in_memory().expect("open storage");
    let issues = client
        .search_issues_jql("project = PROJ", 10, None, None)
        .await
        .unwrap()
        .issues;
    storage.upsert_issues(&issues).unwrap();
    storage.upsert_spaces(&spaces).unwrap();
    storage.upsert_pages(&roots).unwrap();
    storage.upsert_page_detail(&page_2001).unwrap();

    let page_hits = storage.search_pages("배포", 10).expect("search_pages");
    assert!(page_hits.iter().any(|h| h.id_or_key == "2001"));

    let unified = storage.search_unified("배포", 20).expect("search_unified");
    assert!(
        unified.iter().any(|h| h.kind == SearchHitKind::JiraIssue),
        "expected Jira hit in unified search: {unified:?}"
    );
    assert!(
        unified.iter().any(|h| h.kind == SearchHitKind::ConfluencePage),
        "expected Confluence hit in unified search: {unified:?}"
    );
}

#[tokio::test]
async fn e2e_datacenter_pat_jira_v2_and_confluence_v1() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat-secret-999");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let creds = StoredCredentials::BearerPat {
        site_url: base_url.clone(),
        token: "dc-pat-secret-999".into(),
        deployment: DeploymentKind::DataCenter,
    };
    assert_eq!(creds.authorization_header(), "Bearer dc-pat-secret-999");

    let auth = AuthManager::with_credentials("dc-e2e", creds);
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth)
        .expect("build dc client");

    // 1. /rest/api/2/myself
    let me = client.get_myself().await.expect("dc get_myself");
    assert_eq!(me.id, "dc-admin");
    assert_eq!(me.display_name, "DC Admin");

    // 2. POST /rest/api/2/search with startAt pagination
    let p1 = client
        .search_issues_jql("project = PROJ", 1, None, Some(0))
        .await
        .expect("dc search startAt=0");
    assert_eq!(p1.issues.len(), 1);
    assert_eq!(p1.next_start_at, Some(1));

    let p2 = client
        .search_issues_jql("project = PROJ", 1, None, p1.next_start_at)
        .await
        .expect("dc search startAt=1");
    assert_eq!(p2.issues.len(), 1);
    assert!(p2.next_start_at.is_none());

    // 3. POST /rest/api/2/issue (Wiki Markup)
    let created_dc_issue = client
        .create_issue(&OutgoingIssue {
            project_key: "DC".into(),
            issue_type: "Task".into(),
            summary: "Data Center 이슈".into(),
            description_markdown: "# 제목\n\n`code` 테스트".into(),
            priority: None,
            assignee_id: None,
            labels: vec![],
            parent_key: None,
        })
        .await
        .expect("dc create_issue");
    assert!(created_dc_issue.summary.key.starts_with("DC-"));
    {
        let bodies = state.captured_dc_create_issue_bodies.read().unwrap();
        assert!(bodies[0]["fields"]["description"].is_string());
    }

    // 4. Confluence DC /rest/api/space & /rest/api/content (Storage XHTML <ac:structured-macro>)
    let spaces = client.list_spaces(20).await.expect("dc list_spaces");
    assert_eq!(spaces[0].key, "ENG");

    let dc_page = client.get_page("2001").await.expect("dc get_page 2001");
    assert_eq!(dc_page.body_representation, "storage");
    assert!(
        dc_page
            .blocks
            .iter()
            .any(|b| matches!(b, DocBlock::Panel { .. })),
        "expected <ac:structured-macro ac:name=\"info\"> to parse as DocBlock::Panel, got {:?}",
        dc_page.blocks
    );

    let dc_created_page = client
        .create_page(&OutgoingPage {
            space_id_or_key: "ENG".into(),
            parent_id: None,
            title: "DC Storage 페이지".into(),
            body_markdown: "# DC 가이드\n\n본문입니다.".into(),
        })
        .await
        .expect("dc create_page");
    assert_eq!(dc_created_page.body_representation, "storage");
    {
        let bodies = state.captured_dc_create_page_bodies.read().unwrap();
        assert_eq!(bodies[0]["body"]["storage"]["representation"], "storage");
    }
}

#[tokio::test]
async fn e2e_serve_http_mcp_image_preview_and_self_updater() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let atlassian_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = atlassian_url.clone();

    let cfg = AppConfig {
        site_url: atlassian_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..AppConfig::default()
    };

    let auth = AuthManager::with_credentials(
        "srv-e2e",
        StoredCredentials::ApiToken {
            site_url: atlassian_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth).expect("client");
    let storage = Storage::open_in_memory().expect("storage");

    // Seed cache for offline search checks
    let issue_101 = client.get_issue("PROJ-101").await.unwrap();
    storage.upsert_issue_detail(&issue_101).unwrap();
    let page_2001 = client.get_page("2001").await.unwrap();
    storage.upsert_page_detail(&page_2001).unwrap();

    // 1. HTTP API (serve::build_router)
    let api_token = "local-serve-secret";
    let serve_url = spawn_server(serve::build_router(
        cfg.clone(),
        client.clone(),
        storage.clone(),
        serve::ServeOptions {
            token: Some(api_token.to_string()),
            ..serve::ServeOptions::default()
        },
    ))
    .await;

    let http = reqwest::Client::new();
    let health_resp = http
        .get(format!("{serve_url}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health_resp.status(), StatusCode::OK);
    let health_json: Value = health_resp.json().await.unwrap();
    assert_eq!(health_json["ok"], true);

    // Bearer-protected endpoints reject missing token
    let unauth_resp = http
        .get(format!("{serve_url}/issues"))
        .send()
        .await
        .expect("GET /issues unauth");
    assert_eq!(unauth_resp.status(), StatusCode::UNAUTHORIZED);

    let issues_resp = http
        .get(format!("{serve_url}/issues"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /issues");
    assert_eq!(issues_resp.status(), StatusCode::OK);
    let issues_json: Value = issues_resp.json().await.unwrap();
    assert!(!issues_json["issues"].as_array().unwrap().is_empty());

    let page_resp = http
        .get(format!("{serve_url}/pages/2001"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /pages/2001");
    assert_eq!(page_resp.status(), StatusCode::OK);
    let page_json: Value = page_resp.json().await.unwrap();
    assert_eq!(page_json["page"]["summary"]["id"], "2001");

    let search_resp = http
        .get(format!("{serve_url}/search?q=%EB%B0%B0%ED%8F%AC"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /search");
    assert_eq!(search_resp.status(), StatusCode::OK);
    let search_json: Value = search_resp.json().await.unwrap();
    assert!(!search_json["hits"].as_array().unwrap().is_empty());

    // 2. MCP (mcp::handle_rpc_line)
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        })
        .to_string(),
    )
    .await
    .expect("initialize response");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "atlassian");

    let list_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
    )
    .await
    .expect("tools/list response");
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "move_confluence_page"));
    assert!(tools.iter().any(|t| t["name"] == "list_sites"));
    assert!(tools.iter().any(|t| t["name"] == "download_attachment"));

    let get_issue_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_jira_issue", "arguments": { "key": "PROJ-101" } }
        })
        .to_string(),
    )
    .await
    .expect("get_jira_issue mcp");
    assert_ne!(get_issue_mcp["result"]["isError"], true);

    let get_page_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "2001" } }
        })
        .to_string(),
    )
    .await
    .expect("get_confluence_page mcp");
    assert_ne!(get_page_mcp["result"]["isError"], true);

    let search_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "search_all", "arguments": { "query": "배포" } }
        })
        .to_string(),
    )
    .await
    .expect("search_all mcp");
    assert_ne!(search_mcp["result"]["isError"], true);

    // Destructive confirm: true guard
    let del_blocked = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "delete_jira_issue", "arguments": { "key": "PROJ-103" } }
        })
        .to_string(),
    )
    .await
    .expect("delete_jira_issue without confirm");
    assert_eq!(del_blocked["result"]["isError"], true);

    let del_allowed = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "delete_jira_issue", "arguments": { "key": "PROJ-103", "confirm": true } }
        })
        .to_string(),
    )
    .await
    .expect("delete_jira_issue with confirm: true");
    assert_ne!(del_allowed["result"]["isError"], true);

    // 3. Inline image preview (render_halfblocks + render_for_stdout)
    let png_bytes = generate_test_png_bytes();
    let rendered = img_preview::render_halfblocks(&png_bytes, 16, 8).expect("render_halfblocks");
    assert_eq!(rendered.width_px, 16);
    assert_eq!(rendered.height_px, 16);
    assert!(!rendered.lines.is_empty());

    std::env::set_var("ATLASSIAN_IMAGE_PROTOCOL", "halfblocks");
    let stdout_ansi = img_preview::render_for_stdout(&png_bytes, 16, 8).expect("render_for_stdout");
    assert!(stdout_ansi.contains('▀'));
    std::env::remove_var("ATLASSIAN_IMAGE_PROTOCOL");

    // 4. Self-updater against mock GitHub release endpoint
    let asset_name = if cfg!(windows) { "jira.exe" } else { "jira" };
    let new_binary_payload = b"mock-updated-jira-binary-v9.9.9".to_vec();
    let sha_hex = atlassian_cli::encode_hex(Sha256::digest(&new_binary_payload));
    let sums_txt = format!("{sha_hex}  {asset_name}\n");

    let gh_base_holder = Arc::new(RwLock::new(String::new()));
    let gh_base_clone = gh_base_holder.clone();
    let bin_bytes = new_binary_payload.clone();
    let sums_clone = sums_txt.clone();
    let asset_name_owned = asset_name.to_string();

    let gh_app = Router::new()
        .route(
            "/repos/BiteFoundry/atlassian/releases/latest",
            get(move || {
                let gh_base = gh_base_clone.read().unwrap().clone();
                let asset = asset_name_owned.clone();
                let bin_len = bin_bytes.len() as u64;
                let sums_len = sums_clone.len() as u64;
                async move {
                    Json(json!({
                        "tag_name": "v9.9.9",
                        "html_url": format!("{gh_base}/BiteFoundry/atlassian/releases/tag/v9.9.9"),
                        "assets": [
                            {
                                "name": asset,
                                "size": bin_len,
                                "browser_download_url": format!("{gh_base}/download/{asset}")
                            },
                            {
                                "name": "SHA256SUMS.txt",
                                "size": sums_len,
                                "browser_download_url": format!("{gh_base}/download/SHA256SUMS.txt")
                            }
                        ]
                    }))
                }
            }),
        )
        .route(
            &format!("/download/{asset_name}"),
            get({
                let b = new_binary_payload.clone();
                move || async move { b }
            }),
        )
        .route(
            "/download/SHA256SUMS.txt",
            get({
                let s = sums_txt.clone();
                move || async move { s }
            }),
        );

    let gh_url = spawn_server(gh_app).await;
    *gh_base_holder.write().unwrap() = gh_url.clone();

    let upd_dir = tempfile::tempdir().expect("updater tempdir");
    let target_exe = upd_dir.path().join(asset_name);
    std::fs::write(&target_exe, b"old-binary-v0.1.0").unwrap();

    let check_res = updater::check_or_apply_update_at(
        &gh_url,
        "BiteFoundry/atlassian",
        true,
        false,
        &target_exe,
        None,
    )
    .await
    .expect("update check");
    assert!(check_res.update_available);
    assert!(!check_res.updated);

    let apply_res = updater::check_or_apply_update_at(
        &gh_url,
        "BiteFoundry/atlassian",
        false,
        false,
        &target_exe,
        None,
    )
    .await
    .expect("update apply");
    assert!(apply_res.updated);
    assert_eq!(std::fs::read(&target_exe).unwrap(), new_binary_payload);
}

#[tokio::test]
async fn e2e_multi_site_oauth_3lo_refresh_collision_free_sync_and_federation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use atlassian_cli::{
        atlassian::SiteClientPool,
        auth::{exchange_oauth_code, fetch_accessible_resources},
        config::SiteProfile,
        storage::SiteRecord,
        sync::{self, CircuitBreakerMap, SiteSyncTarget},
    };

    ensure_isolated_env();

    struct MockOAuthGatewayState {
        refresh_calls: AtomicUsize,
        acme_forced_401: AtomicUsize,
        gateway_hits_acme: AtomicUsize,
        gateway_hits_side: AtomicUsize,
    }

    let oauth_state = Arc::new(MockOAuthGatewayState {
        refresh_calls: AtomicUsize::new(0),
        acme_forced_401: AtomicUsize::new(0),
        gateway_hits_acme: AtomicUsize::new(0),
        gateway_hits_side: AtomicUsize::new(0),
    });

    fn check_oauth_bearer(headers: &HeaderMap, expected: &str) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {expected}"))
    }

    let oauth_router = Router::new()
        .route(
            "/oauth/token",
            post(|State(st): State<Arc<MockOAuthGatewayState>>, Json(body): Json<Value>| async move {
                let grant = body["grant_type"].as_str().unwrap_or_default();
                match grant {
                    "authorization_code" => {
                        assert_eq!(body["code"], "code-3lo-xyz");
                        assert_eq!(body["client_id"], "mock-client-id");
                        (
                            StatusCode::OK,
                            Json(json!({
                                "access_token": "oauth-tok-v1",
                                "refresh_token": "oauth-ref-v1",
                                "expires_in": 3600,
                                "scope": "offline_access read:jira-work read:confluence-content.all"
                            })),
                        )
                    }
                    "refresh_token" => {
                        assert_eq!(body["refresh_token"], "oauth-ref-v1");
                        st.refresh_calls.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(json!({
                                "access_token": "oauth-tok-v2",
                                "refresh_token": "oauth-ref-v2",
                                "expires_in": 3600,
                                "scope": "offline_access read:jira-work read:confluence-content.all"
                            })),
                        )
                    }
                    _ => (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "unsupported_grant_type" })),
                    ),
                }
            }),
        )
        .route(
            "/oauth/token/accessible-resources",
            get(|headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" })));
                }
                (
                    StatusCode::OK,
                    Json(json!([
                        {
                            "id": "cloud-id-acme",
                            "url": "https://acme-cloud.atlassian.net",
                            "name": "Acme Cloud",
                            "scopes": ["read:jira-work", "read:confluence-content.all"]
                        },
                        {
                            "id": "cloud-id-side",
                            "url": "https://side-cloud.atlassian.net",
                            "name": "Side Cloud",
                            "scopes": ["read:jira-work", "read:confluence-content.all"]
                        }
                    ])),
                )
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/api/3/project/search",
            get(|State(st): State<Arc<MockOAuthGatewayState>>, Path(cid): Path<String>, headers: HeaderMap| async move {
                // Force a 401 on the very first request to Acme Cloud with v1 token to verify automatic refresh!
                if cid == "cloud-id-acme"
                    && check_oauth_bearer(&headers, "oauth-tok-v1")
                    && st.acme_forced_401.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "Access token expired" })),
                    );
                }
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                if cid == "cloud-id-acme" {
                    st.gateway_hits_acme.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "values": [{
                                "id": "10001",
                                "key": "ENG",
                                "name": "Acme Engineering",
                                "projectTypeKey": "software"
                            }]
                        })),
                    )
                } else {
                    st.gateway_hits_side.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "values": [{
                                "id": "20001",
                                "key": "SIDE",
                                "name": "Side Product",
                                "projectTypeKey": "software"
                            }]
                        })),
                    )
                }
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board",
            get(|Path(cid): Path<String>, headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                let (id, name, pkey) = if cid == "cloud-id-acme" {
                    (1, "Acme Scrum", "ENG")
                } else {
                    (2, "Side Kanban", "SIDE")
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "values": [{
                            "id": id,
                            "name": name,
                            "type": "scrum",
                            "location": { "projectKey": pkey, "projectName": name }
                        }]
                    })),
                )
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board/{board_id}/sprint",
            get(|Path((_cid, board_id)): Path<(String, i64)>| async move {
                Json(json!({
                    "values": [{
                        "id": 10,
                        "name": format!("Sprint 10 (B{board_id})"),
                        "state": "active"
                    }]
                }))
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board/{board_id}/sprint/{sprint_id}/issue",
            get(|Path((cid, _bid, _sid)): Path<(String, i64, i64)>| async move {
                let (key, proj, summary) = if cid == "cloud-id-acme" {
                    ("ENG-1", "ENG", "Acme Cloud Gateway Migration")
                } else {
                    ("SIDE-1", "SIDE", "Side Cloud Telemetry Rollout")
                };
                Json(json!({
                    "issues": [{
                        "id": format!("id-{cid}-{key}"),
                        "key": key,
                        "fields": {
                            "project": { "key": proj },
                            "summary": summary,
                            "status": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T12:00:00.000+0000"
                        }
                    }]
                }))
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/api/3/search/jql",
            post(|Path(cid): Path<String>, headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                let (key, proj, summary) = if cid == "cloud-id-acme" {
                    ("ENG-1", "ENG", "Acme Cloud Gateway Migration")
                } else {
                    ("SIDE-1", "SIDE", "Side Cloud Telemetry Rollout")
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "issues": [{
                            "id": format!("id-{cid}-{key}"),
                            "key": key,
                            "fields": {
                                "project": { "key": proj },
                                "summary": summary,
                                "status": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                                "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                                "updated": "2026-11-19T12:00:00.000+0000"
                            }
                        }]
                    })),
                )
            }),
        )
        .route(
            "/ex/confluence/{cloud_id}/wiki/api/v2/spaces",
            get(|Path(cid): Path<String>| async move {
                let (id, key, name) = if cid == "cloud-id-acme" {
                    ("sp-acme", "DOCS", "Acme Cloud Docs")
                } else {
                    ("sp-side", "SIDEWIKI", "Side Cloud Wiki")
                };
                Json(json!({
                    "results": [{
                        "id": id,
                        "key": key,
                        "name": name,
                        "type": "global",
                        "status": "current"
                    }]
                }))
            }),
        )
        .route(
            "/ex/confluence/{cloud_id}/wiki/api/v2/spaces/{space_id}/pages",
            get(|Path((cid, space_id)): Path<(String, String)>| async move {
                let title = if cid == "cloud-id-acme" {
                    "Acme Cloud Gateway Runbook"
                } else {
                    "Side Cloud Guide"
                };
                Json(json!({
                    "results": [{
                        "id": "100",
                        "spaceId": space_id,
                        "title": title,
                        "status": "current",
                        "version": { "number": 1, "createdAt": "2026-11-19T12:00:00Z" }
                    }]
                }))
            }),
        )
        .with_state(oauth_state.clone());

    let oauth_base = spawn_server(oauth_router).await;

    // Mock Data Center server (`corp-dc`) sharing the EXACT same project key `ENG` and issue key `ENG-1`!
    let dc_router = Router::new()
        .route(
            "/rest/api/2/project",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return *resp;
                }
                Json(json!([{
                    "id": "90001",
                    "key": "ENG",
                    "name": "Corp DC Engineering",
                    "projectTypeKey": "software"
                }]))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return *resp;
                }
                Json(json!({
                    "values": [{
                        "id": 1,
                        "name": "Corp DC Board",
                        "type": "scrum",
                        "location": { "projectKey": "ENG", "projectName": "Corp DC Engineering" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/sprint",
            get(|| async {
                Json(json!({
                    "values": [{
                        "id": 10,
                        "name": "Corp DC Sprint 10",
                        "state": "active"
                    }]
                }))
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/sprint/{sprint_id}/issue",
            get(|| async {
                Json(json!({
                    "issues": [{
                        "id": "dc-eng-1",
                        "key": "ENG-1",
                        "fields": {
                            "project": { "key": "ENG" },
                            "summary": "Corp DC Gateway Migration",
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T13:00:00.000+0000"
                        }
                    }]
                }))
            }),
        )
        .route(
            "/rest/api/2/search",
            post(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return *resp;
                }
                Json(json!({
                    "startAt": 0,
                    "maxResults": 50,
                    "total": 1,
                    "issues": [{
                        "id": "dc-eng-1",
                        "key": "ENG-1",
                        "fields": {
                            "project": { "key": "ENG" },
                            "summary": "Corp DC Gateway Migration",
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T13:00:00.000+0000"
                        }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/space",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return *resp;
                }
                Json(json!({
                    "results": [{
                        "id": 901,
                        "key": "DOCS",
                        "name": "Corp DC Docs",
                        "type": "global"
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/content",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return *resp;
                }
                Json(json!({
                    "results": [{
                        "id": "100",
                        "title": "Corp DC Gateway Runbook",
                        "status": "current",
                        "space": { "id": 901, "key": "DOCS" },
                        "version": { "number": 2, "when": "2026-11-19T13:00:00Z" }
                    }]
                }))
                .into_response()
            }),
        );

    let dc_base = spawn_server(dc_router).await;

    // 1. OAuth 3LO token exchange + accessible-resources auto-discovery
    let http = reqwest::Client::new();
    let initial_oauth_creds = exchange_oauth_code(
        &http,
        &oauth_base,
        "mock-client-id",
        Some("mock-client-secret"),
        "code-3lo-xyz",
        "http://127.0.0.1:9999/callback",
    )
    .await
    .expect("OAuth 3LO token exchange");
    assert_eq!(initial_oauth_creds.oauth_access_token(), Some("oauth-tok-v1"));

    let resources = fetch_accessible_resources(
        &http,
        &oauth_base,
        initial_oauth_creds.oauth_access_token().unwrap(),
    )
    .await
    .expect("fetch accessible-resources");
    assert_eq!(resources.len(), 2);

    let cred_dir = tempfile::tempdir().expect("isolated credentials dir");
    let cred_path = cred_dir.path().join("credentials.json");
    let oauth_auth = AuthManager::for_account("cloud-oauth").with_fallback_path(cred_path.clone());
    oauth_auth
        .save_credentials(&initial_oauth_creds)
        .expect("save OAuth credentials");
    let dc_auth = AuthManager::for_account("corp-dc-acct").with_fallback_path(cred_path.clone());
    dc_auth
        .save_credentials(&StoredCredentials::BearerPat {
            site_url: dc_base.clone(),
            token: "dc-pat-secret".to_string(),
            deployment: DeploymentKind::DataCenter,
        })
        .expect("save DC PAT credentials");

    let mut cfg = AppConfig::default();
    for res in &resources {
        cfg.upsert_site(res.to_site_profile("cloud-oauth"));
    }
    cfg.upsert_site(SiteProfile {
        id: "corp-dc".to_string(),
        name: "Corp DC".to_string(),
        site_url: dc_base.clone(),
        confluence_url: dc_base.clone(),
        deployment: DeploymentKind::DataCenter,
        cloud_id: None,
        account_id: "corp-dc-acct".to_string(),
        enabled: true,
        scopes: Vec::new(),
        confluence_account_id: None,
    });
    cfg.default_site = Some("acme-cloud".to_string());

    // 2. Build SiteClientPool with OAuth gateway + token refresh endpoints
    let pool = SiteClientPool::from_config_with_options(
        &cfg,
        Some(cred_path.clone()),
        Some(oauth_base.clone()),
        Some(oauth_base.clone()),
    )
    .expect("build SiteClientPool");
    assert_eq!(pool.enabled_clients().len(), 3);

    // 3. Parallel multi-site sync into SQLite v2
    let storage = Storage::open_in_memory().expect("in-memory SQLite v2");
    for site in cfg.effective_sites() {
        storage
            .upsert_site(&SiteRecord {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment: site.deployment.as_str().to_string(),
                enabled: site.enabled,
                last_synced_at: None,
                last_error: None,
            })
            .unwrap();
    }

    let targets: Vec<SiteSyncTarget> = pool
        .enabled_clients()
        .into_iter()
        .map(|(site, client)| SiteSyncTarget {
            site_id: site.id.clone(),
            site_name: site.display_name().to_string(),
            client,
        })
        .collect();

    let mut breakers = CircuitBreakerMap::new();
    let outcome = sync::run_multi_site_sync(&targets, &storage, &mut breakers, 4, None).await;
    assert_eq!(outcome.total_sites, 3, "outcome: {outcome:?}");
    assert_eq!(outcome.succeeded_sites, 3, "outcome: {outcome:?}");

    // Verify OAuth 3LO gateway routing + 401 automatic refresh + rotated refresh token persistence
    assert!(
        oauth_state.gateway_hits_acme.load(Ordering::SeqCst) >= 1,
        "expected OAuth gateway hits for acme-cloud"
    );
    assert!(
        oauth_state.gateway_hits_side.load(Ordering::SeqCst) >= 1,
        "expected OAuth gateway hits for side-cloud"
    );
    assert!(
        oauth_state.refresh_calls.load(Ordering::SeqCst) >= 1,
        "expected automatic 401 OAuth token refresh"
    );
    let refreshed_creds = oauth_auth.load_credentials().unwrap().unwrap();
    assert_eq!(refreshed_creds.oauth_access_token(), Some("oauth-tok-v2"));
    match refreshed_creds {
        StoredCredentials::CloudOAuth { refresh_token, .. } => {
            assert_eq!(refresh_token.as_deref(), Some("oauth-ref-v2"));
        }
        other => panic!("expected CloudOAuth credentials, got {other:?}"),
    }

    // 4. Verify zero key collision across acme-cloud and corp-dc for ENG / ENG-1
    let acme_projects = storage.list_projects_for_site("acme-cloud").unwrap();
    let dc_projects = storage.list_projects_for_site("corp-dc").unwrap();
    assert_eq!(acme_projects.len(), 1);
    assert_eq!(dc_projects.len(), 1);
    assert_eq!(acme_projects[0].key, "ENG");
    assert_eq!(acme_projects[0].name, "Acme Engineering");
    assert_eq!(dc_projects[0].key, "ENG");
    assert_eq!(dc_projects[0].name, "Corp DC Engineering");

    let acme_issues = storage.list_issues_for_site("acme-cloud", None, 20).unwrap();
    let dc_issues = storage.list_issues_for_site("corp-dc", None, 20).unwrap();
    assert_eq!(acme_issues[0].key, "ENG-1");
    assert_eq!(acme_issues[0].summary, "Acme Cloud Gateway Migration");
    assert_eq!(dc_issues[0].key, "ENG-1");
    assert_eq!(dc_issues[0].summary, "Corp DC Gateway Migration");

    // 5. Verify federated unified_search_scoped returns BOTH ENG-1 hits with distinct site_ids!
    let federated_hits = storage
        .unified_search_scoped("Gateway Migration", None, 20)
        .expect("federated unified_search_scoped");
    let eng1_sites: Vec<&str> = federated_hits
        .iter()
        .filter(|(_, h)| h.id_or_key == "ENG-1")
        .map(|(sid, _)| sid.as_str())
        .collect();
    assert_eq!(eng1_sites.len(), 2, "expected 2 distinct ENG-1 hits, got {federated_hits:?}");
    assert!(eng1_sites.contains(&"acme-cloud"));
    assert!(eng1_sites.contains(&"corp-dc"));

    let acme_only_hits = storage
        .unified_search_scoped("Gateway Migration", Some("acme-cloud"), 20)
        .expect("scoped search");
    assert_eq!(acme_only_hits.len(), 1);
    assert_eq!(acme_only_hits[0].0, "acme-cloud");

    // 6. Verify HTTP GET /api/v1/sites & ?site= scoping + MCP list_sites & site argument
    let default_client = pool.client_for("acme-cloud").unwrap();
    let api_router = serve::build_router(cfg.clone(), default_client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;

    let sites_resp: Value = http
        .get(format!("{api_base}/api/v1/sites"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sites_resp["sites"].as_array().unwrap().len(), 3);

    let dc_search_resp: Value = http
        .get(format!("{api_base}/search?q=Gateway&site=corp-dc"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dc_hits = dc_search_resp["hits"].as_array().unwrap();
    assert!(!dc_hits.is_empty());
    assert!(dc_hits.iter().all(|h| h["site_id"] == "corp-dc"));

    let mcp_sites = mcp::handle_rpc_line(
        &cfg,
        &default_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": { "name": "list_sites", "arguments": {} }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_sites["result"]["isError"], false);
    let mcp_sites_payload: Value =
        serde_json::from_str(mcp_sites["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_sites_payload["sites"].as_array().unwrap().len(), 3);

    let mcp_scoped_search = mcp::handle_rpc_line(
        &cfg,
        &default_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": {
                "name": "search_all",
                "arguments": { "query": "Gateway Migration", "site": "acme-cloud" }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_scoped_search["result"]["isError"], false);
    let mcp_search_payload: Value =
        serde_json::from_str(mcp_scoped_search["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    let mcp_hits = mcp_search_payload["hits"].as_array().unwrap();
    assert_eq!(mcp_hits.len(), 1);
    assert_eq!(mcp_hits[0]["site_id"], "acme-cloud");
    assert_eq!(mcp_hits[0]["id_or_key"], "ENG-1");
}

#[tokio::test]
async fn e2e_tui_request_issue_detail_fetches_remotelink_once_across_cache_miss_and_reopen() {
    use atlassian_cli::{
        config::Persona,
        sync,
        ui::app::{AppState, UiMsg, UiState},
    };

    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    {
        let mut issues = state.issues.write().unwrap();
        let rec = issues.get_mut("PROJ-101").unwrap();
        rec.description_cloud_adf = json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Cached v1 body — see https://acme.atlassian.net/wiki/spaces/ENG/pages/2002/Rollback"
                }]
            }]
        });
    }
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let auth = AuthManager::with_credentials(
        "tui-detail-e2e",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth.clone())
        .expect("build cloud client");
    let storage = Storage::open_in_memory().expect("open in-memory storage");
    let (core_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    async fn next_issue_detail(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiMsg>,
    ) -> UiMsg {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(msg @ UiMsg::IssueDetailLoaded { .. }) => return msg,
                    Some(_) => continue,
                    None => panic!("ui_rx closed before IssueDetailLoaded"),
                }
            }
        })
        .await
        .expect("timed out waiting for IssueDetailLoaded")
    }

    // 1. Cache miss: active_issue stays empty until network load finishes with 1 /remotelink call.
    app.request_issue_detail(&mut ui, "PROJ-101");
    assert!(app.active_issue.is_none());

    let msg = next_issue_detail(&mut ui_rx).await;
    app.handle_ui_msg(&mut ui, msg);

    assert_eq!(state.remotelink_requests.load(Ordering::SeqCst), 1);
    let active = app.active_issue.as_ref().expect("active_issue populated");
    assert!(active.description_markdown.contains("Cached v1 body"));
    assert!(active.linked_pages.iter().any(|p| p.page_id == "2001" && p.title == "Release Guide"));
    assert!(active.linked_pages.iter().any(|p| p.page_id == "2002"));
    let cached = storage.get_issue_detail("PROJ-101").unwrap().expect("cached detail");
    assert_eq!(cached.linked_pages.len(), 2);

    // 2. Cache hit reopen: cached v1 shows immediately, then fresh v2 replaces it with 1 more /remotelink call.
    {
        let mut issues = state.issues.write().unwrap();
        let rec = issues.get_mut("PROJ-101").unwrap();
        rec.description_cloud_adf = json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Fresh v2 body — see https://acme.atlassian.net/wiki/spaces/ENG/pages/2002/Rollback"
                }]
            }]
        });
    }
    app.active_issue = None;
    app.request_issue_detail(&mut ui, "PROJ-101");
    assert!(
        app.active_issue
            .as_ref()
            .expect("cached detail shown immediately")
            .description_markdown
            .contains("Cached v1 body")
    );

    let msg2 = next_issue_detail(&mut ui_rx).await;
    app.handle_ui_msg(&mut ui, msg2);

    assert_eq!(state.remotelink_requests.load(Ordering::SeqCst), 2);
    let refreshed = app.active_issue.as_ref().expect("refreshed active_issue");
    assert!(refreshed.description_markdown.contains("Fresh v2 body"));
    assert!(refreshed.linked_pages.iter().any(|p| p.page_id == "2001"));
    assert!(refreshed.linked_pages.iter().any(|p| p.page_id == "2002"));
    assert!(
        storage
            .get_issue_detail("PROJ-101")
            .unwrap()
            .expect("updated cache")
            .description_markdown
            .contains("Fresh v2 body")
    );
}

