//! Atlassian authentication manager: Cloud (`OAuth 2.0 3LO`, shared `CloudAccountToken`,
//! `Basic base64(email:api_token)`) + Data Center (`Bearer PAT` or `Basic`) + OS Keyring + file fallback.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::oneshot};
use url::Url;

use crate::config::{
    credentials_fallback_path, normalize_base_url, slugify_site_id, write_atomic_0600, AppConfig,
    DeploymentKind, SiteProfile,
};

const KEYRING_SERVICE: &str = "atlassian-cli";

pub const DEFAULT_OAUTH_AUTH_BASE: &str = "https://auth.atlassian.com";
pub const DEFAULT_OAUTH_API_BASE: &str = "https://api.atlassian.com";
pub const OAUTH_REFRESH_MARGIN_SECS: u64 = 60;

/// Default Atlassian Cloud OAuth 2.0 (3LO) scopes covering offline refresh, user profile,
/// Jira Work / Agile, and Confluence Spaces / Pages / Search.
pub const DEFAULT_OAUTH_SCOPES: &[&str] = &[
    "offline_access",
    "read:me",
    "read:jira-user",
    "read:jira-work",
    "write:jira-work",
    "read:board-scope:jira-software",
    "read:sprint:jira-software",
    "read:confluence-space.summary",
    "read:confluence-content.all",
    "read:confluence-content.summary",
    "write:confluence-content",
    "write:confluence-file",
    "search:confluence",
];

/// An Atlassian Cloud site returned by `GET /oauth/token/accessible-resources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibleResource {
    /// Atlassian Cloud `cloudId` used in `https://api.atlassian.com/ex/{jira,confluence}/{id}`.
    pub id: String,
    /// Human-facing Cloud site URL (e.g. `https://acme.atlassian.net`).
    pub url: String,
    /// Site display name.
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, rename = "avatarUrl", alias = "avatar_url")]
    pub avatar_url: Option<String>,
}

impl AccessibleResource {
    /// Converts this discovered Cloud resource into a [`SiteProfile`] linked to `account_id`.
    pub fn to_site_profile(&self, account_id: impl Into<String>) -> SiteProfile {
        let normalized_url = normalize_base_url(&self.url);
        let id = slugify_site_id(if !normalized_url.is_empty() {
            &normalized_url
        } else {
            &self.name
        });
        let display_name = if self.name.trim().is_empty() {
            id.clone()
        } else {
            self.name.trim().to_string()
        };
        SiteProfile {
            id,
            name: display_name,
            site_url: normalized_url,
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: Some(self.id.trim().to_string()),
            account_id: account_id.into(),
            enabled: true,
            scopes: self.scopes.clone(),
            confluence_account_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "auth_kind", rename_all = "snake_case")]
pub enum StoredCredentials {
    /// Atlassian Cloud (`email` + API Token -> `Authorization: Basic base64(email:token)`)
    /// or Data Center Basic auth.
    ApiToken {
        site_url: String,
        email: String,
        api_token: String,
        deployment: DeploymentKind,
    },
    /// Personal Access Token or Bearer token (`Authorization: Bearer <token>`).
    BearerPat {
        site_url: String,
        token: String,
        deployment: DeploymentKind,
    },
    /// Atlassian Cloud OAuth 2.0 (3LO) credentials with automatic refresh token rotation.
    CloudOAuth {
        #[serde(default)]
        email: Option<String>,
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at_epoch: u64,
        client_id: String,
        #[serde(default)]
        client_secret: Option<String>,
        #[serde(default)]
        scopes: Vec<String>,
    },
    /// Account-level Atlassian Cloud API Token shared across unlimited `*.atlassian.net` sites.
    CloudAccountToken {
        email: String,
        api_token: String,
    },
}

impl StoredCredentials {
    pub fn site_url(&self) -> &str {
        match self {
            Self::ApiToken { site_url, .. } | Self::BearerPat { site_url, .. } => site_url,
            Self::CloudOAuth { .. } | Self::CloudAccountToken { .. } => "",
        }
    }

    pub fn deployment(&self) -> DeploymentKind {
        match self {
            Self::ApiToken { deployment, .. } | Self::BearerPat { deployment, .. } => *deployment,
            Self::CloudOAuth { .. } | Self::CloudAccountToken { .. } => DeploymentKind::Cloud,
        }
    }

    pub fn auth_kind_label(&self) -> &'static str {
        match self {
            Self::ApiToken { deployment, .. } => match deployment {
                DeploymentKind::Cloud => "cloud_api_token",
                DeploymentKind::DataCenter => "datacenter_basic",
            },
            Self::BearerPat { deployment, .. } => match deployment {
                DeploymentKind::Cloud => "cloud_pat",
                DeploymentKind::DataCenter => "datacenter_pat",
            },
            Self::CloudOAuth { .. } => "cloud_oauth",
            Self::CloudAccountToken { .. } => "cloud_account_token",
        }
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::CloudOAuth { .. })
    }

    pub fn oauth_access_token(&self) -> Option<&str> {
        match self {
            Self::CloudOAuth { access_token, .. } => Some(access_token.trim()),
            _ => None,
        }
    }

    /// Returns `true` if this is a `CloudOAuth` credential whose token expires within `margin_secs`.
    pub fn is_expired_within(&self, margin_secs: u64) -> bool {
        match self {
            Self::CloudOAuth {
                expires_at_epoch, ..
            } => {
                if *expires_at_epoch == 0 {
                    false
                } else {
                    now_epoch_secs().saturating_add(margin_secs) >= *expires_at_epoch
                }
            }
            _ => false,
        }
    }

    pub fn authorization_header(&self) -> String {
        match self {
            Self::ApiToken {
                email, api_token, ..
            }
            | Self::CloudAccountToken { email, api_token } => {
                let raw = format!("{}:{}", email.trim(), api_token.trim());
                format!("Basic {}", BASE64_STANDARD.encode(raw.as_bytes()))
            }
            Self::BearerPat { token, .. } => format!("Bearer {}", token.trim()),
            Self::CloudOAuth { access_token, .. } => format!("Bearer {}", access_token.trim()),
        }
    }

    pub fn status_summary(&self) -> String {
        match self {
            Self::ApiToken {
                email, deployment, ..
            } => match deployment {
                DeploymentKind::Cloud => format!("Signed in (Cloud API Token: {email})"),
                DeploymentKind::DataCenter => format!("Signed in (Data Center Basic: {email})"),
            },
            Self::BearerPat { deployment, .. } => match deployment {
                DeploymentKind::Cloud => "Signed in (Cloud Bearer PAT)".to_string(),
                DeploymentKind::DataCenter => "Signed in (Data Center PAT)".to_string(),
            },
            Self::CloudOAuth { email, .. } => match email.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(e) => format!("Signed in (Cloud OAuth 2.0: {e})"),
                None => "Signed in (Cloud OAuth 2.0)".to_string(),
            },
            Self::CloudAccountToken { email, .. } => {
                format!("Signed in (Cloud Shared API Token: {email})")
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub site_url: Option<String>,
    pub confluence_url: Option<String>,
    pub email: Option<String>,
    pub token: Option<String>,
    pub pat: Option<String>,
    pub deployment: Option<DeploymentKind>,
}

/// Configuration for [`run_oauth_loopback_login`] and [`start_oauth_loopback_session`].
#[derive(Debug, Clone)]
pub struct OAuthLoopbackOptions {
    pub auth_base: String,
    pub api_base: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Local callback bind port (`0` selects an ephemeral OS port).
    pub callback_port: u16,
    /// Callback path (default `"/callback"`).
    pub callback_path: String,
    /// Maximum time to wait for browser redirect callback.
    pub timeout: Duration,
    /// Whether to open the authorization URL automatically using `open::that`.
    pub open_browser: bool,
}

impl Default for OAuthLoopbackOptions {
    fn default() -> Self {
        Self {
            auth_base: DEFAULT_OAUTH_AUTH_BASE.to_string(),
            api_base: DEFAULT_OAUTH_API_BASE.to_string(),
            client_id: String::new(),
            client_secret: None,
            scopes: DEFAULT_OAUTH_SCOPES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            callback_port: 0,
            callback_path: "/callback".to_string(),
            timeout: Duration::from_secs(180),
            open_browser: true,
        }
    }
}

/// Active local loopback listener session returned by [`start_oauth_loopback_session`].
pub struct OAuthLoopbackSession {
    pub authorize_url: String,
    pub redirect_uri: String,
    pub state: String,
    callback_rx: oneshot::Receiver<Result<String>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl OAuthLoopbackSession {
    /// Waits for the browser callback, verifies `state`, exchanges the authorization code for tokens,
    /// fetches accessible Cloud resources, and enriches `email` from `/me` on a best-effort basis.
    pub async fn finish(
        mut self,
        http: &reqwest::Client,
        opts: &OAuthLoopbackOptions,
    ) -> Result<(StoredCredentials, Vec<AccessibleResource>)> {
        let code_res = tokio::time::timeout(opts.timeout, &mut self.callback_rx).await;
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let code = match code_res {
            Ok(Ok(Ok(code))) => code,
            Ok(Ok(Err(err))) => return Err(err),
            Ok(Err(_)) => bail!("OAuth loopback callback channel closed unexpectedly"),
            Err(_) => bail!(
                "Timed out after {}s waiting for Atlassian OAuth browser callback",
                opts.timeout.as_secs()
            ),
        };

        let mut creds = exchange_oauth_code(
            http,
            &opts.auth_base,
            &opts.client_id,
            opts.client_secret.as_deref(),
            &code,
            &self.redirect_uri,
        )
        .await?;

        let access_token = creds
            .oauth_access_token()
            .unwrap_or_default()
            .to_string();

        if let Ok(Some(email)) = fetch_oauth_me_email(http, &opts.api_base, &access_token).await {
            if let StoredCredentials::CloudOAuth {
                email: ref mut slot,
                ..
            } = creds
            {
                *slot = Some(email);
            }
        }

        let resources = fetch_accessible_resources(http, &opts.api_base, &access_token).await?;
        Ok((creds, resources))
    }
}

impl Drop for OAuthLoopbackSession {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Builds the Atlassian Cloud OAuth 2.0 (3LO) authorization URL.
///
/// Always includes `audience=api.atlassian.com`, `response_type=code`, `prompt=consent`,
/// and ensures `offline_access` plus Jira/Confluence scopes are requested.
pub fn build_oauth_authorize_url<S: AsRef<str>>(
    auth_base: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    scopes: &[S],
) -> String {
    let base = effective_auth_base(auth_base);
    let scope_list: Vec<String> = if scopes.is_empty() {
        DEFAULT_OAUTH_SCOPES
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        let mut collected: Vec<String> = scopes
            .iter()
            .map(|s| s.as_ref().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !collected.iter().any(|s| s == "offline_access") {
            collected.insert(0, "offline_access".to_string());
        }
        collected
    };
    let scope_str = scope_list.join(" ");

    let mut url = Url::parse(&format!("{base}/authorize"))
        .unwrap_or_else(|_| Url::parse("https://auth.atlassian.com/authorize").unwrap());
    url.query_pairs_mut()
        .append_pair("audience", "api.atlassian.com")
        .append_pair("client_id", client_id.trim())
        .append_pair("scope", &scope_str)
        .append_pair("redirect_uri", redirect_uri.trim())
        .append_pair("state", state.trim())
        .append_pair("response_type", "code")
        .append_pair("prompt", "consent");
    url.to_string()
}

/// Exchanges an OAuth 2.0 authorization code at `{auth_base}/oauth/token`.
pub async fn exchange_oauth_code(
    http: &reqwest::Client,
    auth_base: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
) -> Result<StoredCredentials> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        bail!("OAuth client_id is required");
    }
    let code = code.trim();
    if code.is_empty() {
        bail!("OAuth authorization code is empty");
    }

    let token_url = format!("{}/oauth/token", effective_auth_base(auth_base));
    let mut payload = json!({
        "grant_type": "authorization_code",
        "client_id": client_id,
        "code": code,
        "redirect_uri": redirect_uri.trim(),
    });
    let secret_opt = client_secret
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(ref secret) = secret_opt {
        payload["client_secret"] = Value::String(secret.clone());
    }

    let resp = http
        .post(&token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("Failed to send OAuth token exchange request to {token_url}"))?;

    let token_resp = parse_oauth_token_response(resp, "OAuth code exchange").await?;
    let expires_at_epoch = now_epoch_secs().saturating_add(token_resp.expires_in.unwrap_or(3600));
    let scopes = split_oauth_scopes(token_resp.scope.as_deref());

    Ok(StoredCredentials::CloudOAuth {
        email: None,
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token.filter(|s| !s.trim().is_empty()),
        expires_at_epoch,
        client_id: client_id.to_string(),
        client_secret: secret_opt,
        scopes,
    })
}

/// Refreshes an expired or expiring `StoredCredentials::CloudOAuth` token via `{auth_base}/oauth/token`,
/// properly persisting rotated refresh tokens when Atlassian issues a new `refresh_token`.
pub async fn refresh_oauth_token(
    http: &reqwest::Client,
    auth_base: &str,
    creds: &StoredCredentials,
) -> Result<StoredCredentials> {
    let StoredCredentials::CloudOAuth {
        email,
        refresh_token,
        client_id,
        client_secret,
        scopes: existing_scopes,
        ..
    } = creds
    else {
        bail!("Cannot refresh non-OAuth credentials");
    };

    let current_refresh = refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!("OAuth refresh token is missing; please sign in again with OAuth 2.0")
        })?;

    let token_url = format!("{}/oauth/token", effective_auth_base(auth_base));
    let mut payload = json!({
        "grant_type": "refresh_token",
        "client_id": client_id.trim(),
        "refresh_token": current_refresh,
    });
    if let Some(secret) = client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        payload["client_secret"] = Value::String(secret.to_string());
    }

    let resp = http
        .post(&token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("Failed to send OAuth token refresh request to {token_url}"))?;

    let token_resp = parse_oauth_token_response(resp, "OAuth token refresh").await?;
    let expires_at_epoch = now_epoch_secs().saturating_add(token_resp.expires_in.unwrap_or(3600));
    let rotated_refresh = token_resp
        .refresh_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| Some(current_refresh.to_string()));
    let new_scopes = split_oauth_scopes(token_resp.scope.as_deref());
    let scopes = if new_scopes.is_empty() {
        existing_scopes.clone()
    } else {
        new_scopes
    };

    Ok(StoredCredentials::CloudOAuth {
        email: email.clone(),
        access_token: token_resp.access_token,
        refresh_token: rotated_refresh,
        expires_at_epoch,
        client_id: client_id.clone(),
        client_secret: client_secret.clone(),
        scopes,
    })
}

/// Discovers all consented Atlassian Cloud sites for an OAuth access token via
/// `GET {api_base}/oauth/token/accessible-resources`.
pub async fn fetch_accessible_resources(
    http: &reqwest::Client,
    api_base: &str,
    access_token: &str,
) -> Result<Vec<AccessibleResource>> {
    let token = access_token.trim();
    if token.is_empty() {
        bail!("OAuth access token is empty");
    }
    let url = format!(
        "{}/oauth/token/accessible-resources",
        effective_api_base(api_base)
    );
    let resp = http
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .with_context(|| format!("Failed to fetch accessible resources from {url}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("Failed to read accessible resources response from {url}"))?;
    if !status.is_success() {
        bail!(
            "Failed to fetch accessible Atlassian Cloud resources (HTTP {status}): {}",
            extract_oauth_error(&text)
        );
    }

    let mut resources: Vec<AccessibleResource> = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse accessible resources JSON from {url}"))?;
    for res in &mut resources {
        res.url = normalize_base_url(&res.url);
    }
    Ok(resources)
}

/// Starts a local `127.0.0.1` loopback HTTP listener for OAuth 2.0 (3LO) authorization callbacks.
pub async fn start_oauth_loopback_session(
    opts: &OAuthLoopbackOptions,
) -> Result<OAuthLoopbackSession> {
    if opts.client_id.trim().is_empty() {
        bail!("OAuth client_id is required to start loopback login");
    }

    let listener = TcpListener::bind(("127.0.0.1", opts.callback_port))
        .await
        .with_context(|| {
            format!(
                "Failed to bind local OAuth callback listener on 127.0.0.1:{}",
                opts.callback_port
            )
        })?;
    let addr = listener
        .local_addr()
        .context("Failed to read bound OAuth callback listener address")?;

    let normalized_path = {
        let p = opts.callback_path.trim();
        if p.is_empty() {
            "/callback".to_string()
        } else if p.starts_with('/') {
            p.to_string()
        } else {
            format!("/{p}")
        }
    };

    let redirect_uri = format!("http://127.0.0.1:{}{}", addr.port(), normalized_path);
    let state = crate::new_uuid_v4().replace('-', "");
    let authorize_url = build_oauth_authorize_url(
        &opts.auth_base,
        &opts.client_id,
        &redirect_uri,
        &state,
        &opts.scopes,
    );

    let (callback_tx, callback_rx) = oneshot::channel::<Result<String>>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    #[derive(Clone)]
    struct CallbackState {
        expected_state: String,
        tx: Arc<Mutex<Option<oneshot::Sender<Result<String>>>>>,
    }

    #[derive(Deserialize)]
    struct CallbackQuery {
        code: Option<String>,
        state: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }

    let cb_state = CallbackState {
        expected_state: state.clone(),
        tx: Arc::new(Mutex::new(Some(callback_tx))),
    };

    let router = Router::new()
        .route(
            &normalized_path,
            get(
                |State(st): State<CallbackState>, Query(q): Query<CallbackQuery>| async move {
                    if let Some(err) = q.error.filter(|s| !s.trim().is_empty()) {
                        let desc = q.error_description.unwrap_or_default();
                        let msg = if desc.trim().is_empty() {
                            format!("Atlassian OAuth authorization failed: {err}")
                        } else {
                            format!("Atlassian OAuth authorization failed: {err} ({desc})")
                        };
                        if let Some(sender) = st.tx.lock().ok().and_then(|mut g| g.take()) {
                            let _ = sender.send(Err(anyhow!(msg.clone())));
                        }
                        return (
                            StatusCode::BAD_REQUEST,
                            Html(format!(
                                "<html><body><h2>Atlassian Sign-In Failed</h2><p>{}</p></body></html>",
                                html_escape(&msg)
                            )),
                        );
                    }

                    let received_state = q.state.as_deref().unwrap_or("").trim();
                    if received_state != st.expected_state {
                        let msg = "OAuth state mismatch (possible CSRF); please retry login.";
                        if let Some(sender) = st.tx.lock().ok().and_then(|mut g| g.take()) {
                            let _ = sender.send(Err(anyhow!(msg)));
                        }
                        return (
                            StatusCode::BAD_REQUEST,
                            Html(
                                "<html><body><h2>Authentication Error</h2><p>Invalid OAuth state parameter. Please retry from the terminal.</p></body></html>"
                                    .to_string(),
                            ),
                        );
                    }

                    match q.code.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) {
                        Some(code) => {
                            if let Some(sender) = st.tx.lock().ok().and_then(|mut g| g.take()) {
                                let _ = sender.send(Ok(code));
                            }
                            (
                                StatusCode::OK,
                                Html(
                                    "<html><body><h2>Atlassian Sign-In Complete</h2><p>You can close this browser tab and return to your terminal.</p></body></html>"
                                        .to_string(),
                                ),
                            )
                        }
                        None => {
                            let msg = "OAuth callback did not contain an authorization code.";
                            if let Some(sender) = st.tx.lock().ok().and_then(|mut g| g.take()) {
                                let _ = sender.send(Err(anyhow!(msg)));
                            }
                            (
                                StatusCode::BAD_REQUEST,
                                Html(
                                    "<html><body><h2>Authentication Error</h2><p>Missing authorization code.</p></body></html>"
                                        .to_string(),
                                ),
                            )
                        }
                    }
                },
            ),
        )
        .with_state(cb_state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    Ok(OAuthLoopbackSession {
        authorize_url,
        redirect_uri,
        state,
        callback_rx,
        shutdown_tx: Some(shutdown_tx),
    })
}

/// Runs the full Atlassian Cloud OAuth 2.0 (3LO) loopback flow:
/// starts a local HTTP listener, opens the consent URL in the default browser (when
/// `opts.open_browser` is `true`), verifies the returned `state` and `code`, exchanges
/// the code for tokens, and discovers all accessible Cloud sites.
pub async fn run_oauth_loopback_login(
    http: &reqwest::Client,
    opts: &OAuthLoopbackOptions,
) -> Result<(StoredCredentials, Vec<AccessibleResource>)> {
    let session = start_oauth_loopback_session(opts).await?;
    if opts.open_browser {
        if let Err(err) = open::that(&session.authorize_url) {
            eprintln!(
                "Could not open browser automatically ({err}). Open this URL manually:\n{}",
                session.authorize_url
            );
        }
    }
    session.finish(http, opts).await
}

async fn fetch_oauth_me_email(
    http: &reqwest::Client,
    api_base: &str,
    access_token: &str,
) -> Result<Option<String>> {
    if access_token.trim().is_empty() {
        return Ok(None);
    }
    let url = format!("{}/me", effective_api_base(api_base));
    let resp = http
        .get(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token.trim()),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let val: Value = resp.json().await?;
    Ok(val
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

async fn parse_oauth_token_response(
    resp: reqwest::Response,
    op_label: &str,
) -> Result<OAuthTokenResponse> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("Failed to read {op_label} response body"))?;
    if !status.is_success() {
        bail!(
            "{op_label} failed (HTTP {status}): {}",
            extract_oauth_error(&text)
        );
    }
    let parsed: OAuthTokenResponse = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse {op_label} JSON response"))?;
    if parsed.access_token.trim().is_empty() {
        bail!("{op_label} returned an empty access_token");
    }
    Ok(parsed)
}

fn extract_oauth_error(body: &str) -> String {
    if let Ok(val) = serde_json::from_str::<Value>(body) {
        let err = val.get("error").and_then(|v| v.as_str()).unwrap_or("").trim();
        let desc = val
            .get("error_description")
            .or_else(|| val.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        match (err, desc) {
            ("", "") => {}
            (e, "") => return e.to_string(),
            ("", d) => return d.to_string(),
            (e, d) => return format!("{e}: {d}"),
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty error response".to_string()
    } else {
        trimmed.chars().take(240).collect()
    }
}

fn split_oauth_scopes(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split_whitespace()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn effective_auth_base(auth_base: &str) -> String {
    let normalized = normalize_base_url(auth_base);
    if normalized.is_empty() {
        DEFAULT_OAUTH_AUTH_BASE.to_string()
    } else {
        normalized
    }
}

fn effective_api_base(api_base: &str) -> String {
    let normalized = normalize_base_url(api_base);
    if normalized.is_empty() {
        DEFAULT_OAUTH_API_BASE.to_string()
    } else {
        normalized
    }
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn global_refresh_mutex(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// How credentials supplied through environment variables (ATLASSIAN_PAT,
/// ATLASSIAN_API_TOKEN + ATLASSIAN_EMAIL, ...) rank against stored credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvCredentialPolicy {
    /// Environment credentials win over stored ones (single-site behaviour).
    #[default]
    Primary,
    /// Stored credentials win; the environment is used only when nothing is stored.
    FallbackOnly,
    /// Environment credentials are never used for this account.
    Never,
}

impl EnvCredentialPolicy {
    fn strength(self) -> u8 {
        match self {
            Self::Primary => 2,
            Self::FallbackOnly => 1,
            Self::Never => 0,
        }
    }

    /// The policy that lets the environment apply more often.
    pub fn strongest(self, other: Self) -> Self {
        if other.strength() > self.strength() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthManager {
    profile: String,
    direct_credentials: Arc<RwLock<Option<StoredCredentials>>>,
    fallback_path_override: Option<PathBuf>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    env_policy: EnvCredentialPolicy,
}

impl AuthManager {
    pub fn new(profile: impl Into<String>) -> Self {
        let profile = profile.into();
        let profile = if profile.trim().is_empty() {
            "default".to_string()
        } else {
            profile.trim().to_string()
        };
        let lock_key = format!("default::{}", profile);
        Self {
            profile,
            direct_credentials: Arc::new(RwLock::new(None)),
            fallback_path_override: None,
            refresh_lock: global_refresh_mutex(&lock_key),
            env_policy: EnvCredentialPolicy::Primary,
        }
    }

    /// Creates an `AuthManager` bound to a specific multi-site `account_id`.
    pub fn for_account(account_id: impl Into<String>) -> Self {
        Self::new(account_id)
    }

    /// Derives an `AuthManager` for `account_id` preserving this manager's fallback file path override.
    pub fn with_account(&self, account_id: impl Into<String>) -> Self {
        let mut mgr = Self::for_account(account_id);
        if let Some(ref path) = self.fallback_path_override {
            mgr = mgr.with_fallback_path(path.clone());
        }
        mgr
    }

    /// Creates an in-memory `AuthManager` with fixed credentials (never touches OS keyring or disk).
    pub fn with_credentials(profile: impl Into<String>, creds: StoredCredentials) -> Self {
        let profile = profile.into();
        let profile = if profile.trim().is_empty() {
            "default".to_string()
        } else {
            profile.trim().to_string()
        };
        Self {
            profile,
            direct_credentials: Arc::new(RwLock::new(Some(creds))),
            fallback_path_override: None,
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            env_policy: EnvCredentialPolicy::Primary,
        }
    }

    /// Alias kept for convenience when constructing static credentials in tests or callers.
    pub fn from_static(profile: impl Into<String>, creds: StoredCredentials) -> Self {
        Self::with_credentials(profile, creds)
    }

    /// Overrides the fallback credentials file path (and bypasses OS keyring) for isolated testing.
    pub fn with_fallback_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_key = format!("file:{}::{}", path.display(), self.profile);
        self.refresh_lock = global_refresh_mutex(&lock_key);
        self.fallback_path_override = Some(path);
        self
    }

    /// Sets how environment credentials rank against this account's stored credentials.
    pub fn with_env_policy(mut self, policy: EnvCredentialPolicy) -> Self {
        self.env_policy = policy;
        self
    }

    pub fn env_policy(&self) -> EnvCredentialPolicy {
        self.env_policy
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn account_id(&self) -> &str {
        &self.profile
    }

    fn effective_fallback_path(&self) -> PathBuf {
        self.fallback_path_override
            .clone()
            .unwrap_or_else(credentials_fallback_path)
    }

    fn use_keyring(&self) -> bool {
        self.fallback_path_override.is_none() && !keyring_disabled()
    }

    fn has_direct_credentials(&self) -> bool {
        self.direct_credentials
            .read()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    pub fn load_credentials(&self) -> Result<Option<StoredCredentials>> {
        // An isolated fallback file (tests, DI) never consults the process environment.
        self.load_credentials_with_env(self.fallback_path_override.is_none())
    }

    /// Resolution order by [EnvCredentialPolicy]:
    /// Primary = direct, env, keyring, file; FallbackOnly = direct, keyring, file, env;
    /// Never = direct, keyring, file.
    fn load_credentials_with_env(&self, env_enabled: bool) -> Result<Option<StoredCredentials>> {
        if let Ok(guard) = self.direct_credentials.read() {
            if let Some(ref creds) = *guard {
                return Ok(Some(creds.clone()));
            }
        }

        let env_enabled = env_enabled && self.env_policy != EnvCredentialPolicy::Never;
        if env_enabled && self.env_policy == EnvCredentialPolicy::Primary {
            if let Some(env_creds) = load_credentials_from_env() {
                return Ok(Some(env_creds));
            }
        }

        if self.use_keyring() {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &self.profile) {
                if let Ok(secret_json) = entry.get_password() {
                    if !secret_json.trim().is_empty() {
                        if let Ok(parsed) = serde_json::from_str::<StoredCredentials>(&secret_json)
                        {
                            return Ok(Some(parsed));
                        }
                    }
                }
            }
        }

        let fallback = self.effective_fallback_path();
        let stored = load_profile_from_file(&fallback, &self.profile)?;
        if stored.is_some() {
            return Ok(stored);
        }
        if env_enabled && self.env_policy == EnvCredentialPolicy::FallbackOnly {
            return Ok(load_credentials_from_env());
        }
        Ok(None)
    }

    pub fn save_credentials(&self, creds: &StoredCredentials) -> Result<()> {
        if self.has_direct_credentials() {
            if let Ok(mut guard) = self.direct_credentials.write() {
                *guard = Some(creds.clone());
            }
            return Ok(());
        }

        let fallback = self.effective_fallback_path();
        if !self.use_keyring() {
            return save_profile_to_file(&fallback, &self.profile, creds);
        }

        let json =
            serde_json::to_string_pretty(creds).context("Failed to serialize credentials")?;
        match keyring::Entry::new(KEYRING_SERVICE, &self.profile) {
            Ok(entry) => {
                if entry.set_password(&json).is_ok() {
                    // Mirror to fallback file for consistency across headless/CLI sessions.
                    let _ = save_profile_to_file(&fallback, &self.profile, creds);
                    return Ok(());
                }
                // Keyring failed (e.g., Windows Credential blob limit or headless daemon):
                // clean up any stale entry and persist to fallback file.
                let _ = entry.delete_credential();
                save_profile_to_file(&fallback, &self.profile, creds)
            }
            Err(_) => save_profile_to_file(&fallback, &self.profile, creds),
        }
    }

    pub fn delete_credentials(&self) -> Result<()> {
        if self.has_direct_credentials() {
            if let Ok(mut guard) = self.direct_credentials.write() {
                *guard = None;
            }
            return Ok(());
        }

        if self.use_keyring() {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &self.profile) {
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(_) => {}
                }
            }
        }
        let fallback = self.effective_fallback_path();
        delete_profile_from_file(&fallback, &self.profile)
    }

    /// Lists all stored accounts from the fallback credentials file (and includes this manager's
    /// in-memory or keyring account if not already present).
    pub fn list_accounts(&self) -> Result<Vec<(String, StoredCredentials)>> {
        let mut map = if self.has_direct_credentials() {
            BTreeMap::new()
        } else {
            load_credentials_map(&self.effective_fallback_path())?
        };
        if let Ok(Some(current)) = self.load_credentials() {
            map.entry(self.profile.clone()).or_insert(current);
        }
        Ok(map.into_iter().collect())
    }

    /// Ensures stored credentials are fresh. For `CloudOAuth` credentials expiring within
    /// [`OAUTH_REFRESH_MARGIN_SECS`], acquires a single-flight `tokio::sync::Mutex` so concurrent
    /// callers do not stampede token refresh (and rotate refresh tokens safely).
    pub async fn ensure_fresh_credentials(
        &self,
        http: &reqwest::Client,
        auth_base: &str,
    ) -> Result<StoredCredentials> {
        let creds = self.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `atlassian login` or `confluence login` first.")
        })?;
        if !creds.is_expired_within(OAUTH_REFRESH_MARGIN_SECS) {
            return Ok(creds);
        }

        let _guard = self.refresh_lock.lock().await;
        let current = self.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `atlassian login` or `confluence login` first.")
        })?;
        if !current.is_expired_within(OAUTH_REFRESH_MARGIN_SECS) {
            return Ok(current);
        }

        let refreshed = refresh_oauth_token(http, auth_base, &current).await?;
        self.save_credentials(&refreshed)?;
        Ok(refreshed)
    }

    /// Forces a single-flight OAuth token refresh (used on HTTP `401`). If another concurrent
    /// task has already refreshed past `stale_access_token`, reuses the newly saved token without
    /// issuing a duplicate refresh call.
    pub async fn force_refresh_credentials(
        &self,
        http: &reqwest::Client,
        auth_base: &str,
        stale_access_token: Option<&str>,
    ) -> Result<StoredCredentials> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `atlassian login` or `confluence login` first.")
        })?;
        if !current.is_oauth() {
            return Ok(current);
        }

        if let (Some(stale), Some(cur_token)) = (stale_access_token, current.oauth_access_token()) {
            if !stale.is_empty()
                && cur_token != stale
                && !current.is_expired_within(OAUTH_REFRESH_MARGIN_SECS)
            {
                return Ok(current);
            }
        }

        let refreshed = refresh_oauth_token(http, auth_base, &current).await?;
        self.save_credentials(&refreshed)?;
        Ok(refreshed)
    }

    pub fn login_with_options(&self, opts: &LoginOptions) -> Result<StoredCredentials> {
        let cfg = AppConfig::load().unwrap_or_default();
        let site_url = opts
            .site_url
            .as_deref()
            .map(normalize_base_url)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg.site_url.clone());

        if let Some(pat) = opts
            .pat
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let deployment = opts.deployment.unwrap_or_else(|| {
                if !site_url.is_empty() {
                    DeploymentKind::infer_from_url(&site_url)
                } else {
                    DeploymentKind::DataCenter
                }
            });
            let creds = StoredCredentials::BearerPat {
                site_url,
                token: pat.to_string(),
                deployment,
            };
            self.save_credentials(&creds)?;
            return Ok(creds);
        }

        let token = opts
            .token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Please provide an API token or Personal Access Token (PAT)."))?;
        let email = opts
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Email (or username) is required for API token login."))?;

        let deployment = opts.deployment.unwrap_or_else(|| {
            if !site_url.is_empty() {
                DeploymentKind::infer_from_url(&site_url)
            } else {
                DeploymentKind::Cloud
            }
        });

        let creds = StoredCredentials::ApiToken {
            site_url,
            email: email.to_string(),
            api_token: token.to_string(),
            deployment,
        };
        self.save_credentials(&creds)?;
        Ok(creds)
    }

    pub fn authorization_header(&self) -> Result<String> {
        let creds = self.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `atlassian login` or `confluence login` first.")
        })?;
        Ok(creds.authorization_header())
    }

    pub fn is_logged_in(&self) -> bool {
        self.load_credentials().ok().flatten().is_some()
    }

    pub fn status_summary(&self) -> String {
        match self.load_credentials() {
            Ok(Some(creds)) => creds.status_summary(),
            _ => "Not signed in".to_string(),
        }
    }
}

fn load_credentials_from_env() -> Option<StoredCredentials> {
    if let Some(pat) = first_nonempty_env(&["ATLASSIAN_PAT", "JIRA_PAT"]) {
        let (site_url, deployment) = resolve_env_site_and_deployment(DeploymentKind::DataCenter);
        return Some(StoredCredentials::BearerPat {
            site_url,
            token: pat,
            deployment,
        });
    }

    let api_token = first_nonempty_env(&["ATLASSIAN_API_TOKEN", "JIRA_API_TOKEN"]);
    let email = first_nonempty_env(&["ATLASSIAN_EMAIL", "JIRA_EMAIL"]);
    if let (Some(api_token), Some(email)) = (api_token, email) {
        let (site_url, deployment) = resolve_env_site_and_deployment(DeploymentKind::Cloud);
        return Some(StoredCredentials::ApiToken {
            site_url,
            email,
            api_token,
            deployment,
        });
    }

    None
}

/// Site URL the environment credentials belong to, or "None" when the environment
/// carries no credentials. Used to scope them to one site in multi-site configurations.
pub(crate) fn env_credentials_site_url() -> Option<String> {
    load_credentials_from_env().map(|creds| normalize_base_url(creds.site_url()))
}

/// Confluence-only Data Center PAT from ATLASSIAN_CONFLUENCE_PAT (alias CONFLUENCE_PAT).
pub(crate) fn env_confluence_pat() -> Option<String> {
    first_nonempty_env(&["ATLASSIAN_CONFLUENCE_PAT", "CONFLUENCE_PAT"])
}

/// Confluence root named by ATLASSIAN_CONFLUENCE_URL (alias CONFLUENCE_URL), normalized.
pub(crate) fn env_confluence_url() -> Option<String> {
    first_nonempty_env(&["ATLASSIAN_CONFLUENCE_URL", "CONFLUENCE_URL"])
        .map(|u| normalize_base_url(&u))
        .filter(|u| !u.is_empty())
}

fn resolve_env_site_and_deployment(default_deployment: DeploymentKind) -> (String, DeploymentKind) {
    let cfg = AppConfig::load().ok();
    let env_url =
        first_nonempty_env(&["ATLASSIAN_SITE_URL", "JIRA_URL"]).map(|u| normalize_base_url(&u));
    let site_url = env_url
        .clone()
        .or_else(|| {
            cfg.as_ref()
                .map(|c| c.site_url.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();

    let deployment = if let Some(dep) = first_nonempty_env(&["ATLASSIAN_DEPLOYMENT"]) {
        match dep.to_ascii_lowercase().as_str() {
            "cloud" => DeploymentKind::Cloud,
            "datacenter" | "dc" | "server" => DeploymentKind::DataCenter,
            _ => cfg
                .as_ref()
                .map(|c| c.deployment)
                .unwrap_or(default_deployment),
        }
    } else if let Some(ref u) = env_url {
        DeploymentKind::infer_from_url(u)
    } else if let Some(ref c) = cfg {
        if !c.site_url.is_empty() {
            c.deployment
        } else {
            default_deployment
        }
    } else {
        default_deployment
    };

    (site_url, deployment)
}

fn first_nonempty_env(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn keyring_disabled() -> bool {
    std::env::var("ATLASSIAN_DISABLE_KEYRING")
        .ok()
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "1" | "true" | "yes")
        })
        .unwrap_or(false)
}

type CredentialsMap = BTreeMap<String, StoredCredentials>;

fn load_credentials_map(path: &Path) -> Result<CredentialsMap> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Failed to read credentials file: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str::<CredentialsMap>(&raw)
        .with_context(|| format!("Failed to parse credentials file: {}", path.display()))
}

fn load_profile_from_file(path: &Path, profile: &str) -> Result<Option<StoredCredentials>> {
    let map = load_credentials_map(path)?;
    Ok(map.get(profile).cloned())
}

fn save_profile_to_file(path: &Path, profile: &str, creds: &StoredCredentials) -> Result<()> {
    let mut map = load_credentials_map(path).unwrap_or_default();
    map.insert(profile.to_string(), creds.clone());
    let json = serde_json::to_vec_pretty(&map).context("Failed to serialize credentials map")?;
    write_atomic(path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn delete_profile_from_file(path: &Path, profile: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut map = load_credentials_map(path).unwrap_or_default();
    if map.remove(profile).is_none() {
        return Ok(());
    }
    if map.is_empty() {
        fs::remove_file(path)
            .with_context(|| format!("Failed to delete credentials file: {}", path.display()))?;
        return Ok(());
    }
    let json = serde_json::to_vec_pretty(&map).context("Failed to serialize credentials map")?;
    write_atomic(path, &json)
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_0600(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::HeaderMap,
        routing::post,
        Json,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn clear(keys: &[&'static str]) -> Self {
            let mut saved = Vec::with_capacity(keys.len());
            for &k in keys {
                saved.push((k, std::env::var(k).ok()));
                std::env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    const ALL_AUTH_ENVS: &[&str] = &[
        "ATLASSIAN_PAT",
        "JIRA_PAT",
        "ATLASSIAN_API_TOKEN",
        "JIRA_API_TOKEN",
        "ATLASSIAN_EMAIL",
        "JIRA_EMAIL",
        "ATLASSIAN_SITE_URL",
        "JIRA_URL",
        "ATLASSIAN_DEPLOYMENT",
        "ATLASSIAN_DISABLE_KEYRING",
        "ATLASSIAN_CONFIG_DIR",
    ];

    async fn spawn_mock_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[test]
    fn test_cloud_basic_auth_and_status_summary() {
        let creds = StoredCredentials::ApiToken {
            site_url: "https://acme.atlassian.net".into(),
            email: "user@acme.com".into(),
            api_token: "super-secret-token".into(),
            deployment: DeploymentKind::Cloud,
        };
        let mgr = AuthManager::with_credentials("default", creds);
        assert!(mgr.is_logged_in());

        let header = mgr.authorization_header().unwrap();
        let expected = format!(
            "Basic {}",
            BASE64_STANDARD.encode("user@acme.com:super-secret-token")
        );
        assert_eq!(header, expected);

        let summary = mgr.status_summary();
        assert_eq!(summary, "Signed in (Cloud API Token: user@acme.com)");
        assert!(!summary.contains("super-secret-token"));
    }

    #[test]
    fn test_datacenter_bearer_pat_and_status_summary() {
        let creds = StoredCredentials::BearerPat {
            site_url: "https://jira.company.com".into(),
            token: "  pat-secret-xyz  ".into(),
            deployment: DeploymentKind::DataCenter,
        };
        let mgr = AuthManager::with_credentials("work", creds);
        assert_eq!(
            mgr.authorization_header().unwrap(),
            "Bearer pat-secret-xyz"
        );
        let summary = mgr.status_summary();
        assert_eq!(summary, "Signed in (Data Center PAT)");
        assert!(!summary.contains("pat-secret-xyz"));
    }

    #[test]
    fn test_cloud_oauth_and_shared_account_token_variants() {
        let shared = StoredCredentials::CloudAccountToken {
            email: "dev@acme.com".into(),
            api_token: "shared-tok".into(),
        };
        assert_eq!(shared.deployment(), DeploymentKind::Cloud);
        assert_eq!(
            shared.authorization_header(),
            format!("Basic {}", BASE64_STANDARD.encode("dev@acme.com:shared-tok"))
        );
        assert_eq!(
            shared.status_summary(),
            "Signed in (Cloud Shared API Token: dev@acme.com)"
        );

        let oauth = StoredCredentials::CloudOAuth {
            email: Some("alice@acme.com".into()),
            access_token: "oauth-access-1".into(),
            refresh_token: Some("oauth-refresh-1".into()),
            expires_at_epoch: now_epoch_secs() + 3600,
            client_id: "cid".into(),
            client_secret: Some("csec".into()),
            scopes: vec!["offline_access".into(), "read:jira-work".into()],
        };
        assert!(oauth.is_oauth());
        assert!(!oauth.is_expired_within(60));
        assert_eq!(oauth.authorization_header(), "Bearer oauth-access-1");
        assert_eq!(
            oauth.status_summary(),
            "Signed in (Cloud OAuth 2.0: alice@acme.com)"
        );

        // JSON serde roundtrip
        let json = serde_json::to_string(&oauth).unwrap();
        let decoded: StoredCredentials = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, oauth);
    }

    #[test]
    fn test_isolated_file_fallback_multi_profile_roundtrip_and_delete() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);

        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");

        let default_mgr = AuthManager::new("default").with_fallback_path(cred_path.clone());
        let dc_mgr = AuthManager::for_account("dc").with_fallback_path(cred_path.clone());

        assert!(!default_mgr.is_logged_in());
        assert_eq!(default_mgr.status_summary(), "Not signed in");
        assert!(default_mgr.authorization_header().is_err());

        let cloud_creds = StoredCredentials::ApiToken {
            site_url: "https://acme.atlassian.net".into(),
            email: "alice@acme.com".into(),
            api_token: "tok-1".into(),
            deployment: DeploymentKind::Cloud,
        };
        let dc_creds = StoredCredentials::BearerPat {
            site_url: "https://jira.internal".into(),
            token: "pat-2".into(),
            deployment: DeploymentKind::DataCenter,
        };

        default_mgr.save_credentials(&cloud_creds).unwrap();
        dc_mgr.save_credentials(&dc_creds).unwrap();

        let accounts = default_mgr.list_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].0, "dc");
        assert_eq!(accounts[1].0, "default");

        assert_eq!(
            default_mgr.load_credentials().unwrap().as_ref(),
            Some(&cloud_creds)
        );
        assert_eq!(dc_mgr.load_credentials().unwrap().as_ref(), Some(&dc_creds));

        default_mgr.delete_credentials().unwrap();
        assert!(default_mgr.load_credentials().unwrap().is_none());
        assert_eq!(dc_mgr.load_credentials().unwrap().as_ref(), Some(&dc_creds));

        dc_mgr.delete_credentials().unwrap();
        assert!(!cred_path.exists());
    }

    #[test]
    fn test_env_var_overrides_pat_and_api_token() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);
        let dir = tempdir().unwrap();
        std::env::set_var("ATLASSIAN_CONFIG_DIR", dir.path());
        std::env::set_var("ATLASSIAN_DISABLE_KEYRING", "1");

        // 1. API Token + Email via env
        std::env::set_var("ATLASSIAN_SITE_URL", "https://acme.atlassian.net/");
        std::env::set_var("ATLASSIAN_EMAIL", "bob@acme.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "api-123");

        let mgr = AuthManager::new("default");
        let loaded = mgr.load_credentials().unwrap().unwrap();
        assert_eq!(
            loaded,
            StoredCredentials::ApiToken {
                site_url: "https://acme.atlassian.net".into(),
                email: "bob@acme.com".into(),
                api_token: "api-123".into(),
                deployment: DeploymentKind::Cloud,
            }
        );

        // 2. PAT takes precedence over API Token
        std::env::set_var("JIRA_PAT", "dc-pat-999");
        std::env::set_var("JIRA_URL", "https://jira.corp.local");
        std::env::remove_var("ATLASSIAN_SITE_URL");

        let loaded_pat = mgr.load_credentials().unwrap().unwrap();
        assert_eq!(
            loaded_pat,
            StoredCredentials::BearerPat {
                site_url: "https://jira.corp.local".into(),
                token: "dc-pat-999".into(),
                deployment: DeploymentKind::DataCenter,
            }
        );
    }

    #[test]
    fn test_build_oauth_authorize_url_params_and_defaults() {
        let url_str = build_oauth_authorize_url(
            "",
            "my-client-id",
            "http://127.0.0.1:18791/callback",
            "state-xyz",
            &["read:jira-work", "read:confluence-content.all"],
        );
        let parsed = Url::parse(&url_str).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("auth.atlassian.com"));
        assert_eq!(parsed.path(), "/authorize");
        let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(params.get("audience").map(String::as_str), Some("api.atlassian.com"));
        assert_eq!(params.get("client_id").map(String::as_str), Some("my-client-id"));
        assert_eq!(params.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(params.get("state").map(String::as_str), Some("state-xyz"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:18791/callback")
        );
        let scope = params.get("scope").unwrap();
        assert!(scope.contains("offline_access"));
        assert!(scope.contains("read:jira-work"));
        assert!(scope.contains("read:confluence-content.all"));

        // Empty scopes fall back to DEFAULT_OAUTH_SCOPES
        let default_url = build_oauth_authorize_url::<&str>(
            "http://127.0.0.1:9999/",
            "cid",
            "http://127.0.0.1:1000/cb",
            "st",
            &[],
        );
        assert!(default_url.starts_with("http://127.0.0.1:9999/authorize?"));
        let default_params: HashMap<String, String> = Url::parse(&default_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert!(default_params
            .get("scope")
            .unwrap()
            .contains("read:jira-work"));
        assert!(default_params
            .get("scope")
            .unwrap()
            .contains("search:confluence"));
    }

    #[tokio::test]
    async fn test_oauth_exchange_refresh_rotation_and_accessible_resources() {
        let app = Router::new()
            .route(
                "/oauth/token",
                post(|Json(body): Json<Value>| async move {
                    let grant = body.get("grant_type").and_then(|v| v.as_str()).unwrap_or("");
                    if grant == "authorization_code" {
                        assert_eq!(body["client_id"], "cid-1");
                        assert_eq!(body["client_secret"], "csec-1");
                        assert_eq!(body["code"], "auth-code-123");
                        assert_eq!(body["redirect_uri"], "http://127.0.0.1:1234/callback");
                        Json(json!({
                            "access_token": "acc-v1",
                            "refresh_token": "ref-v1",
                            "expires_in": 3600,
                            "scope": "offline_access read:jira-work read:confluence-content.all"
                        }))
                    } else if grant == "refresh_token" {
                        assert_eq!(body["refresh_token"], "ref-v1");
                        Json(json!({
                            "access_token": "acc-v2",
                            "refresh_token": "ref-v2-rotated",
                            "expires_in": 7200,
                            "scope": "offline_access read:jira-work"
                        }))
                    } else {
                        panic!("unexpected grant_type: {grant}");
                    }
                }),
            )
            .route(
                "/oauth/token/accessible-resources",
                get(|headers: HeaderMap| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer acc-v2"
                    );
                    Json(json!([
                        {
                            "id": "cloud-id-1",
                            "url": "https://acme.atlassian.net/",
                            "name": "Acme Cloud",
                            "scopes": ["read:jira-work", "write:jira-work"],
                            "avatarUrl": "https://cdn.example.com/acme.png"
                        },
                        {
                            "id": "cloud-id-2",
                            "url": "https://side.atlassian.net",
                            "name": "Side Project",
                            "scopes": ["read:confluence-content.all"]
                        }
                    ]))
                }),
            );

        let base = spawn_mock_router(app).await;
        let http = reqwest::Client::new();

        let creds_v1 = exchange_oauth_code(
            &http,
            &base,
            "cid-1",
            Some("csec-1"),
            "auth-code-123",
            "http://127.0.0.1:1234/callback",
        )
        .await
        .unwrap();

        match &creds_v1 {
            StoredCredentials::CloudOAuth {
                access_token,
                refresh_token,
                scopes,
                ..
            } => {
                assert_eq!(access_token, "acc-v1");
                assert_eq!(refresh_token.as_deref(), Some("ref-v1"));
                assert_eq!(scopes.len(), 3);
            }
            _ => panic!("expected CloudOAuth"),
        }

        let creds_v2 = refresh_oauth_token(&http, &base, &creds_v1).await.unwrap();
        match &creds_v2 {
            StoredCredentials::CloudOAuth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "acc-v2");
                assert_eq!(refresh_token.as_deref(), Some("ref-v2-rotated"));
            }
            _ => panic!("expected CloudOAuth"),
        }

        let resources = fetch_accessible_resources(&http, &base, "acc-v2")
            .await
            .unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].id, "cloud-id-1");
        assert_eq!(resources[0].url, "https://acme.atlassian.net");
        assert_eq!(
            resources[0].avatar_url.as_deref(),
            Some("https://cdn.example.com/acme.png")
        );
        let profile = resources[0].to_site_profile("oauth-main");
        assert_eq!(profile.id, "acme");
        assert_eq!(profile.cloud_id.as_deref(), Some("cloud-id-1"));
        assert_eq!(profile.account_id, "oauth-main");
    }

    #[tokio::test]
    async fn test_oauth_loopback_session_happy_path_and_state_mismatch() {
        let app = Router::new()
            .route(
                "/oauth/token",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["code"], "good-code-777");
                    Json(json!({
                        "access_token": "loopback-acc",
                        "refresh_token": "loopback-ref",
                        "expires_in": 3600,
                        "scope": "offline_access read:me read:jira-work"
                    }))
                }),
            )
            .route(
                "/me",
                get(|| async {
                    Json(json!({ "email": "oauth-user@acme.com", "name": "OAuth User" }))
                }),
            )
            .route(
                "/oauth/token/accessible-resources",
                get(|| async {
                    Json(json!([{
                        "id": "cid-loopback",
                        "url": "https://loopback.atlassian.net",
                        "name": "Loopback Site",
                        "scopes": ["read:jira-work"]
                    }]))
                }),
            );
        let mock_base = spawn_mock_router(app).await;
        let http = reqwest::Client::new();

        let opts = OAuthLoopbackOptions {
            auth_base: mock_base.clone(),
            api_base: mock_base.clone(),
            client_id: "cid-loop".into(),
            client_secret: Some("csec-loop".into()),
            open_browser: false,
            timeout: Duration::from_secs(5),
            ..Default::default()
        };

        // 1. Happy path
        let session = start_oauth_loopback_session(&opts).await.unwrap();
        let cb_url = format!(
            "{}?code=good-code-777&state={}",
            session.redirect_uri, session.state
        );
        let browser_http = http.clone();
        tokio::spawn(async move {
            let resp = browser_http.get(&cb_url).send().await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        });
        let (creds, resources) = session.finish(&http, &opts).await.unwrap();
        assert_eq!(creds.oauth_access_token(), Some("loopback-acc"));
        assert_eq!(
            creds.status_summary(),
            "Signed in (Cloud OAuth 2.0: oauth-user@acme.com)"
        );
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].id, "cid-loopback");

        // 2. CSRF state mismatch is rejected
        let bad_session = start_oauth_loopback_session(&opts).await.unwrap();
        let bad_cb_url = format!("{}?code=evil&state=wrong-state", bad_session.redirect_uri);
        let browser_http2 = http.clone();
        tokio::spawn(async move {
            let resp = browser_http2.get(&bad_cb_url).send().await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        });
        let err = bad_session.finish(&http, &opts).await.unwrap_err();
        assert!(format!("{err:#}").contains("state mismatch"));
    }

    #[tokio::test]
    async fn test_single_flight_oauth_refresh_prevents_stampede() {
        let refresh_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = refresh_hits.clone();

        let app = Router::new().route(
            "/oauth/token",
            post(move |Json(body): Json<Value>| {
                let hits = hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    assert_eq!(body["refresh_token"], "initial-ref");
                    Json(json!({
                        "access_token": "fresh-access-token",
                        "refresh_token": "rotated-ref-token",
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let auth_base = spawn_mock_router(app).await;
        let http = reqwest::Client::new();

        let expired_creds = StoredCredentials::CloudOAuth {
            email: Some("team@acme.com".into()),
            access_token: "expired-access".into(),
            refresh_token: Some("initial-ref".into()),
            expires_at_epoch: 1, // already expired
            client_id: "cid".into(),
            client_secret: Some("csec".into()),
            scopes: vec!["offline_access".into()],
        };
        let auth = AuthManager::with_credentials("oauth-shared", expired_creds);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let a = auth.clone();
            let c = http.clone();
            let base = auth_base.clone();
            handles.push(tokio::spawn(async move {
                a.ensure_fresh_credentials(&c, &base).await.unwrap()
            }));
        }

        for h in handles {
            let creds = h.await.unwrap();
            assert_eq!(creds.oauth_access_token(), Some("fresh-access-token"));
        }
        // Only ONE HTTP refresh call despite 8 concurrent callers!
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
    }

    fn stored_api_token() -> StoredCredentials {
        StoredCredentials::ApiToken {
            site_url: "https://acme.atlassian.net".into(),
            email: "alice@acme.com".into(),
            api_token: "stored-token".into(),
            deployment: DeploymentKind::Cloud,
        }
    }

    /// Sets an environment PAT and returns a manager whose stored credentials live in "dir".
    fn manager_with_env_pat(dir: &Path, policy: EnvCredentialPolicy) -> AuthManager {
        std::env::set_var("ATLASSIAN_CONFIG_DIR", dir);
        std::env::set_var("ATLASSIAN_DISABLE_KEYRING", "1");
        std::env::set_var("ATLASSIAN_PAT", "env-pat");
        std::env::set_var("ATLASSIAN_SITE_URL", "https://jira.corp.example");
        AuthManager::for_account("acct")
            .with_fallback_path(dir.join("credentials.json"))
            .with_env_policy(policy)
    }

    fn env_pat() -> StoredCredentials {
        StoredCredentials::BearerPat {
            site_url: "https://jira.corp.example".into(),
            token: "env-pat".into(),
            deployment: DeploymentKind::DataCenter,
        }
    }

    #[test]
    fn env_pat_primary_for_single_site() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);
        let dir = tempdir().unwrap();
        let mgr = manager_with_env_pat(dir.path(), EnvCredentialPolicy::Primary);
        assert_eq!(mgr.env_policy(), EnvCredentialPolicy::Primary);
        assert_eq!(AuthManager::new("x").env_policy(), EnvCredentialPolicy::Primary);
        mgr.save_credentials(&stored_api_token()).unwrap();
        // Primary: the environment wins over the stored credential (unchanged behaviour).
        assert_eq!(mgr.load_credentials_with_env(true).unwrap(), Some(env_pat()));
    }

    #[test]
    fn env_pat_fallback_only_does_not_override_stored_credentials() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);
        let dir = tempdir().unwrap();
        let mgr = manager_with_env_pat(dir.path(), EnvCredentialPolicy::FallbackOnly);
        mgr.save_credentials(&stored_api_token()).unwrap();
        assert_eq!(
            mgr.load_credentials_with_env(true).unwrap(),
            Some(stored_api_token())
        );
    }

    #[test]
    fn env_pat_used_when_no_stored_credentials_under_fallback_only() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);
        let dir = tempdir().unwrap();
        let mgr = manager_with_env_pat(dir.path(), EnvCredentialPolicy::FallbackOnly);
        assert_eq!(mgr.load_credentials_with_env(true).unwrap(), Some(env_pat()));
    }

    #[test]
    fn env_policy_never_ignores_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);
        let dir = tempdir().unwrap();
        let mgr = manager_with_env_pat(dir.path(), EnvCredentialPolicy::Never);
        assert_eq!(mgr.load_credentials_with_env(true).unwrap(), None);
        mgr.save_credentials(&stored_api_token()).unwrap();
        assert_eq!(
            mgr.load_credentials_with_env(true).unwrap(),
            Some(stored_api_token())
        );
        // An isolated fallback file never consults the environment, whatever the policy.
        let isolated = manager_with_env_pat(dir.path(), EnvCredentialPolicy::Primary);
        assert_eq!(isolated.load_credentials().unwrap(), Some(stored_api_token()));
    }

    #[test]
    fn env_credential_policy_strongest_orders_primary_first() {
        use EnvCredentialPolicy::*;
        assert_eq!(FallbackOnly.strongest(Primary), Primary);
        assert_eq!(Primary.strongest(Never), Primary);
        assert_eq!(Never.strongest(FallbackOnly), FallbackOnly);
        assert_eq!(Never.strongest(Never), Never);
    }
}
