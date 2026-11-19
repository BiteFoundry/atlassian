//! Local HTTP JSON API (`jira serve` / `confluence serve`, default `127.0.0.1:8789`) powered by `axum`.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::cors::CorsLayer;

use crate::{
    atlassian::{AtlassianClient, SiteClientPool},
    config::AppConfig,
    models::{
        FederatedSearchHit, IssueSummary, IssueUpdate, OutgoingIssue, OutgoingPage, PageSummary,
        PageUpdate, SearchHitKind, UnifiedSearchHit,
    },
    storage::{Storage, DEFAULT_SITE_ID},
    sync::{self, SiteSyncTarget, SyncCommand},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: crate::config::DEFAULT_SERVE_PORT,
            token: None,
        }
    }
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub client: AtlassianClient,
    pub site_pool: SiteClientPool,
    pub storage: Storage,
    pub token: Option<String>,
}

/// JSON error returned by every failing handler.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub deployment: String,
    pub site_url: String,
    pub confluence_url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SiteQuery {
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BoardsQuery {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SprintsQuery {
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BoardIssuesQuery {
    #[serde(default)]
    pub sprint_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IssuesQuery {
    #[serde(default)]
    pub jql: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub page_token: Option<String>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct OfflineQuery {
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateIssueBody {
    pub project_key: String,
    #[serde(default = "default_task_type")]
    pub issue_type: String,
    pub summary: String,
    #[serde(default)]
    pub description_markdown: String,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub assignee_id: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub parent_key: Option<String>,
}

fn default_task_type() -> String {
    "Task".to_string()
}

#[derive(Debug, Deserialize)]
pub struct TransitionBody {
    pub transition: String,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssigneeBody {
    #[serde(default)]
    pub assignee_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommentBody {
    pub body_markdown: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpacesQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpacePagesQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePageBody {
    pub space_id_or_key: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub body_markdown: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePageBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body_markdown: Option<String>,
    #[serde(default)]
    pub prev_version: i32,
    #[serde(default)]
    pub version_message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub site: Option<String>,
}

/// Build the `axum::Router` for the unified Jira & Confluence HTTP JSON API.
pub fn build_router(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    token: Option<String>,
) -> Router {
    let site_pool = SiteClientPool::from_config(&config).unwrap_or_else(|_| {
        let auth = client.auth().clone();
        SiteClientPool::new(Vec::new(), move |_| auth.clone(), None, None)
            .expect("empty site pool initialization")
    });
    let state = Arc::new(AppState {
        config,
        client,
        site_pool,
        storage,
        token,
    });

    Router::new()
        .route("/health", get(health))
        .route("/api/v1/sites", get(list_sites))
        .route("/sites", get(list_sites))
        .route("/myself", get(get_myself))
        .route("/projects", get(list_projects))
        .route("/boards", get(list_boards))
        .route("/boards/{board_id}/sprints", get(list_board_sprints))
        .route("/boards/{board_id}/issues", get(list_board_issues))
        .route("/issues", get(list_issues).post(create_issue))
        .route(
            "/issues/{key}",
            get(get_issue).patch(patch_issue).delete(delete_issue),
        )
        .route(
            "/issues/{key}/transitions",
            get(list_transitions).post(transition_issue),
        )
        .route("/issues/{key}/assignee", put(assign_issue))
        .route(
            "/issues/{key}/comments",
            get(list_issue_comments).post(add_issue_comment),
        )
        .route("/spaces", get(list_spaces))
        .route("/spaces/{space}/pages", get(list_space_pages))
        .route("/pages", post(create_page))
        .route(
            "/pages/{id}",
            get(get_page).put(update_page).delete(delete_page),
        )
        .route("/pages/{id}/children", get(list_child_pages))
        .route(
            "/pages/{id}/comments",
            get(list_page_comments).post(add_page_comment),
        )
        .route("/search", get(search))
        .route("/sync", post(sync_workspace))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the HTTP JSON API server and background sync engine until stopped.
pub async fn run(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), client.clone(), storage.clone(), event_tx);
    sync_ctl.request(SyncCommand::RefreshAll);

    let app = build_router(config, client, storage, opts.token);

    let addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("잘못된 바인드 주소: {}:{}", opts.bind, opts.port))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("리스너 바인드 실패: {addr}"))?;

    println!("atlassian HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("HTTP 서버 오류")?;

    Ok(())
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(expected) = &state.token {
        if request.uri().path() != "/health" {
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));

            if presented != Some(expected.as_str()) {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                ));
            }
        }
    }

    Ok(next.run(request).await)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        deployment: state.config.deployment.as_str().to_string(),
        site_url: state.config.site_url.clone(),
        confluence_url: state.config.effective_confluence_root(),
    })
}

fn resolve_site_id(state: &AppState, raw_site: Option<&str>) -> Option<String> {
    let trimmed = raw_site.map(str::trim).filter(|s| !s.is_empty())?;
    if trimmed.eq_ignore_ascii_case("all") {
        return None;
    }
    if let Some(found) = state.config.find_effective_site(trimmed) {
        return Some(found.id);
    }
    Some(trimmed.to_string())
}

fn resolve_site_client(state: &AppState, raw_site: Option<&str>) -> Result<(String, AtlassianClient), ApiError> {
    if let Some(site_id) = resolve_site_id(state, raw_site) {
        if site_id.eq_ignore_ascii_case(state.client.site_id()) {
            return Ok((site_id, state.client.clone()));
        }
        if let Ok(client) = state.site_pool.client_for(&site_id) {
            return Ok((site_id, client));
        }
        if site_id.eq_ignore_ascii_case(DEFAULT_SITE_ID) {
            return Ok((site_id, state.client.clone()));
        }
        return Err(ApiError::not_found(format!("site not found: '{site_id}'")));
    }
    Ok((state.client.site_id().to_string(), state.client.clone()))
}

fn site_name_lookup(config: &AppConfig, site_id: &str) -> Option<String> {
    config
        .find_effective_site(site_id)
        .map(|s| s.display_name().to_string())
}

async fn list_sites(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let mut sites_out = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for site in state.config.effective_sites() {
        seen_ids.insert(site.id.to_ascii_lowercase());
        let counts = state
            .storage
            .cache_counts_for_site(&site.id)
            .unwrap_or_default();
        let db_rec = state.storage.get_site(&site.id).ok().flatten();
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

    if let Ok(db_sites) = state.storage.list_sites() {
        for rec in db_sites {
            if seen_ids.insert(rec.site_id.to_ascii_lowercase()) {
                let counts = state
                    .storage
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

    Ok(Json(json!({
        "active_site": state.config.active_site,
        "default_site": state.config.default_site,
        "sites": sites_out,
    })))
}

async fn get_myself(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let (site_id, client) = resolve_site_client(&state, query.site.as_deref())?;
    let me = client.get_myself().await?;
    client.set_cached_me(me.clone());
    Ok(Json(json!({ "site_id": site_id, "user": me })))
}

async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshQuery>,
) -> Result<Json<Value>, ApiError> {
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let cached = match scoped_site.as_deref() {
        Some(sid) => state.storage.list_projects_for_site(sid)?,
        None => state.storage.list_projects()?,
    };
    let projects = if query.refresh || cached.is_empty() {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        match client.list_projects().await {
            Ok(fetched) => {
                let _ = state.storage.upsert_projects_for_site(&target_site, &fetched);
                let stored = state.storage.list_projects_for_site(&target_site)?;
                if stored.is_empty() {
                    fetched
                } else {
                    stored
                }
            }
            Err(err) if !query.refresh && !cached.is_empty() => {
                let _ = err;
                cached
            }
            Err(err) if !query.refresh => {
                if state.config.site_url.trim().is_empty() && state.config.sites.is_empty() {
                    cached
                } else {
                    return Err(err.into());
                }
            }
            Err(err) => return Err(err.into()),
        }
    } else {
        cached
    };

    Ok(Json(json!({ "site_id": scoped_site, "projects": projects })))
}

async fn list_boards(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BoardsQuery>,
) -> Result<Json<Value>, ApiError> {
    let project = query
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let cached = match scoped_site.as_deref() {
        Some(sid) => state.storage.list_boards_for_site(sid, project)?,
        None => state.storage.list_boards(project)?,
    };
    let boards = if query.refresh || cached.is_empty() {
        if !query.refresh && state.config.site_url.trim().is_empty() && state.config.sites.is_empty() {
            cached
        } else {
            let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
            let fetched = client.list_boards(project).await?;
            let _ = state.storage.upsert_boards_for_site(&target_site, &fetched);
            let stored = state.storage.list_boards_for_site(&target_site, project)?;
            if stored.is_empty() {
                fetched
            } else {
                stored
            }
        }
    } else {
        cached
    };

    Ok(Json(json!({ "site_id": scoped_site, "boards": boards })))
}

async fn list_board_sprints(
    State(state): State<Arc<AppState>>,
    Path(board_id): Path<i64>,
    Query(query): Query<SprintsQuery>,
) -> Result<Json<Value>, ApiError> {
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let sprints = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state.storage.list_sprints_for_site(sid, board_id)?,
            None => state.storage.list_sprints(board_id)?,
        }
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.list_board_sprints(board_id).await?;
        let _ = state.storage.upsert_sprints_for_site(&target_site, &fetched);
        fetched
    };

    Ok(Json(json!({ "site_id": scoped_site, "board_id": board_id, "sprints": sprints })))
}

async fn list_board_issues(
    State(state): State<Arc<AppState>>,
    Path(board_id): Path<i64>,
    Query(query): Query<BoardIssuesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let issues = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state
                .storage
                .list_board_issues_for_site(sid, board_id, query.sprint_id, limit)?,
            None => state
                .storage
                .list_board_issues(board_id, query.sprint_id, limit)?,
        }
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client
            .list_board_issues(board_id, query.sprint_id, limit)
            .await?;
        let _ = state.storage.upsert_issues_for_site(&target_site, &fetched);
        fetched
    };

    Ok(Json(json!({
        "site_id": scoped_site,
        "board_id": board_id,
        "sprint_id": query.sprint_id,
        "issues": issues,
    })))
}

async fn list_issues(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssuesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let project = query
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let scoped_site = resolve_site_id(&state, query.site.as_deref());

    if query.offline {
        let issues = match scoped_site.as_deref() {
            Some(sid) => state.storage.list_issues_for_site(sid, project, limit)?,
            None => state.storage.list_issues(project, limit)?,
        };
        let total = issues.len();
        return Ok(Json(json!({
            "site_id": scoped_site,
            "issues": issues,
            "next_page_token": null,
            "next_start_at": null,
            "total": total,
        })));
    }

    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let jql = match query.jql.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => raw.to_string(),
        None => match project.or(state.config.default_project.as_deref()) {
            Some(proj) if !proj.trim().is_empty() => {
                format!("project = \"{}\" ORDER BY updated DESC", escape_jql_str(proj.trim()))
            }
            _ => AtlassianClient::my_open_issues_jql().to_string(),
        },
    };

    let page = client
        .search_issues_jql(&jql, limit, query.page_token.as_deref(), None)
        .await?;
    let _ = state.storage.upsert_issues_for_site(&target_site, &page.issues);

    Ok(Json(json!({
        "site_id": target_site,
        "issues": page.issues,
        "next_page_token": page.next_page_token,
        "next_start_at": page.next_start_at,
        "total": page.total,
    })))
}

async fn create_issue(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateIssueBody>,
) -> Result<Json<Value>, ApiError> {
    let project_key = body.project_key.trim();
    if project_key.is_empty() {
        return Err(ApiError::bad_request("project_key must not be empty"));
    }
    let summary = body.summary.trim();
    if summary.is_empty() {
        return Err(ApiError::bad_request("summary must not be empty"));
    }
    let issue_type = if body.issue_type.trim().is_empty() {
        "Task".to_string()
    } else {
        body.issue_type.trim().to_string()
    };

    let outgoing = OutgoingIssue {
        project_key: project_key.to_string(),
        issue_type,
        summary: summary.to_string(),
        description_markdown: body.description_markdown,
        priority: body.priority.filter(|s| !s.trim().is_empty()),
        assignee_id: body.assignee_id.filter(|s| !s.trim().is_empty()),
        labels: body.labels,
        parent_key: body.parent_key.filter(|s| !s.trim().is_empty()),
    };

    let detail = state.client.create_issue(&outgoing).await?;
    let _ = state.storage.upsert_issue_detail(&detail);

    Ok(Json(json!({ "ok": true, "issue": detail })))
}

async fn get_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }

    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let detail = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state.storage.get_issue_detail_for_site(sid, key)?,
            None => state.storage.get_issue_detail(key)?,
        }
        .ok_or_else(|| ApiError::not_found(format!("issue not found in cache: {key}")))?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.get_issue(key).await?;
        let _ = state.storage.upsert_issue_detail_for_site(&target_site, &fetched);
        fetched
    };

    Ok(Json(json!({ "site_id": scoped_site, "issue": detail })))
}

async fn patch_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(patch): Json<IssueUpdate>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    if patch.summary.is_none()
        && patch.description_markdown.is_none()
        && patch.priority.is_none()
        && patch.assignee_id.is_none()
        && patch.labels.is_none()
    {
        return Err(ApiError::bad_request(
            "at least one field must be provided for issue update",
        ));
    }

    let detail = state.client.update_issue(key, &patch).await?;
    let _ = state.storage.upsert_issue_detail(&detail);

    Ok(Json(json!({ "ok": true, "issue": detail })))
}

async fn delete_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    state.client.delete_issue(key).await?;
    let _ = state.storage.delete_issue(key);
    Ok(Json(json!({ "ok": true, "key": key })))
}

async fn list_transitions(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let transitions = state.client.list_transitions(key).await?;
    Ok(Json(json!({ "key": key, "transitions": transitions })))
}

async fn transition_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<TransitionBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    let transition = body.transition.trim();
    if key.is_empty() || transition.is_empty() {
        return Err(ApiError::bad_request(
            "issue key and transition must not be empty",
        ));
    }
    state
        .client
        .transition_issue(key, transition, body.comment.as_deref())
        .await?;
    Ok(Json(json!({
        "ok": true,
        "key": key,
        "transition": transition,
    })))
}

async fn assign_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<AssigneeBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let resolved = match body.assignee_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) if raw.eq_ignore_ascii_case("me") => {
            let me = state.client.get_myself().await?;
            Some(me.id)
        }
        Some(raw) => Some(raw.to_string()),
    };

    state.client.assign_issue(key, resolved.as_deref()).await?;
    Ok(Json(json!({
        "ok": true,
        "key": key,
        "assignee_id": resolved,
    })))
}

async fn list_issue_comments(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    let comments = state.client.list_issue_comments(key).await?;
    Ok(Json(json!({ "key": key, "comments": comments })))
}

async fn add_issue_comment(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if body.body_markdown.trim().is_empty() {
        return Err(ApiError::bad_request("body_markdown must not be empty"));
    }
    let comment = state
        .client
        .add_issue_comment(key, &body.body_markdown)
        .await?;
    Ok(Json(json!({ "ok": true, "comment": comment })))
}

async fn list_spaces(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SpacesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let cached = match scoped_site.as_deref() {
        Some(sid) => state.storage.list_spaces_for_site(sid)?,
        None => state.storage.list_spaces()?,
    };
    let mut spaces = if query.refresh || cached.is_empty() {
        if !query.refresh && state.config.site_url.trim().is_empty() && state.config.sites.is_empty() {
            cached
        } else {
            let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
            let fetched = client.list_spaces(limit).await?;
            let _ = state.storage.upsert_spaces_for_site(&target_site, &fetched);
            let stored = state.storage.list_spaces_for_site(&target_site)?;
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
    Ok(Json(json!({ "site_id": scoped_site, "spaces": spaces })))
}

async fn list_space_pages(
    State(state): State<Arc<AppState>>,
    Path(space): Path<String>,
    Query(query): Query<SpacePagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let space = space.trim();
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let mut pages = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state.storage.list_space_root_pages_for_site(sid, space)?,
            None => state.storage.list_space_root_pages(space)?,
        }
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.list_space_root_pages(space, limit).await?;
        let _ = state.storage.upsert_pages_for_site(&target_site, &fetched);
        fetched
    };
    pages.truncate(limit);
    Ok(Json(json!({ "site_id": scoped_site, "space": space, "pages": pages })))
}

async fn get_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let detail = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state.storage.get_page_detail_for_site(sid, id)?,
            None => state.storage.get_page_detail(id)?,
        }
        .ok_or_else(|| ApiError::not_found(format!("page not found in cache: {id}")))?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.get_page(id).await?;
        let _ = state.storage.upsert_page_detail_for_site(&target_site, &fetched);
        fetched
    };
    Ok(Json(json!({ "site_id": scoped_site, "page": detail })))
}

async fn list_child_pages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let scoped_site = resolve_site_id(&state, query.site.as_deref());
    let pages = if query.offline {
        match scoped_site.as_deref() {
            Some(sid) => state.storage.list_child_pages_for_site(sid, id)?,
            None => state.storage.list_child_pages(id)?,
        }
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.list_child_pages(id).await?;
        let _ = state.storage.upsert_pages_for_site(&target_site, &fetched);
        fetched
    };
    Ok(Json(json!({ "site_id": scoped_site, "parent_id": id, "pages": pages })))
}

async fn create_page(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePageBody>,
) -> Result<Json<Value>, ApiError> {
    let space = body.space_id_or_key.trim();
    let title = body.title.trim();
    if space.is_empty() {
        return Err(ApiError::bad_request("space_id_or_key must not be empty"));
    }
    if title.is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }
    let req = OutgoingPage {
        space_id_or_key: space.to_string(),
        parent_id: body.parent_id.filter(|s| !s.trim().is_empty()),
        title: title.to_string(),
        body_markdown: body.body_markdown,
    };
    let detail = state.client.create_page(&req).await?;
    let _ = state.storage.upsert_page_detail(&detail);
    Ok(Json(json!({ "ok": true, "page": detail })))
}

async fn update_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdatePageBody>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if body.title.is_none() && body.body_markdown.is_none() {
        return Err(ApiError::bad_request(
            "at least one of title or body_markdown must be provided",
        ));
    }
    let need_current =
        body.prev_version <= 0 || body.title.is_none() || body.body_markdown.is_none();
    let current = if need_current {
        Some(state.client.get_page(id).await?)
    } else {
        None
    };

    let title = match body.title {
        Some(t) if !t.trim().is_empty() => t,
        Some(_) => return Err(ApiError::bad_request("title must not be empty")),
        None => current
            .as_ref()
            .map(|c| c.summary.title.clone())
            .unwrap_or_default(),
    };
    let body_markdown = match body.body_markdown {
        Some(b) => b,
        None => current
            .as_ref()
            .map(|c| c.body_markdown.clone())
            .unwrap_or_default(),
    };
    let prev_version = if body.prev_version > 0 {
        body.prev_version
    } else {
        current
            .as_ref()
            .map(|c| c.summary.version_number)
            .unwrap_or(1)
    };

    let patch = PageUpdate {
        title,
        body_markdown,
        prev_version,
        version_message: body.version_message,
    };
    let detail = state.client.update_page(id, &patch).await?;
    let _ = state.storage.upsert_page_detail(&detail);
    Ok(Json(json!({ "ok": true, "page": detail })))
}

async fn delete_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("page id must not be empty"));
    }
    state.client.delete_page(id).await?;
    let _ = state.storage.delete_page(id);
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn list_page_comments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let comments = state.client.list_page_comments(id).await?;
    Ok(Json(json!({ "page_id": id, "comments": comments })))
}

async fn add_page_comment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if body.body_markdown.trim().is_empty() {
        return Err(ApiError::bad_request("body_markdown must not be empty"));
    }
    let comment = state
        .client
        .add_page_comment(id, &body.body_markdown)
        .await?;
    Ok(Json(json!({ "ok": true, "comment": comment })))
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(ApiError::bad_request("query 'q' must not be empty"));
    }
    let scope = query
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("all")
        .to_ascii_lowercase();
    if !matches!(scope.as_str(), "all" | "jira" | "confluence") {
        return Err(ApiError::bad_request(format!(
            "invalid scope '{scope}' (expected all, jira, or confluence)"
        )));
    }
    let limit = clamp_limit(query.limit.unwrap_or(30));
    let scoped_site = resolve_site_id(&state, query.site.as_deref());

    let hits: Vec<FederatedSearchHit> = if !query.remote {
        let scoped_hits = match scope.as_str() {
            "jira" => state
                .storage
                .search_issues_scoped(q, scoped_site.as_deref(), limit)?,
            "confluence" => state
                .storage
                .search_pages_scoped(q, scoped_site.as_deref(), limit)?,
            _ => state
                .storage
                .unified_search_scoped(q, scoped_site.as_deref(), limit)?,
        };
        scoped_hits
            .into_iter()
            .map(|(sid, hit)| {
                let sname = site_name_lookup(&state.config, &sid);
                FederatedSearchHit::new(sid, sname, hit)
            })
            .collect()
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let target_name = site_name_lookup(&state.config, &target_site);
        let mut combined = Vec::new();
        if scope == "all" || scope == "jira" {
            let jql = if looks_like_query_expr(q) {
                q.to_string()
            } else {
                format!("text ~ \"{}\"", escape_jql_str(q))
            };
            let page = client.search_issues_jql(&jql, limit, None, None).await?;
            let _ = state
                .storage
                .upsert_issues_for_site(&target_site, &page.issues);
            combined.extend(page.issues.iter().map(|i| {
                FederatedSearchHit::new(target_site.clone(), target_name.clone(), issue_to_hit(i))
            }));
        }
        if scope == "all" || scope == "confluence" {
            let cql = if looks_like_query_expr(q) {
                q.to_string()
            } else {
                format!("text ~ \"{}\"", escape_jql_str(q))
            };
            let pages = client.search_confluence_cql(&cql, limit).await?;
            let _ = state.storage.upsert_pages_for_site(&target_site, &pages);
            combined.extend(pages.iter().map(|p| {
                FederatedSearchHit::new(target_site.clone(), target_name.clone(), page_to_hit(p))
            }));
        }
        combined.truncate(limit);
        combined
    };

    Ok(Json(json!({
        "query": q,
        "scope": scope,
        "site_id": scoped_site,
        "remote": query.remote,
        "hits": hits,
    })))
}

async fn sync_workspace(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.site.is_none() && state.site_pool.enabled_clients().len() > 1 {
        let targets: Vec<SiteSyncTarget> = state
            .site_pool
            .enabled_clients()
            .into_iter()
            .map(|(site, client)| SiteSyncTarget {
                site_id: site.id.clone(),
                site_name: site.display_name().to_string(),
                client,
            })
            .collect();
        let mut breakers = sync::CircuitBreakerMap::new();
        let outcome = sync::run_multi_site_sync(
            &targets,
            &state.storage,
            &mut breakers,
            sync::DEFAULT_MULTI_SITE_CONCURRENCY,
            None,
        )
        .await;
        let counts = state.storage.cache_counts()?;
        return Ok(Json(json!({
            "ok": outcome.succeeded_sites > 0,
            "summary": outcome.summary,
            "sites": outcome.per_site,
            "counts": counts,
        })));
    }
    let (_site_id, client) = resolve_site_client(&state, query.site.as_deref())?;
    let summary = sync::bootstrap_workspace(&client, &state.storage).await?;
    let counts = state.storage.cache_counts()?;
    Ok(Json(json!({
        "ok": true,
        "summary": summary,
        "counts": counts,
    })))
}

pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) fn escape_jql_str(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn looks_like_query_expr(raw: &str) -> bool {
    let upper = raw.to_ascii_uppercase();
    raw.contains('=')
        || raw.contains('~')
        || upper.contains(" AND ")
        || upper.contains(" OR ")
        || upper.contains(" ORDER BY ")
}

pub(crate) fn issue_to_hit(issue: &IssueSummary) -> UnifiedSearchHit {
    UnifiedSearchHit {
        kind: SearchHitKind::JiraIssue,
        id_or_key: issue.key.clone(),
        container: issue.project_key.clone(),
        title: issue.summary.clone(),
        snippet: issue
            .assignee
            .as_ref()
            .map(|a| format!("{} · {}", issue.issue_type.name, a.display_name))
            .unwrap_or_else(|| issue.issue_type.name.clone()),
        status_or_version: issue.status.name.clone(),
        updated_at: issue.updated_at.clone(),
        web_url: issue.web_url.clone(),
    }
}

pub(crate) fn page_to_hit(page: &PageSummary) -> UnifiedSearchHit {
    UnifiedSearchHit {
        kind: SearchHitKind::ConfluencePage,
        id_or_key: page.id.clone(),
        container: page.space_key.clone(),
        title: page.title.clone(),
        snippet: page
            .author_name
            .clone()
            .unwrap_or_else(|| page.space_key.clone()),
        status_or_version: format!("v{}", page.version_number),
        updated_at: page.updated_at.clone(),
        web_url: page.web_url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::DeploymentKind,
        models::{
            IssuePriority, IssueStatus, IssueType, ProjectSummary, SpaceSummary, StatusCategory,
        },
    };

    async fn spawn_test_server(token: Option<String>) -> (String, Storage) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".to_string(),
            deployment: DeploymentKind::Cloud,
            ..AppConfig::default()
        };
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router(cfg, client, storage.clone(), token);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{addr}"), storage)
    }

    #[tokio::test]
    async fn health_is_public_and_protected_routes_require_bearer_token() {
        let (base, storage) = spawn_test_server(Some("secret-token".to_string())).await;
        let _ = storage.upsert_projects(&[ProjectSummary {
            id: "1".to_string(),
            key: "ENG".to_string(),
            name: "Engineering".to_string(),
            project_type: "software".to_string(),
            ..Default::default()
        }]);
        let http = reqwest::Client::new();

        // /health must succeed without Authorization header
        let res = http.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: HealthResponse = res.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(body.deployment, "cloud");
        assert_eq!(body.site_url, "https://acme.atlassian.net");
        assert_eq!(body.confluence_url, "https://acme.atlassian.net/wiki");

        // Protected route without token -> 401 JSON error
        let res = http.get(format!("{base}/issues?offline=true")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let err: Value = res.json().await.unwrap();
        assert!(err["error"].as_str().is_some());

        // Protected route with wrong token -> 401
        let res = http
            .get(format!("{base}/issues?offline=true"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Protected route with valid token -> 200
        let res = http
            .get(format!("{base}/issues?offline=true"))
            .bearer_auth("secret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let data: Value = res.json().await.unwrap();
        assert!(data["issues"].is_array());
    }

    #[tokio::test]
    async fn validation_rejects_empty_bodies_and_bad_queries() {
        let (base, _storage) = spawn_test_server(None).await;
        let http = reqwest::Client::new();

        let res = http
            .post(format!("{base}/issues"))
            .json(&json!({ "project_key": "", "summary": "Title" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .patch(format!("{base}/issues/ENG-1"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .post(format!("{base}/issues/ENG-1/comments"))
            .json(&json!({ "body_markdown": "   " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .post(format!("{base}/pages"))
            .json(&json!({ "space_id_or_key": "ENG", "title": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .put(format!("{base}/pages/123"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .get(format!("{base}/search?q=%20%20"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = http
            .get(format!("{base}/search?q=release&scope=invalid"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn offline_routes_and_search_work_against_sqlite() {
        let (base, storage) = spawn_test_server(None).await;
        storage
            .upsert_issues(&[IssueSummary {
                id: "100".to_string(),
                key: "ENG-42".to_string(),
                project_key: "ENG".to_string(),
                summary: "Fix Korean FTS5 search".to_string(),
                status: IssueStatus {
                    id: "3".to_string(),
                    name: "In Progress".to_string(),
                    category: StatusCategory::InProgress,
                },
                priority: Some(IssuePriority {
                    id: "2".to_string(),
                    name: "High".to_string(),
                }),
                issue_type: IssueType {
                    id: "10001".to_string(),
                    name: "Bug".to_string(),
                    subtask: false,
                    epic: false,
                },
                web_url: "https://acme.atlassian.net/browse/ENG-42".to_string(),
                ..Default::default()
            }])
            .unwrap();
        storage
            .upsert_spaces(&[SpaceSummary {
                id: "sp-1".to_string(),
                key: "ENG".to_string(),
                name: "Engineering".to_string(),
                space_type: "global".to_string(),
                ..Default::default()
            }])
            .unwrap();
        storage
            .upsert_pages(&[PageSummary {
                id: "9001".to_string(),
                space_id: "sp-1".to_string(),
                space_key: "ENG".to_string(),
                title: "Architecture Spec".to_string(),
                status: "current".to_string(),
                version_number: 2,
                ..Default::default()
            }])
            .unwrap();

        let http = reqwest::Client::new();

        let res = http
            .get(format!("{base}/issues?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let issues_json: Value = res.json().await.unwrap();
        assert_eq!(issues_json["issues"][0]["key"], "ENG-42");

        let res = http
            .get(format!("{base}/spaces/ENG/pages?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let pages_json: Value = res.json().await.unwrap();
        assert_eq!(pages_json["pages"][0]["id"], "9001");

        let res = http
            .get(format!("{base}/search?q=Architecture&scope=all"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let search_json: Value = res.json().await.unwrap();
        assert_eq!(search_json["scope"], "all");
        assert_eq!(search_json["remote"], false);
        assert_eq!(search_json["hits"][0]["id_or_key"], "9001");

        let res = http
            .get(format!("{base}/search?q=FTS5&scope=jira"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let jira_search: Value = res.json().await.unwrap();
        assert_eq!(jira_search["hits"][0]["id_or_key"], "ENG-42");
    }
}
