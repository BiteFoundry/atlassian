//! Local HTTP JSON API (`atlassian serve` / `confluence serve`, default `127.0.0.1:8789`) powered by `axum`.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    atlassian::{
        body::{BodyEdit, BodyEditError, TextReplacement},
        client::{http_status_code, is_not_found_error},
        AtlassianClient, SiteClientPool,
    },
    config::AppConfig,
    models::{
        FederatedSearchHit, IssueSummary, IssueUpdate, OutgoingIssue, OutgoingPage, PageSummary,
        SearchHitKind, UnifiedSearchHit,
    },
    resolver::resolve_rejected_assignee,
    storage::Storage,
    sync::{self, SiteSyncTarget, SyncCommand},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
    /// Exact browser origins ("http://localhost:5173") allowed to call the API cross-origin.
    /// Empty (the default) means no cross-origin browser access at all.
    pub cors_origins: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: crate::config::DEFAULT_SERVE_PORT,
            token: None,
            cors_origins: Vec::new(),
        }
    }
}

const CROSS_ORIGIN_REFUSED: &str =
    "cross-origin request refused; start serve with --cors-origin <origin>";
const HOST_REFUSED: &str =
    "request refused: the Host header is not a loopback name (DNS rebinding protection)";

/// Validates one "--cors-origin" value and returns it in the form browsers send
/// (scheme://host[:port], default port omitted).
pub fn normalize_cors_origin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed == "*" {
        bail!(
            "`--cors-origin *` is not accepted: every website open in the browser could then call this API with your Atlassian credentials. List exact origins, for example `--cors-origin http://localhost:5173`."
        );
    }
    let invalid = || {
        anyhow!("Invalid `--cors-origin {trimmed}`: expected an exact origin such as http://localhost:5173.")
    };
    let url = url::Url::parse(trimmed).map_err(|_| invalid())?;
    let exact = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && !trimmed.contains('*');
    if !exact {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

/// Browser-facing request policy derived from [ServeOptions]: which "Origin" values may
/// call the API and whether "Host" must be a loopback name.
#[derive(Debug, Clone, Default)]
pub struct RequestGuard {
    cors_origins: Vec<String>,
    same_origins: Vec<String>,
    loopback_bind: bool,
}

impl RequestGuard {
    /// Invalid "cors_origins" entries are dropped here; [run] rejects them before serving.
    pub fn new(opts: &ServeOptions) -> Self {
        let bind = opts.bind.trim();
        let bind_ip = bind.parse::<IpAddr>().ok();
        let bind_host = match bind_ip {
            Some(IpAddr::V6(ip)) => format!("[{ip}]"),
            _ => bind.to_string(),
        };
        let same_origins = [bind_host.as_str(), "localhost", "127.0.0.1", "[::1]"]
            .iter()
            .filter_map(|host| url::Url::parse(&format!("http://{host}:{}", opts.port)).ok())
            .map(|url| url.origin().ascii_serialization())
            .collect();
        Self {
            cors_origins: opts
                .cors_origins
                .iter()
                .filter_map(|origin| normalize_cors_origin(origin).ok())
                .collect(),
            same_origins,
            loopback_bind: bind_ip.map_or(bind.eq_ignore_ascii_case("localhost"), |ip| ip.is_loopback()),
        }
    }

    /// True when a request carrying this "Origin" header may proceed.
    fn origin_allowed(&self, origin: &str) -> bool {
        let Ok(url) = url::Url::parse(origin.trim()) else {
            return false;
        };
        let origin = url.origin().ascii_serialization();
        self.cors_origins.contains(&origin) || self.same_origins.contains(&origin)
    }

    fn cors_layer(&self) -> Option<CorsLayer> {
        let origins: Vec<HeaderValue> = self
            .cors_origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect();
        if origins.is_empty() {
            return None;
        }
        Some(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
    }
}

/// True for "127.0.0.1[:port]", "localhost[:port]" and "[::1][:port]".
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    match name.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => name.eq_ignore_ascii_case("localhost"),
    }
}

/// A non-loopback listener without a token would expose the user's Atlassian access to
/// the whole network.
pub(crate) fn validate_bind(addr: &SocketAddr, token: Option<&str>) -> Result<()> {
    let has_token = token.is_some_and(|t| !t.trim().is_empty());
    if !addr.ip().is_loopback() && !has_token {
        bail!("Refusing to bind {addr} without --token. Pass --token <secret> or bind 127.0.0.1.");
    }
    Ok(())
}

/// Compares two byte strings without stopping at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub client: AtlassianClient,
    pub site_pool: SiteClientPool,
    pub storage: Storage,
    pub token: Option<String>,
    pub guard: RequestGuard,
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
        if let Some(invalid) = err.downcast_ref::<BodyEditError>() {
            return Self::bad_request(invalid.to_string());
        }
        let status = match http_status_code(&err) {
            Some(reqwest::StatusCode::BAD_REQUEST) => StatusCode::BAD_REQUEST,
            Some(reqwest::StatusCode::UNAUTHORIZED) => StatusCode::UNAUTHORIZED,
            Some(reqwest::StatusCode::FORBIDDEN) => StatusCode::FORBIDDEN,
            Some(reqwest::StatusCode::NOT_FOUND) => StatusCode::NOT_FOUND,
            Some(reqwest::StatusCode::CONFLICT) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, format!("{err:#}"))
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
    pub start_at: Option<usize>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetailQuery {
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    compact: bool,
    site: Option<String>,
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
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchIssueBody {
    #[serde(flatten)]
    pub update: IssueUpdate,
    pub append_markdown: Option<String>,
    pub replace_text: Option<TextReplacement>,
    #[serde(default)]
    pub site: Option<String>,
}

#[cfg(test)]
#[test]
fn issue_patch_distinguishes_omitted_null_and_assignee_id() {
    for (input, expected) in [
        (json!({"summary": "new"}), None),
        (json!({"summary": "new", "assignee_id": null}), Some(None)),
        (
            json!({"summary": "new", "assignee_id": "account-01"}),
            Some(Some("account-01".to_owned())),
        ),
    ] {
        let patch: PatchIssueBody = serde_json::from_value(input).unwrap();
        assert_eq!(patch.update.summary.as_deref(), Some("new"));
        assert_eq!(patch.update.assignee_id, expected);
    }
}

fn default_task_type() -> String {
    "Task".to_string()
}

#[derive(Debug, Deserialize)]
pub struct TransitionBody {
    pub transition: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssigneeBody {
    #[serde(default)]
    pub assignee_id: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommentBody {
    pub body_markdown: String,
    #[serde(default)]
    pub site: Option<String>,
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
    #[serde(default)]
    pub site: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePageBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body_markdown: Option<String>,
    pub append_markdown: Option<String>,
    pub replace_text: Option<TextReplacement>,
    #[serde(default)]
    pub prev_version: i32,
    #[serde(default)]
    pub version_message: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
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
///
/// Cross-origin browser requests are refused unless their origin is listed in
/// "opts.cors_origins"; requests without an "Origin" header (curl, agents) are unaffected.
pub fn build_router(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    opts: ServeOptions,
) -> Router {
    let guard = RequestGuard::new(&opts);
    let cors = guard.cors_layer();
    let token = opts.token;
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
        guard,
    });

    let router = Router::new()
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
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));
    // No CORS layer at all unless origins were allowlisted; the origin guard is outermost,
    // so it also stops "simple" cross-origin requests that never send a preflight.
    let router = match cors {
        Some(cors) => router.layer(cors),
        None => router,
    };
    router
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .with_state(state)
}

/// Start the HTTP JSON API server and background sync engine until stopped.
pub async fn run(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("Invalid bind address: {}:{}", opts.bind, opts.port))?;
    validate_bind(&addr, opts.token.as_deref())?;
    for origin in &opts.cors_origins {
        normalize_cors_origin(origin)?;
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), client.clone(), storage.clone(), event_tx);
    sync_ctl.request(SyncCommand::RefreshAll);

    let app = build_router(config, client, storage, opts);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind listener: {addr}"))?;

    println!("atlassian HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}

/// Refuses browser requests from origins that were not allowlisted and, on a loopback
/// listener, requests whose "Host" is not a loopback name.
async fn origin_guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let guard = &state.guard;
    if guard.loopback_bind {
        let host = request
            .headers()
            .get(header::HOST)
            .map(|value| value.to_str().unwrap_or("").to_string())
            .or_else(|| request.uri().authority().map(|a| a.as_str().to_string()));
        if host.is_some_and(|host| !is_loopback_host(&host)) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, HOST_REFUSED));
        }
    }
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        if !guard.origin_allowed(origin.to_str().unwrap_or("")) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, CROSS_ORIGIN_REFUSED));
        }
    }
    Ok(next.run(request).await)
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

            if !presented.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
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
    if trimmed.eq_ignore_ascii_case(state.client.site_id()) {
        return Some(state.client.site_id().to_string());
    }
    if let Some(found) = state.site_pool.site_for(trimmed) {
        return Some(found.id.clone());
    }
    Some(trimmed.to_string())
}

fn resolve_single_site_id(state: &AppState, raw_site: Option<&str>) -> Result<String, ApiError> {
    let Some(trimmed) = raw_site.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(state.client.site_id().to_string());
    };
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(state.client.site_id().to_string());
    }
    if let Some(found) = state.config.find_effective_site(trimmed) {
        return Ok(found.id);
    }
    if trimmed.eq_ignore_ascii_case(state.client.site_id()) {
        return Ok(state.client.site_id().to_string());
    }
    if let Some(found) = state.site_pool.site_for(trimmed) {
        return Ok(found.id.clone());
    }
    Err(ApiError::not_found(format!("site not found: '{trimmed}'")))
}

fn resolve_site_client(
    state: &AppState,
    raw_site: Option<&str>,
) -> Result<(String, AtlassianClient), ApiError> {
    let Some(trimmed) = raw_site.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((state.client.site_id().to_string(), state.client.clone()));
    };
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok((state.client.site_id().to_string(), state.client.clone()));
    }
    let canonical = state
        .config
        .find_effective_site(trimmed)
        .map(|s| s.id)
        .unwrap_or_else(|| trimmed.to_string());

    if canonical.eq_ignore_ascii_case(state.client.site_id()) {
        return Ok((state.client.site_id().to_string(), state.client.clone()));
    }
    if let Ok(client) = state.site_pool.client_for(&canonical) {
        return Ok((client.site_id().to_string(), client));
    }
    Err(ApiError::not_found(format!("site not found: '{trimmed}'")))
}

async fn refresh_issue_cache_after_write(
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

async fn refresh_page_cache_after_write(
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
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let cached = state.storage.list_projects_for_site(&target_site)?;
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

    Ok(Json(json!({ "site_id": target_site, "projects": projects })))
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
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let cached = state.storage.list_boards_for_site(&target_site, project)?;
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

    Ok(Json(json!({ "site_id": target_site, "boards": boards })))
}

async fn list_board_sprints(
    State(state): State<Arc<AppState>>,
    Path(board_id): Path<i64>,
    Query(query): Query<SprintsQuery>,
) -> Result<Json<Value>, ApiError> {
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let sprints = if query.offline {
        state.storage.list_sprints_for_site(&target_site, board_id)?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client.list_board_sprints(board_id).await?;
        let _ = state.storage.upsert_sprints_for_site(&target_site, &fetched);
        fetched
    };

    Ok(Json(json!({ "site_id": target_site, "board_id": board_id, "sprints": sprints })))
}

async fn list_board_issues(
    State(state): State<Arc<AppState>>,
    Path(board_id): Path<i64>,
    Query(query): Query<BoardIssuesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let issues = if query.offline {
        state
            .storage
            .list_board_issues_for_site(&target_site, board_id, query.sprint_id, limit)?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let fetched = client
            .list_board_issues(board_id, query.sprint_id, limit)
            .await?;
        let _ = state.storage.upsert_board_issues_for_site(
            &target_site,
            board_id,
            query.sprint_id,
            &fetched,
        );
        if fetched.len() < limit.min(100) {
            let keep: Vec<String> = fetched.iter().map(|i| i.key.clone()).collect();
            let _ = state.storage.prune_board_issues_for_site(
                &target_site,
                board_id,
                query.sprint_id,
                &keep,
            );
        }
        fetched
    };

    Ok(Json(json!({
        "site_id": target_site,
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
    let page_token = query
        .page_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if page_token.is_some() && query.start_at.is_some() {
        return Err(ApiError::bad_request(
            "specify either 'page_token' or 'start_at', not both",
        ));
    }
    let project = query
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if query.offline {
        let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
        let issues = state.storage.list_issues_for_site(&target_site, project, limit)?;
        let total = state
            .storage
            .count_issues_for_site(&target_site, project)?;
        return Ok(Json(json!({
            "site_id": target_site,
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
        .search_issues_jql(&jql, limit, page_token, query.start_at)
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
    Query(query): Query<SiteQuery>,
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

    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let (assignee_id, raw_assignee) = match body.assignee_id.as_deref().map(str::trim) {
        None | Some("") => (None, None),
        Some(raw) if raw.eq_ignore_ascii_case("me") => {
            let me = client.get_myself().await?;
            (Some(me.id), None)
        }
        Some(raw) => (Some(raw.to_string()), Some(raw.to_string())),
    };
    let mut outgoing = OutgoingIssue {
        project_key: project_key.to_string(),
        issue_type,
        summary: summary.to_string(),
        description_markdown: body.description_markdown,
        priority: body.priority.filter(|s| !s.trim().is_empty()),
        assignee_id,
        labels: body.labels,
        parent_key: body.parent_key.filter(|s| !s.trim().is_empty()),
    };

    let mut resolved_assignee = None;
    let detail = match client.create_issue(&outgoing).await {
        Ok(detail) => detail,
        Err(err) => {
            if let Some(raw) = raw_assignee.as_deref() {
                match resolve_rejected_assignee(&client, project_key, raw, &err, true).await {
                    Some(Ok(user)) => {
                        outgoing.assignee_id = Some(user.id.clone());
                        resolved_assignee = Some(user);
                        client.create_issue(&outgoing).await?
                    }
                    Some(Err(ambig)) => return Err(ambig.into()),
                    None => return Err(err.into()),
                }
            } else {
                return Err(err.into());
            }
        }
    };
    let _ = state.storage.upsert_issue_detail_for_site(&target_site, &detail);

    let mut payload = json!({ "ok": true, "site_id": target_site, "issue": detail });
    if let Some(user) = resolved_assignee {
        payload["resolved_assignee"] = json!({
            "id": user.id,
            "display_name": user.display_name,
        });
    }
    Ok(Json(payload))
}

async fn get_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }

    if query.offline {
        let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
        let detail = state
            .storage
            .get_issue_detail_for_site(&target_site, key)?
            .ok_or_else(|| ApiError::not_found(format!("issue not found in cache: {key}")))?;
        let stale = state
            .storage
            .is_issue_detail_stale_for_site(&target_site, key)
            .unwrap_or(false);
        return Ok(Json(json!({
            "site_id": target_site,
            "detail_stale": stale,
            "issue": detail.output(query.compact),
        })));
    }

    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let (mut fetched, meta) = match client.get_issue_with_meta(key).await {
        Ok(res) => res,
        Err(err) => {
            if is_not_found_error(&err) {
                let _ = state.storage.delete_issue_for_site(&target_site, key);
            }
            return Err(err.into());
        }
    };
    let _ = state.storage.upsert_issue_detail_for_site_merging(
        &target_site,
        &mut fetched,
        !meta.remote_links_ok,
    );

    Ok(Json(
        json!({ "site_id": target_site, "detail_stale": false, "issue": fetched.output(query.compact) }),
    ))
}

async fn patch_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<PatchIssueBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let mut patch = body.update;
    let edit = BodyEdit::from_inputs(
        patch.description_markdown.as_deref(),
        body.append_markdown.as_deref(),
        body.replace_text.as_ref(),
        None,
    )
    .map_err(anyhow::Error::from)?;
    if patch.summary.is_none()
        && edit.is_none()
        && patch.priority.is_none()
        && patch.assignee_id.is_none()
        && patch.labels.is_none()
    {
        return Err(ApiError::bad_request(
            "at least one field must be provided for issue update",
        ));
    }

    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let raw_assignee = match patch.assignee_id.take() {
        Some(Some(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                patch.assignee_id = Some(None);
                None
            } else if trimmed.eq_ignore_ascii_case("me") {
                let me = client.get_myself().await?;
                patch.assignee_id = Some(Some(me.id));
                None
            } else {
                patch.assignee_id = Some(Some(trimmed.to_string()));
                Some(trimmed.to_string())
            }
        }
        other => {
            patch.assignee_id = other;
            None
        }
    };
    let mut resolved_assignee = None;
    let detail = match client.edit_issue(key, &patch, edit).await {
        Ok(detail) => detail,
        Err(err) => {
            if let Some(raw) = raw_assignee.as_deref() {
                match resolve_rejected_assignee(&client, key, raw, &err, true).await {
                    Some(Ok(user)) => {
                        patch.assignee_id = Some(Some(user.id.clone()));
                        resolved_assignee = Some(user);
                        client.edit_issue(key, &patch, edit).await?
                    }
                    Some(Err(ambig)) => return Err(ambig.into()),
                    None => return Err(err.into()),
                }
            } else {
                return Err(err.into());
            }
        }
    };
    let _ = state
        .storage
        .upsert_issue_detail_for_site(&target_site, &detail);

    let mut payload = json!({ "ok": true, "site_id": target_site, "issue": detail });
    if let Some(user) = resolved_assignee {
        payload["resolved_assignee"] = json!({
            "id": user.id,
            "display_name": user.display_name,
        });
    }
    Ok(Json(payload))
}

async fn delete_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    client.delete_issue(key).await?;
    let _ = state.storage.delete_issue_for_site(&target_site, key);
    Ok(Json(json!({ "ok": true, "site_id": target_site, "key": key })))
}

async fn list_transitions(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let transitions = client.list_transitions(key).await?;
    Ok(Json(json!({ "site_id": target_site, "key": key, "transitions": transitions })))
}

async fn transition_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<TransitionBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    let transition = body.transition.trim();
    if key.is_empty() || transition.is_empty() {
        return Err(ApiError::bad_request(
            "issue key and transition must not be empty",
        ));
    }
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let executed = client
        .transition_issue(key, transition, body.comment.as_deref())
        .await?;
    refresh_issue_cache_after_write(&client, &state.storage, &target_site, key).await;
    let transition_id = executed
        .as_ref()
        .map(|t| t.id.as_str())
        .unwrap_or(transition);
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
    Ok(Json(payload))
}

async fn assign_issue(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<AssigneeBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiError::bad_request("issue key must not be empty"));
    }
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let (mut resolved, raw_assignee) = match body.assignee_id.as_deref().map(str::trim) {
        None | Some("") => (None, None),
        Some(raw) if raw.eq_ignore_ascii_case("me") => {
            let me = client.get_myself().await?;
            (Some(me.id), None)
        }
        Some(raw) => (Some(raw.to_string()), Some(raw.to_string())),
    };

    let mut resolved_assignee = None;
    if let Err(err) = client.assign_issue(key, resolved.as_deref()).await {
        if let Some(raw) = raw_assignee.as_deref() {
            match resolve_rejected_assignee(&client, key, raw, &err, false).await {
                Some(Ok(user)) => {
                    resolved = Some(user.id.clone());
                    resolved_assignee = Some(user);
                    client.assign_issue(key, resolved.as_deref()).await?;
                }
                Some(Err(ambig)) => return Err(ambig.into()),
                None => return Err(err.into()),
            }
        } else {
            return Err(err.into());
        }
    }
    refresh_issue_cache_after_write(&client, &state.storage, &target_site, key).await;
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
    Ok(Json(payload))
}

async fn list_issue_comments(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let comments = client.list_issue_comments(key).await?;
    Ok(Json(json!({ "site_id": target_site, "key": key, "comments": comments })))
}

async fn add_issue_comment(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let key = key.trim();
    if body.body_markdown.trim().is_empty() {
        return Err(ApiError::bad_request("body_markdown must not be empty"));
    }
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let comment = client
        .add_issue_comment(key, &body.body_markdown)
        .await?;
    refresh_issue_cache_after_write(&client, &state.storage, &target_site, key).await;
    Ok(Json(json!({ "ok": true, "site_id": target_site, "comment": comment })))
}

async fn list_spaces(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SpacesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let cached = state.storage.list_spaces_for_site(&target_site)?;
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
    Ok(Json(json!({ "site_id": target_site, "spaces": spaces })))
}

async fn list_space_pages(
    State(state): State<Arc<AppState>>,
    Path(space): Path<String>,
    Query(query): Query<SpacePagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit.unwrap_or(50));
    let space = space.trim();
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let mut pages = if query.offline {
        state
            .storage
            .list_space_root_pages_for_site(&target_site, space)?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let mut fetched = client.list_space_root_pages(space, limit).await?;
        let _ = state
            .storage
            .upsert_authoritative_pages_for_site(&target_site, &fetched);
        if fetched.len() < limit.min(250) {
            let keep: Vec<String> = fetched.iter().map(|p| p.id.clone()).collect();
            let _ = state
                .storage
                .prune_space_root_pages_for_site(&target_site, space, &keep);
        }
        let _ = state
            .storage
            .overlay_children_state_for_site(&target_site, &mut fetched);
        fetched
    };
    pages.truncate(limit);
    Ok(Json(json!({ "site_id": target_site, "space": space, "pages": pages })))
}

async fn get_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if query.offline {
        let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
        let detail = state
            .storage
            .get_page_detail_for_site(&target_site, id)?
            .ok_or_else(|| ApiError::not_found(format!("page not found in cache: {id}")))?;
        let stale = state
            .storage
            .is_page_detail_stale_for_site(&target_site, id)
            .unwrap_or(false);
        return Ok(Json(json!({
            "site_id": target_site,
            "detail_stale": stale,
            "page": detail.output(query.compact),
        })));
    }

    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let (mut fetched, meta) = match client
        .get_page_with_meta_in_cache(id, &target_site, &state.storage)
        .await
    {
        Ok(res) => res,
        Err(err) => {
            if is_not_found_error(&err) {
                let _ = state.storage.delete_page_for_site(&target_site, id);
            }
            return Err(err.into());
        }
    };
    let _ = state
        .storage
        .upsert_page_detail_for_site_with_meta(&target_site, &mut fetched, meta);
    Ok(Json(
        json!({ "site_id": target_site, "detail_stale": false, "page": fetched.output(query.compact) }),
    ))
}

async fn list_child_pages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let target_site = resolve_single_site_id(&state, query.site.as_deref())?;
    let pages = if query.offline {
        state.storage.list_child_pages_for_site(&target_site, id)?
    } else {
        let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
        let mut fetched = client.list_child_pages(id).await?;
        let _ = state.storage.upsert_pages_for_site(&target_site, &fetched);
        if fetched.len() < 100 {
            let keep: Vec<String> = fetched.iter().map(|p| p.id.clone()).collect();
            let _ = state
                .storage
                .prune_child_pages_for_site(&target_site, id, &keep);
        }
        let _ = state
            .storage
            .overlay_children_state_for_site(&target_site, &mut fetched);
        fetched
    };
    Ok(Json(json!({ "site_id": target_site, "parent_id": id, "pages": pages })))
}

async fn create_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SiteQuery>,
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
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let req = OutgoingPage {
        space_id_or_key: space.to_string(),
        parent_id: body.parent_id.filter(|s| !s.trim().is_empty()),
        title: title.to_string(),
        body_markdown: body.body_markdown,
    };
    let detail = client.create_page(&req).await?;
    let _ = state.storage.upsert_page_detail_for_site(&target_site, &detail);
    Ok(Json(json!({ "ok": true, "site_id": target_site, "page": detail })))
}

async fn update_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<UpdatePageBody>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let edit = BodyEdit::from_inputs(
        body.body_markdown.as_deref(),
        body.append_markdown.as_deref(),
        body.replace_text.as_ref(),
        None,
    )
    .map_err(anyhow::Error::from)?;
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;

    let detail = client
        .edit_page(
            id,
            body.title,
            edit,
            Some(body.prev_version),
            body.version_message,
        )
        .await?;
    let _ = state
        .storage
        .upsert_page_detail_for_site(&target_site, &detail);
    Ok(Json(
        json!({ "ok": true, "site_id": target_site, "page": detail }),
    ))
}

async fn delete_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("page id must not be empty"));
    }
    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    client.delete_page(id).await?;
    let _ = state.storage.delete_page_for_site(&target_site, id);
    Ok(Json(json!({ "ok": true, "site_id": target_site, "id": id })))
}

async fn list_page_comments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<SiteQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    let (target_site, client) = resolve_site_client(&state, query.site.as_deref())?;
    let comments = client.list_page_comments(id).await?;
    Ok(Json(json!({ "site_id": target_site, "page_id": id, "comments": comments })))
}

async fn add_page_comment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<SiteQuery>,
    Json(body): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim();
    if body.body_markdown.trim().is_empty() {
        return Err(ApiError::bad_request("body_markdown must not be empty"));
    }
    let (target_site, client) =
        resolve_site_client(&state, query.site.as_deref().or(body.site.as_deref()))?;
    let comment = client
        .add_page_comment(id, &body.body_markdown)
        .await?;
    refresh_page_cache_after_write(&client, &state.storage, &target_site, id).await;
    Ok(Json(json!({ "ok": true, "site_id": target_site, "comment": comment })))
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
            let jql = if looks_like_jql_expr(q) {
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
            let cql = if looks_like_cql_expr(q) {
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
        crate::storage::sort_federated_hits(&mut combined);
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
    let (site_id, client) = resolve_site_client(&state, query.site.as_deref())?;
    let summary = sync::bootstrap_workspace_for_site(&site_id, &client, &state.storage).await?;
    let counts = state.storage.cache_counts()?;
    Ok(Json(json!({
        "ok": true,
        "site_id": site_id,
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

const JQL_AUTO_FIELDS: &[&str] = &[
    "project",
    "key",
    "issuekey",
    "issuetype",
    "type",
    "status",
    "assignee",
    "reporter",
    "summary",
    "text",
    "labels",
    "priority",
    "created",
    "updated",
    "resolution",
    "sprint",
];

const CQL_AUTO_FIELDS: &[&str] = &[
    "space",
    "type",
    "title",
    "text",
    "ancestor",
    "parent",
    "label",
    "creator",
    "created",
    "lastmodified",
    "id",
];

pub(crate) fn looks_like_jql_expr(raw: &str) -> bool {
    looks_like_structured_query_expr(raw, JQL_AUTO_FIELDS, true)
}

pub(crate) fn looks_like_cql_expr(raw: &str) -> bool {
    looks_like_structured_query_expr(raw, CQL_AUTO_FIELDS, false)
}

fn looks_like_structured_query_expr(raw: &str, known_fields: &[&str], allow_jira_cf: bool) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Leading `ORDER BY <expr>`
    if let Some(after_order) = strip_ascii_keyword(trimmed, "order") {
        let after_ws = after_order.trim_start();
        if after_ws.len() < after_order.len() {
            if let Some(after_by) = strip_ascii_keyword(after_ws, "by") {
                let operand = after_by.trim_start();
                if operand.len() < after_by.len() && !operand.is_empty() {
                    return true;
                }
            }
        }
    }

    let Some(after_field) = consume_query_field(trimmed, known_fields, allow_jira_cf) else {
        return false;
    };
    consume_operator_and_operand(after_field)
}

fn strip_ascii_keyword<'a>(input: &'a str, kw: &str) -> Option<&'a str> {
    if input.len() < kw.len() || !input.is_char_boundary(kw.len()) {
        return None;
    }
    let (head, tail) = input.split_at(kw.len());
    if !head.eq_ignore_ascii_case(kw) {
        return None;
    }
    if tail
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(tail)
}

fn consume_query_field<'a>(
    input: &'a str,
    known_fields: &[&str],
    allow_jira_cf: bool,
) -> Option<&'a str> {
    // Quoted field name: "Custom Field" = ...
    if let Some(rest) = input.strip_prefix('"') {
        let mut escaped = false;
        let mut inner_len = 0usize;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                escaped = false;
                inner_len += 1;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                inner_len += 1;
                continue;
            }
            if ch == '"' {
                if inner_len == 0 {
                    return None;
                }
                return Some(&rest[idx + 1..]);
            }
            inner_len += 1;
        }
        return None;
    }

    // Jira custom field syntax: cf[10016]
    if allow_jira_cf && input.len() >= 5 && input.is_char_boundary(3) {
        let (prefix, rest) = input.split_at(3);
        if prefix.eq_ignore_ascii_case("cf[") {
            let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 && rest.as_bytes().get(digits) == Some(&b']') {
                return Some(&rest[digits + 1..]);
            }
        }
    }

    // Known field identifier (case-insensitive, word boundary)
    for &field in known_fields {
        if let Some(tail) = strip_ascii_keyword(input, field) {
            return Some(tail);
        }
    }
    None
}

fn consume_operator_and_operand(after_field: &str) -> bool {
    let op_start = after_field.trim_start();
    let had_ws_before_op = op_start.len() < after_field.len();

    for sym in ["!=", "!~", "<=", ">=", "=", "~", "<", ">"] {
        if let Some(after_op) = op_start.strip_prefix(sym) {
            return !after_op.trim().is_empty();
        }
    }

    if !had_ws_before_op {
        return false;
    }

    // `NOT IN`
    if let Some(after_not) = strip_ascii_keyword(op_start, "not") {
        let after_not_ws = after_not.trim_start();
        if after_not_ws.len() < after_not.len() {
            if let Some(after_in) = strip_ascii_keyword(after_not_ws, "in") {
                return has_keyword_operand(after_in, true);
            }
        }
    }
    // `IN`
    if let Some(after_in) = strip_ascii_keyword(op_start, "in") {
        return has_keyword_operand(after_in, true);
    }
    // `IS NOT` / `IS`
    if let Some(after_is) = strip_ascii_keyword(op_start, "is") {
        let after_is_ws = after_is.trim_start();
        if after_is_ws.len() < after_is.len() {
            if let Some(after_not) = strip_ascii_keyword(after_is_ws, "not") {
                return has_keyword_operand(after_not, false);
            }
            return !after_is_ws.is_empty();
        }
    }

    false
}

fn has_keyword_operand(after_kw: &str, allow_open_paren: bool) -> bool {
    let trimmed = after_kw.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.len() < after_kw.len() || (allow_open_paren && trimmed.starts_with('('))
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
        spawn_test_server_with(ServeOptions {
            token,
            ..ServeOptions::default()
        })
        .await
    }

    async fn spawn_test_server_with(opts: ServeOptions) -> (String, Storage) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".to_string(),
            deployment: DeploymentKind::Cloud,
            ..AppConfig::default()
        };
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router(cfg, client, storage.clone(), opts);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{addr}"), storage)
    }

    const EVIL_ORIGIN: &str = "https://evil.example";

    #[tokio::test]
    async fn cors_is_not_permissive_by_default() {
        let (base, _storage) = spawn_test_server(None).await;
        let res = reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, format!("{base}/issues/ENG-1"))
            .header("Origin", EVIL_ORIGIN)
            .header("Access-Control-Request-Method", "DELETE")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get("access-control-allow-origin").is_none());
        assert!(res.headers().get("access-control-allow-methods").is_none());
    }

    #[tokio::test]
    async fn cross_origin_request_is_refused_without_allowlist() {
        let (base, _storage) = spawn_test_server(None).await;
        let http = reqwest::Client::new();
        // The guard answers before any handler (and so before any Atlassian call) runs.
        let res = http
            .delete(format!("{base}/issues/ENG-1"))
            .header("Origin", EVIL_ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get("access-control-allow-origin").is_none());
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"], CROSS_ORIGIN_REFUSED);

        // "Simple" requests never trigger a preflight, so they are refused here too.
        for origin in [EVIL_ORIGIN, "null"] {
            let res = http
                .post(format!("{base}/sync"))
                .header("Origin", origin)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("a=b")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin}");
            let res = http
                .get(format!("{base}/health"))
                .header("Origin", origin)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin}");
        }
    }

    #[tokio::test]
    async fn allowlisted_origin_gets_cors_headers() {
        let allowed = "http://localhost:5173";
        let (base, _storage) = spawn_test_server_with(ServeOptions {
            cors_origins: vec![format!("{allowed}/")],
            ..ServeOptions::default()
        })
        .await;
        let http = reqwest::Client::new();

        let preflight = http
            .request(reqwest::Method::OPTIONS, format!("{base}/issues/ENG-1"))
            .header("Origin", allowed)
            .header("Access-Control-Request-Method", "DELETE")
            .header("Access-Control-Request-Headers", "authorization,content-type")
            .send()
            .await
            .unwrap();
        assert!(preflight.status().is_success(), "{}", preflight.status());
        assert_eq!(preflight.headers()["access-control-allow-origin"], allowed);
        let methods = preflight.headers()["access-control-allow-methods"].to_str().unwrap();
        assert!(methods.contains("DELETE") && methods.contains("PATCH"), "{methods}");
        let headers = preflight.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(headers.contains("authorization") && headers.contains("content-type"), "{headers}");

        let res = http
            .get(format!("{base}/health"))
            .header("Origin", allowed)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["access-control-allow-origin"], allowed);

        // Other origins stay refused and get no CORS headers.
        let res = http
            .get(format!("{base}/health"))
            .header("Origin", EVIL_ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn request_without_origin_still_works() {
        let (base, _storage) = spawn_test_server(None).await;
        let http = reqwest::Client::new();
        let res = http.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("access-control-allow-origin").is_none());
        let res = http.get(format!("{base}/issues?offline=true")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rebinding_host_header_is_refused_on_loopback() {
        let (base, _storage) = spawn_test_server(None).await;
        let http = reqwest::Client::new();
        let res = http
            .get(format!("{base}/health"))
            .header(reqwest::header::HOST, "evil.example:8789")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"], HOST_REFUSED);

        for host in ["localhost:8789", "127.0.0.1", "[::1]:8789"] {
            let res = http
                .get(format!("{base}/health"))
                .header(reqwest::header::HOST, host)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{host}");
        }
        assert!(!is_loopback_host("127.0.0.1.evil.example"));
        assert!(!is_loopback_host("localhost.evil.example:8789"));
        assert!(!is_loopback_host(""));

        // A listener on a routable address is reached by name, so Host is not checked there.
        let lan = RequestGuard::new(&ServeOptions {
            bind: "192.168.1.5".into(),
            token: Some("secret".into()),
            ..ServeOptions::default()
        });
        assert!(!lan.loopback_bind);
        assert!(RequestGuard::new(&ServeOptions::default()).loopback_bind);
    }

    #[test]
    fn non_loopback_bind_requires_token() {
        let addr = |text: &str| text.parse::<SocketAddr>().unwrap();
        for open in ["0.0.0.0:8789", "192.168.1.5:8789", "[::]:8789"] {
            let err = validate_bind(&addr(open), None).unwrap_err().to_string();
            assert!(err.contains("Refusing to bind") && err.contains("--token"), "{err}");
            assert!(validate_bind(&addr(open), Some("  ")).is_err(), "{open}");
            assert!(validate_bind(&addr(open), Some("secret")).is_ok(), "{open}");
        }
        for loopback in ["127.0.0.1:8789", "[::1]:8789"] {
            assert!(validate_bind(&addr(loopback), None).is_ok(), "{loopback}");
        }
    }

    #[test]
    fn cors_origin_values_are_exact_origins() {
        assert_eq!(normalize_cors_origin(" http://localhost:5173/ ").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_cors_origin("https://App.Example:443").unwrap(), "https://app.example");
        let wildcard = normalize_cors_origin("*").unwrap_err().to_string();
        assert!(wildcard.contains("every website"), "{wildcard}");
        for bad in ["http://*.example.com", "localhost:5173", "http://localhost:5173/app", "null", "file:///x"] {
            assert!(normalize_cors_origin(bad).is_err(), "{bad}");
        }

        let guard = RequestGuard::new(&ServeOptions {
            cors_origins: vec!["http://localhost:5173".into(), "*".into()],
            ..ServeOptions::default()
        });
        assert!(guard.origin_allowed("http://localhost:5173"));
        // Same-origin with the listener (default port 8789).
        assert!(guard.origin_allowed("http://127.0.0.1:8789"));
        assert!(guard.origin_allowed("http://localhost:8789"));
        assert!(!guard.origin_allowed("http://localhost:5174"));
        assert!(!guard.origin_allowed("https://localhost:5173"));
        assert!(!guard.origin_allowed("null"));
        assert!(!guard.origin_allowed(""));
    }

    #[test]
    fn bearer_token_comparison_is_exact() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
        assert!(!constant_time_eq(b"secret", b"secret-token"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
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
