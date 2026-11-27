use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{AppConfig, DeploymentKind},
    serve,
    storage::Storage,
};

use crate::server::{ensure_isolated_env, spawn_server};

/// Data Center mock that pages every collection by offset and never serves more than
/// "page_cap" items per page, whatever the client asks for. With "fail_after_first_page"
/// set, every request past the first page fails with 400 (not retried by the client).
struct PagedDcMock {
    page_cap: usize,
    fail_after_first_page: std::sync::atomic::AtomicBool,
    spaces: Vec<Value>,
    board_issues: RwLock<Vec<Value>>,
    my_issues: Vec<Value>,
    root_pages: Vec<Value>,
    child_pages: Vec<Value>,
    issue_comments: Vec<Value>,
    page_comments: Vec<Value>,
    page_attachments: Vec<Value>,
}

impl PagedDcMock {
    fn new(page_cap: usize) -> Self {
        let issue = |i: usize, prefix: &str| {
            json!({
                "id": format!("{i}"),
                "key": format!("{prefix}-{i}"),
                "fields": {
                    "project": { "key": prefix, "name": prefix },
                    "summary": format!("{prefix} issue {i}"),
                    "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "updated": "2026-11-20T10:00:00.000+0000"
                }
            })
        };
        let page = |id: usize, parent: Option<usize>| {
            let mut p = json!({
                "id": format!("{id}"),
                "type": "page",
                "title": format!("Page {id}"),
                "space": { "key": "S001" },
                "version": { "number": 1 }
            });
            if let Some(parent) = parent {
                p["ancestors"] = json!([{ "id": format!("{parent}") }]);
            }
            p
        };
        let mut spaces: Vec<Value> = (1..120)
            .map(|i| json!({ "id": i, "key": format!("S{i:03}"), "name": format!("Space {i:03}"), "type": "global" }))
            .collect();
        spaces.push(json!({ "id": 120, "key": "~zed", "name": "Zed", "type": "personal" }));
        Self {
            page_cap,
            fail_after_first_page: std::sync::atomic::AtomicBool::new(false),
            spaces,
            board_issues: RwLock::new((1..=130).map(|i| issue(i, "BRD")).collect()),
            my_issues: (1..=120).map(|i| issue(i, "MY")).collect(),
            root_pages: (1000..1120).map(|id| page(id, None)).collect(),
            child_pages: (2000..2120).map(|id| page(id, Some(1000))).collect(),
            issue_comments: (1..=130)
                .map(|i| json!({ "id": format!("{i}"), "body": format!("comment {i}"), "author": { "displayName": "A" }, "created": "2026-11-20T10:00:00.000+0000" }))
                .collect(),
            page_comments: (1..=60)
                .map(|i| json!({ "id": format!("c{i}"), "type": "comment", "title": "Re", "body": { "storage": { "value": format!("<p>c {i}</p>") } }, "version": { "number": 1 } }))
                .collect(),
            page_attachments: (1..=60)
                .map(|i| json!({ "id": format!("att{i}"), "type": "attachment", "title": format!("f{i}.txt"), "metadata": { "mediaType": "text/plain" }, "extensions": { "fileSize": 1 }, "_links": { "download": format!("/download/f{i}.txt") } }))
                .collect(),
        }
    }

    /// The slice of "items" a server capped at "page_cap" returns for (start, requested).
    fn slice(
        &self,
        items: &[Value],
        start: usize,
        requested: usize,
    ) -> Result<(Vec<Value>, usize, bool), Box<Response>> {
        if start > 0 && self.fail_after_first_page.load(Ordering::SeqCst) {
            return Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    "injected failure past the first page",
                )
                    .into_response(),
            ));
        }
        let max = requested.min(self.page_cap).max(1);
        let slice: Vec<Value> = items.iter().skip(start).take(max).cloned().collect();
        let more = start + slice.len() < items.len();
        Ok((slice, max, more))
    }

    /// Jira offset envelope ("startAt"/"maxResults"/"total"), maxResults as the server applied it.
    fn jira_page(&self, items: &[Value], field: &str, q: &HashMap<String, String>) -> Response {
        let start = q.get("startAt").and_then(|s| s.parse().ok()).unwrap_or(0);
        let requested = q.get("maxResults").and_then(|s| s.parse().ok()).unwrap_or(50);
        match self.slice(items, start, requested) {
            Ok((slice, max, _)) => {
                Json(json!({ "startAt": start, "maxResults": max, "total": items.len(), field: slice })).into_response()
            }
            Err(resp) => *resp,
        }
    }

    /// Confluence offset envelope with "_links.next" while more remain.
    fn confluence_page(&self, items: &[Value], path: &str, q: &HashMap<String, String>) -> Response {
        let start = q.get("start").and_then(|s| s.parse().ok()).unwrap_or(0);
        let requested = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(25);
        match self.slice(items, start, requested) {
            Ok((slice, max, more)) => {
                let mut body = json!({ "results": slice, "start": start, "limit": max, "size": slice.len() });
                if more {
                    body["_links"] = json!({ "next": format!("{path}?limit={max}&start={}", start + max) });
                }
                Json(body).into_response()
            }
            Err(resp) => *resp,
        }
    }
}

fn build_paged_dc_router(state: Arc<PagedDcMock>) -> Router {
    type Q = Query<HashMap<String, String>>;
    Router::new()
        .route("/rest/api/2/project", get(|| async { Json(json!([])) }))
        .route(
            "/rest/agile/1.0/board",
            get(|| async {
                Json(json!({ "startAt": 0, "maxResults": 50, "total": 1, "isLast": true,
                    "values": [{ "id": 1, "name": "Paged board", "type": "kanban" }] }))
            }),
        )
        .route(
            "/rest/agile/1.0/board/1/issue",
            get({
                let s = state.clone();
                move |Query(q): Q| {
                    let s = s.clone();
                    async move {
                        let items = s.board_issues.read().unwrap().clone();
                        s.jira_page(&items, "issues", &q)
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/search",
            post({
                let s = state.clone();
                move |Json(body): Json<Value>| {
                    let s = s.clone();
                    async move {
                        let mut q = HashMap::new();
                        q.insert("startAt".to_string(), body["startAt"].as_u64().unwrap_or(0).to_string());
                        q.insert("maxResults".to_string(), body["maxResults"].as_u64().unwrap_or(50).to_string());
                        s.jira_page(&s.my_issues, "issues", &q)
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/issue/{key}/comment",
            get({
                let s = state.clone();
                move |Query(q): Q| {
                    let s = s.clone();
                    async move { s.jira_page(&s.issue_comments, "comments", &q) }
                }
            }),
        )
        .route(
            "/rest/api/space",
            get({
                let s = state.clone();
                move |Query(q): Q| {
                    let s = s.clone();
                    async move { s.confluence_page(&s.spaces, "/rest/api/space", &q) }
                }
            }),
        )
        .route(
            "/rest/api/space/{key}/content/page",
            get({
                let s = state.clone();
                move |Path(key): Path<String>, Query(q): Q| {
                    let s = s.clone();
                    async move {
                        s.confluence_page(&s.root_pages, &format!("/rest/api/space/{key}/content/page"), &q)
                    }
                }
            }),
        )
        .route(
            "/rest/api/content/{id}/child/{kind}",
            get({
                let s = state.clone();
                move |Path((id, kind)): Path<(String, String)>, Query(q): Q| {
                    let s = s.clone();
                    async move {
                        let path = format!("/rest/api/content/{id}/child/{kind}");
                        match kind.as_str() {
                            "page" => s.confluence_page(&s.child_pages, &path, &q),
                            "comment" => s.confluence_page(&s.page_comments, &path, &q),
                            "attachment" => s.confluence_page(&s.page_attachments, &path, &q),
                            _ => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }
            }),
        )
}

fn paged_dc_client(base_url: &str) -> AtlassianClient {
    let auth = AuthManager::with_credentials(
        "paged-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.to_string(),
            token: "tok".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    AtlassianClient::with_base_urls(base_url, base_url, DeploymentKind::DataCenter, auth).unwrap()
}

#[tokio::test]
async fn regression_known_1_sync_loads_all_spaces_past_50() {
    use atlassian_cli::sync;

    ensure_isolated_env();
    let base_url = spawn_server(build_paged_dc_router(Arc::new(PagedDcMock::new(50)))).await;
    let client = paged_dc_client(&base_url);
    let storage = Storage::open_in_memory().unwrap();
    sync::bootstrap_workspace_for_site("default", &client, &storage)
        .await
        .expect("bootstrap");

    let spaces = storage.list_spaces_for_site("default").unwrap();
    assert_eq!(spaces.len(), 120, "every space past the first page must be cached");
    assert!(spaces.iter().any(|s| s.key == "~zed"), "the last (personal) space must be cached");
}

#[tokio::test]
async fn regression_atl_01_board_load_does_not_prune_beyond_first_page() {
    use atlassian_cli::sync;

    ensure_isolated_env();
    let state = Arc::new(PagedDcMock::new(50));
    let base_url = spawn_server(build_paged_dc_router(state.clone())).await;
    let client = paged_dc_client(&base_url);
    let storage = Storage::open_in_memory().unwrap();

    // The function the TUI board load (request_board_load) and "atlassian board" call.
    let loaded = sync::load_board_issues_for_site(&client, &storage, "default", 1, None)
        .await
        .unwrap();
    assert_eq!(loaded.len(), 130);
    assert_eq!(
        storage.list_board_issues_for_site("default", 1, None, 1000).unwrap().len(),
        130,
        "issues past the first server page must stay on the board"
    );

    // One issue leaves the board on the server; a complete reload drops only that one.
    state.board_issues.write().unwrap().retain(|i| i["key"] != "BRD-77");
    sync::load_board_issues_for_site(&client, &storage, "default", 1, None)
        .await
        .unwrap();
    let board = storage.list_board_issues_for_site("default", 1, None, 1000).unwrap();
    assert_eq!(board.len(), 129);
    assert!(!board.iter().any(|i| i.key == "BRD-77"));
}

#[tokio::test]
async fn regression_atl_12_sync_walks_all_pages_for_my_issues_board_project_and_root_pages() {
    use atlassian_cli::sync;

    ensure_isolated_env();
    let state = Arc::new(PagedDcMock::new(50));
    let base_url = spawn_server(build_paged_dc_router(state.clone())).await;
    let client = paged_dc_client(&base_url);
    let storage = Storage::open_in_memory().unwrap();
    sync::bootstrap_workspace_for_site("default", &client, &storage)
        .await
        .expect("bootstrap");

    let my = storage.list_issues_for_site("default", Some("MY"), 1000).unwrap();
    assert_eq!(my.len(), 120, "my open issues past the first page");
    assert_eq!(storage.list_board_issues_for_site("default", 1, None, 1000).unwrap().len(), 130);
    let roots = storage.list_space_root_pages_for_site("default", "S001").unwrap();
    assert_eq!(roots.len(), 120, "root pages past the first page");
    let children = sync::load_child_pages_for_site(&client, &storage, "default", "1000")
        .await
        .unwrap();
    assert_eq!(children.len(), 120, "child pages past the first page");

    // The board changes on the server and the walk then fails on page 2: the error surfaces
    // and the partial view must not prune the cached board.
    state.board_issues.write().unwrap().truncate(10);
    state.fail_after_first_page.store(true, Ordering::SeqCst);
    state.board_issues.write().unwrap().extend(
        (500..600).map(|i| json!({ "id": format!("{i}"), "key": format!("NEW-{i}"), "fields": { "summary": "new" } })),
    );
    let err = sync::load_board_issues_for_site(&client, &storage, "default", 1, None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("400") || format!("{err:#}").contains("injected"), "{err:#}");
    assert!(
        storage.list_board_issues_for_site("default", 1, None, 1000).unwrap().len() >= 130,
        "a failed walk must not prune the board"
    );
    assert!(sync::load_space_root_pages_for_site(&client, &storage, "default", "S001")
        .await
        .is_err());
    assert_eq!(
        storage.list_space_root_pages_for_site("default", "S001").unwrap().len(),
        120,
        "a failed walk must not prune root pages"
    );
}

#[tokio::test]
async fn regression_atl_14_comments_and_attachments_beyond_first_page() {
    ensure_isolated_env();
    let base_url = spawn_server(build_paged_dc_router(Arc::new(PagedDcMock::new(50)))).await;
    let client = paged_dc_client(&base_url);

    let (comments, total) = client.list_issue_comments_collected("BRD-1").await.unwrap();
    assert_eq!(comments.items.len(), 130);
    assert!(comments.complete);
    assert_eq!(total, Some(130));
    assert_eq!(client.list_issue_comments("BRD-1").await.unwrap().len(), 130);

    assert_eq!(client.list_page_attachments("1000").await.unwrap().len(), 60);
    assert_eq!(client.list_page_comments("1000").await.unwrap().len(), 60);
}

#[tokio::test]
async fn regression_atl_15_http_lists_do_not_prune_on_server_capped_page() {
    use atlassian_cli::sync;

    ensure_isolated_env();
    let base_url = spawn_server(build_paged_dc_router(Arc::new(PagedDcMock::new(25)))).await;
    let client = paged_dc_client(&base_url);
    let storage = Storage::open_in_memory().unwrap();
    // Cache the whole board first (the walk follows 25-item pages to the end).
    sync::load_board_issues_for_site(&client, &storage, "default", 1, None)
        .await
        .unwrap();
    assert_eq!(storage.list_board_issues_for_site("default", 1, None, 1000).unwrap().len(), 130);

    let cfg = AppConfig {
        site_url: base_url.clone(),
        confluence_url: base_url.clone(),
        deployment: DeploymentKind::DataCenter,
        ..Default::default()
    };
    let serve_url =
        spawn_server(serve::build_router(cfg, client, storage, serve::ServeOptions::default())).await;
    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        async move { http.get(path).send().await.unwrap().json::<Value>().await.unwrap() }
    };

    // The server answers 25 of the 50 asked for: that is a page, not the end of the board.
    let page = get(format!("{serve_url}/boards/1/issues?limit=50")).await;
    assert_eq!(page["issues"].as_array().unwrap().len(), 25);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["next_cursor"], "25");
    let next = get(format!("{serve_url}/boards/1/issues?limit=50&cursor=25")).await;
    assert_eq!(next["issues"][0]["key"], "BRD-26");
    let cached = get(format!("{serve_url}/boards/1/issues?offline=true&limit=1000")).await;
    assert_eq!(cached["issues"].as_array().unwrap().len(), 130, "the 26th and later cached issues stay");
    assert_eq!(cached["has_more"], false);

    // Root and child pages page the same way.
    let roots = get(format!("{serve_url}/spaces/S001/pages?limit=50")).await;
    assert_eq!(roots["pages"].as_array().unwrap().len(), 25);
    assert_eq!(roots["has_more"], true);
    let children = get(format!("{serve_url}/pages/1000/children?limit=50")).await;
    assert_eq!(children["pages"].as_array().unwrap().len(), 25);
    assert_eq!(children["has_more"], true);
    let bad = http
        .get(format!("{serve_url}/pages/1000/children?cursor=abc"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "a non-offset Data Center cursor is a bad request");

    // A small refresh still caches the whole catalog.
    let small = get(format!("{serve_url}/spaces?refresh=true&limit=10")).await;
    assert_eq!(small["spaces"].as_array().unwrap().len(), 10);
    assert_eq!(small["has_more"], true);
    let later = get(format!("{serve_url}/spaces?limit=50")).await;
    assert_eq!(later["spaces"].as_array().unwrap().len(), 50);
    let last = get(format!("{serve_url}/spaces?limit=50&cursor=100")).await;
    assert_eq!(last["spaces"].as_array().unwrap().len(), 20);
    assert_eq!(last["has_more"], false);
}


/// ATL-25: "assign to me" on a row from site B must send site B's own account
/// id, never the primary site's cached profile.
#[tokio::test]
async fn regression_atl_25_assign_to_me_uses_target_site_identity() {
    use atlassian_cli::{
        atlassian::SiteClientPool,
        config::{Persona, SiteProfile},
        models::UserSummary,
        sync,
        ui::app::{AppState, UiMsg, UiState},
    };

    ensure_isolated_env();

    #[derive(Default)]
    struct SiteMock {
        me: String,
        myself_calls: AtomicUsize,
        assigned: RwLock<Vec<(String, Value)>>,
    }
    fn site_router(st: Arc<SiteMock>) -> Router {
        Router::new()
            .route(
                "/rest/api/3/myself",
                get(|State(st): State<Arc<SiteMock>>| async move {
                    st.myself_calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "accountId": st.me,
                        "displayName": format!("User {}", st.me),
                        "active": true
                    }))
                }),
            )
            .route(
                "/rest/api/3/issue/{key}/assignee",
                put(
                    |State(st): State<Arc<SiteMock>>,
                     Path(key): Path<String>,
                     Json(body): Json<Value>| async move {
                        st.assigned.write().unwrap().push((key, body));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(st)
    }

    let site_a = Arc::new(SiteMock {
        me: "acct-a".into(),
        ..Default::default()
    });
    let site_b = Arc::new(SiteMock {
        me: "acct-b".into(),
        ..Default::default()
    });
    let base_a = spawn_server(site_router(site_a.clone())).await;
    let base_b = spawn_server(site_router(site_b.clone())).await;

    let sites: Vec<SiteProfile> = [("site-a", &base_a), ("site-b", &base_b)]
        .into_iter()
        .map(|(id, url)| SiteProfile {
            id: id.into(),
            name: id.to_uppercase(),
            site_url: url.clone(),
            deployment: DeploymentKind::Cloud,
            account_id: "default".into(),
            enabled: true,
            ..Default::default()
        })
        .collect();
    let cfg = AppConfig {
        site_url: base_a.clone(),
        deployment: DeploymentKind::Cloud,
        sites: sites.clone(),
        default_site: Some("site-a".into()),
        active_site: Some("site-a".into()),
        ..Default::default()
    };
    let auth = AuthManager::with_credentials(
        "default",
        StoredCredentials::ApiToken {
            site_url: base_a.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    // Built like the TUI does: the primary client carries the default site's id.
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).expect("primary client");
    assert_eq!(client.site_id(), "site-a");
    let storage = Storage::open_in_memory().expect("storage");
    let (core_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth.clone(),
        client,
        storage,
        sync_ctrl,
        ui_tx,
    );
    let pool_auth = auth.clone();
    app.site_pool = SiteClientPool::new(sites, move |_| pool_auth.clone(), None, None)
        .expect("pool");
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
    // The primary site's profile is known; site B's is not.
    app.me = Some(UserSummary {
        id: "acct-a".into(),
        display_name: "Alice (site A)".into(),
        ..Default::default()
    });

    async fn wait_assign_result(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiMsg>) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(UiMsg::IssueMutated { .. } | UiMsg::IssueAssignFailed { .. }) => return,
                    Some(_) => continue,
                    None => panic!("ui_rx closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for the assign result");
    }

    app.request_assign_to_me_for_site(&mut ui, Some("site-b"), "B-1");
    wait_assign_result(&mut ui_rx).await;
    {
        let assigned = site_b.assigned.read().unwrap();
        assert_eq!(assigned.len(), 1, "site B received the assignment");
        assert_eq!(assigned[0].0, "B-1");
        assert_eq!(assigned[0].1["accountId"], "acct-b", "site B's own identity is sent");
    }
    assert_eq!(site_b.myself_calls.load(Ordering::SeqCst), 1);
    assert!(site_a.assigned.read().unwrap().is_empty());

    // The primary site keeps using the already-known profile without a lookup.
    app.request_assign_to_me_for_site(&mut ui, Some("site-a"), "A-1");
    wait_assign_result(&mut ui_rx).await;
    let assigned_a = site_a.assigned.read().unwrap();
    assert_eq!(assigned_a.len(), 1);
    assert_eq!(assigned_a[0].1["accountId"], "acct-a");
    assert_eq!(site_a.myself_calls.load(Ordering::SeqCst), 0);
}
