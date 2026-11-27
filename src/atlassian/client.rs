//! Core HTTP client with Cloud/DataCenter routing, OAuth 2.0 (3LO) gateway routing,
//! `401` single-flight OAuth refresh, `429 Retry-After` backoff, and `SiteClientPool`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::AtomicBool,
        Arc, OnceLock, RwLock,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    header::{HeaderMap, ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RETRY_AFTER},
    multipart::{Form, Part},
    Method, StatusCode,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use url::Url;

use crate::{
    atlassian::body::BodyEditError,
    auth::{
        AuthManager, EnvCredentialPolicy, StoredCredentials, DEFAULT_OAUTH_API_BASE,
        DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{normalize_base_url, normalize_url_for_match, AppConfig, DeploymentKind, SiteProfile},
    models::{AttachmentItem, UserSummary},
};

const MAX_RETRY_ATTEMPTS: usize = 3;
const MAX_RETRY_AFTER_SECS: u64 = 15;
const MAX_REDIRECT_HOPS: usize = 5;

#[derive(Debug, Error)]
#[error("{message}")]
pub(crate) struct HttpStatusError {
    pub status: StatusCode,
    pub message: String,
    pub body: String,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub(crate) struct CodedError {
    pub code: &'static str,
    pub message: String,
    pub details: Option<Value>,
}

/// Builds the structured error object: `{"code","message","http_status"?,"details"?}`.
pub(crate) fn error_payload(err: &anyhow::Error) -> Value {
    let message = format!("{err:#}");
    if let Some(coded) = err.downcast_ref::<CodedError>() {
        let mut obj = serde_json::Map::new();
        obj.insert("code".to_string(), json!(coded.code));
        obj.insert("message".to_string(), json!(message));
        if let Some(details) = &coded.details {
            obj.insert("details".to_string(), details.clone());
        }
        return Value::Object(obj);
    }
    if let Some(http) = err.downcast_ref::<HttpStatusError>() {
        let code = match http.status {
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            StatusCode::TOO_MANY_REQUESTS => "rate_limited",
            _ => "http_error",
        };
        let mut obj = serde_json::Map::new();
        obj.insert("code".to_string(), json!(code));
        obj.insert("message".to_string(), json!(message));
        obj.insert("http_status".to_string(), json!(http.status.as_u16()));
        if let Some(field_errors) = serde_json::from_str::<Value>(&http.body)
            .ok()
            .and_then(|v| v.get("errors").and_then(|e| e.as_object()).cloned())
            .filter(|map| !map.is_empty())
        {
            obj.insert(
                "details".to_string(),
                json!({ "field_errors": Value::Object(field_errors) }),
            );
        }
        return Value::Object(obj);
    }
    if err.downcast_ref::<BodyEditError>().is_some() {
        return json!({
            "code": "invalid_input",
            "message": message,
        });
    }
    json!({
        "code": "error",
        "message": message,
    })
}

pub(crate) fn http_status_code(err: &anyhow::Error) -> Option<StatusCode> {
    err.downcast_ref::<HttpStatusError>().map(|e| e.status)
}

pub(crate) fn is_not_found_error(err: &anyhow::Error) -> bool {
    http_status_code(err) == Some(StatusCode::NOT_FOUND)
}

/// Evidence that a 2xx response was served to an anonymous session although the request
/// carried a credential (Data Center answers 200 instead of 401 when anonymous access is
/// enabled or an SSO filter intercepts the request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnonymousSignal {
    /// "X-AUSERNAME: anonymous".
    HeaderAnonymous,
    /// "X-Seraph-LoginReason" reporting a failed or denied login (value kept).
    SeraphLoginFailed(String),
    /// An HTML page (SSO / login form) answered a JSON REST request.
    HtmlLoginPage,
    /// The identity endpoint returned an anonymous or empty user.
    AnonymousPayload,
}

impl AnonymousSignal {
    fn evidence(&self) -> String {
        match self {
            Self::HeaderAnonymous => "X-AUSERNAME: anonymous".to_string(),
            Self::SeraphLoginFailed(value) => format!("X-Seraph-LoginReason: {value}"),
            Self::HtmlLoginPage => "an HTML login page instead of JSON".to_string(),
            Self::AnonymousPayload => "anonymous user in the identity response".to_string(),
        }
    }
}

/// Marker stored in [HttpStatusError::body] for a 200-but-anonymous response.
const ANONYMOUS_BODY_MARKER: &str = "\"anonymous_signal\"";

/// Detects a 200-but-anonymous response from its headers (names are case-insensitive).
/// Pass "content_type" only for JSON REST requests: an HTML answer to those is a login page.
pub(crate) fn detect_anonymous_response(
    headers: &HeaderMap,
    content_type: Option<&str>,
) -> Option<AnonymousSignal> {
    let values = |name: &str| -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .collect()
    };
    if values("x-ausername")
        .iter()
        .any(|v| v.eq_ignore_ascii_case("anonymous"))
    {
        return Some(AnonymousSignal::HeaderAnonymous);
    }
    for value in values("x-seraph-loginreason") {
        let upper = value.to_ascii_uppercase();
        // "OK" is success and "OUT" (logged out before this request) is not a verdict.
        if ["AUTHENTICATED_FAILED", "AUTHENTICATION_DENIED", "AUTHORISATION_FAILED"]
            .iter()
            .any(|reason| upper.contains(reason))
        {
            return Some(AnonymousSignal::SeraphLoginFailed(value));
        }
    }
    if content_type
        .map(|ct| ct.trim().to_ascii_lowercase())
        .is_some_and(|ct| ct.starts_with("text/html"))
    {
        return Some(AnonymousSignal::HtmlLoginPage);
    }
    None
}

/// True when the error reports a 200-but-anonymous response (see [AnonymousSignal]).
pub(crate) fn is_anonymous_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HttpStatusError>()
        .is_some_and(|e| e.status == StatusCode::UNAUTHORIZED && e.body.contains(ANONYMOUS_BODY_MARKER))
}

fn is_json_rest_path(url: &str) -> bool {
    let path = Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string());
    path.contains("/rest/") || path.contains("/api/")
}

/// Why one product's identity probe failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// HTTP status when the server answered (a 200-but-anonymous answer is reported as 401).
    pub status: Option<u16>,
    /// The server answered, but as an anonymous user.
    pub anonymous: bool,
    pub message: String,
}

impl ProbeFailure {
    pub fn from_error(err: &anyhow::Error) -> Self {
        Self {
            status: http_status_code(err).map(|s| s.as_u16()),
            anonymous: is_anonymous_error(err),
            message: format!("{err:#}"),
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status == Some(404)
    }

    /// Rebuilds an error that keeps the HTTP status, so the JSON error code survives.
    pub fn to_error(&self) -> anyhow::Error {
        match self.status.and_then(|s| StatusCode::from_u16(s).ok()) {
            Some(status) => HttpStatusError {
                status,
                message: self.message.clone(),
                body: if self.anonymous {
                    json!({ "anonymous_signal": "probe" }).to_string()
                } else {
                    String::new()
                },
            }
            .into(),
            None => anyhow!("{}", self.message),
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        self.anonymous || matches!(self.status, Some(401) | Some(403))
    }

    /// "anonymous", "unauthorized", "not_found" or "error".
    pub fn label(&self) -> &'static str {
        if self.anonymous {
            "anonymous"
        } else if matches!(self.status, Some(401) | Some(403)) {
            "unauthorized"
        } else if self.is_not_found() {
            "not_found"
        } else {
            "error"
        }
    }
}

/// Result of asking each product who the credential belongs to. "None" = not probed
/// (no URL configured for that product).
#[derive(Debug, Clone, Default)]
pub struct IdentityProbe {
    pub jira: Option<Result<UserSummary, ProbeFailure>>,
    pub confluence: Option<Result<UserSummary, ProbeFailure>>,
}

impl IdentityProbe {
    pub fn jira_user(&self) -> Option<&UserSummary> {
        self.jira.as_ref().and_then(|r| r.as_ref().ok())
    }

    pub fn confluence_user(&self) -> Option<&UserSummary> {
        self.confluence.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The user a login belongs to. The "confluence" persona, a Jira 404 (a Confluence-only
    /// host) or a missing Jira URL require the Confluence probe to succeed; otherwise the
    /// Jira probe must succeed and a Confluence failure is not fatal.
    pub fn login_user(&self, persona: crate::config::Persona) -> Result<UserSummary> {
        let jira_not_found = matches!(&self.jira, Some(Err(failure)) if failure.is_not_found());
        let confluence_decides = persona == crate::config::Persona::Confluence
            || jira_not_found
            || self.jira.is_none();
        if confluence_decides {
            match (&self.confluence, &self.jira) {
                (Some(Ok(user)), _) => return Ok(user.clone()),
                // Neither product lives at this URL: report the Jira 404 as before.
                (Some(Err(failure)), Some(Err(jira))) if failure.is_not_found() && jira.is_not_found() => {
                    return Err(jira.to_error())
                }
                (Some(Err(failure)), _) => return Err(failure.to_error()),
                (None, None) => bail!("No site URL is configured, so the credential could not be verified."),
                (None, Some(_)) => {}
            }
        }
        match &self.jira {
            Some(Ok(user)) => Ok(user.clone()),
            Some(Err(failure)) => Err(failure.to_error()),
            None => bail!("No site URL is configured, so the credential could not be verified."),
        }
    }

    /// "ok", "skipped", or the failure label, per product.
    pub fn status_labels(&self) -> (&'static str, &'static str) {
        fn label(side: &Option<Result<UserSummary, ProbeFailure>>) -> &'static str {
            match side {
                None => "skipped",
                Some(Ok(_)) => "ok",
                Some(Err(failure)) => failure.label(),
            }
        }
        (label(&self.jira), label(&self.confluence))
    }
}

/// Environment variables that may name extra trusted root CA bundles.
/// Every variable that is set is loaded (not only the first one).
pub(crate) const EXTRA_CA_ENV_VARS: [&str; 3] =
    ["ATLASSIAN_EXTRA_CA_PEM", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"];

/// One extra-CA source and what loading it produced. Never carries certificate contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtraCaSource {
    pub env: &'static str,
    /// File path, or "<inline PEM>" when the variable holds the PEM text itself.
    pub path: String,
    pub certificates: usize,
    pub error: Option<String>,
}

impl ExtraCaSource {
    fn warning(&self) -> Option<String> {
        self.error.as_ref().map(|err| {
            format!(
                "ignoring extra CA bundle from {} ({}): {err}",
                self.env, self.path
            )
        })
    }
}

/// Loads extra root certificates named by [EXTRA_CA_ENV_VARS] through "lookup".
/// A missing or unparseable source is reported in the returned sources, never as a failure.
pub(crate) fn load_extra_ca_sources(
    lookup: impl Fn(&str) -> Option<String>,
) -> (Vec<reqwest::Certificate>, Vec<ExtraCaSource>) {
    let mut certs = Vec::new();
    let mut sources = Vec::new();
    for env in EXTRA_CA_ENV_VARS {
        let Some(raw) = lookup(env)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        // Only ATLASSIAN_EXTRA_CA_PEM may hold the PEM text itself; the other two are paths.
        let inline = env == "ATLASSIAN_EXTRA_CA_PEM" && raw.starts_with("-----BEGIN");
        let (path, bytes) = if inline {
            (
                "<inline PEM>".to_string(),
                Ok(raw.replace("\\n", "\n").into_bytes()),
            )
        } else {
            let read = std::fs::read(&raw).map_err(|e| format!("cannot read file: {e}"));
            (raw, read)
        };
        let parsed = bytes.and_then(|b| {
            reqwest::Certificate::from_pem_bundle(&b).map_err(|e| format!("cannot parse PEM: {e}"))
        });
        match parsed {
            Ok(list) if list.is_empty() => sources.push(ExtraCaSource {
                env,
                path,
                certificates: 0,
                error: Some("no PEM certificates found".to_string()),
            }),
            Ok(list) => {
                sources.push(ExtraCaSource {
                    env,
                    path,
                    certificates: list.len(),
                    error: None,
                });
                certs.extend(list);
            }
            Err(error) => sources.push(ExtraCaSource {
                env,
                path,
                certificates: 0,
                error: Some(error),
            }),
        }
    }
    (certs, sources)
}

/// Extra-CA sources from the process environment (for "atlassian doctor").
pub(crate) fn extra_ca_sources() -> Vec<ExtraCaSource> {
    load_extra_ca_sources(|name| std::env::var(name).ok()).1
}

fn extra_ca_certificates_with(
    lookup: impl Fn(&str) -> Option<String>,
) -> (Vec<reqwest::Certificate>, Vec<String>) {
    let (certs, sources) = load_extra_ca_sources(lookup);
    let warnings = sources.iter().filter_map(ExtraCaSource::warning).collect();
    (certs, warnings)
}

/// Extra root certificates from ATLASSIAN_EXTRA_CA_PEM, SSL_CERT_FILE and NODE_EXTRA_CA_CERTS,
/// plus one warning per source that could not be used.
fn extra_ca_certificates() -> (Vec<reqwest::Certificate>, Vec<String>) {
    extra_ca_certificates_with(|name| std::env::var(name).ok())
}

/// Prints TLS trust-store warnings once per process, to stderr only (stdout carries
/// JSON and the MCP stdio protocol).
fn warn_tls_once(warnings: &[String]) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if warnings.is_empty() {
        return;
    }
    ONCE.call_once(|| {
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
    });
}

fn base_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(format!("atlassian-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
}

/// The one place HTTP clients are configured: user agent, 30 s timeout, the OS trust store
/// plus bundled Mozilla roots (reqwest features), and any extra corporate root CAs.
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    let (certs, warnings) = extra_ca_certificates();
    warn_tls_once(&warnings);
    let mut builder = base_http_client_builder();
    for cert in certs {
        builder = builder.add_root_certificate(cert);
    }
    builder
}

/// Builds a client from [http_client_builder] after "customize". If the extra CA
/// certificates are rejected at build time, falls back to the OS and bundled roots
/// instead of leaving the user without any client.
pub(crate) fn build_http_client(
    customize: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    match customize(http_client_builder()).build() {
        Ok(client) => Ok(client),
        Err(err) => {
            if extra_ca_certificates().0.is_empty() {
                return Err(err);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                eprintln!(
                    "warning: extra CA certificates were rejected ({err}); continuing with the OS and bundled trust roots only"
                );
            });
            customize(base_http_client_builder()).build()
        }
    }
}

fn build_shared_http_client() -> Result<reqwest::Client> {
    build_http_client(|builder| builder).context("Failed to initialize HTTP client")
}

fn error_chain_text(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

/// Turns a TLS trust failure into an actionable message. "chain" is the rendered error
/// chain: reqwest does not expose the rustls error type through a stable API, so this
/// matches on text deliberately.
pub(crate) fn tls_recovery_hint(chain: &str, host: &str) -> Option<String> {
    let lower = chain.to_ascii_lowercase();
    let reason = if lower.contains("unknownissuer") || lower.contains("unknown issuer") {
        "UnknownIssuer"
    } else if lower.contains("self-signed")
        || lower.contains("self signed")
        || lower.contains("unable to get local issuer")
    {
        "self-signed or private root"
    } else if lower.contains("invalid peer certificate") {
        "invalid peer certificate"
    } else {
        return None;
    };
    Some(format!(
        "TLS certificate for {host} is not trusted ({reason}). If your company uses its own root CA, set ATLASSIAN_EXTRA_CA_PEM=<path to PEM bundle> (SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also honoured), then run \"atlassian doctor --probe\"."
    ))
}

/// Wraps a transport error, adding the TLS recovery hint when the chain shows a trust failure.
fn send_error(err: reqwest::Error) -> anyhow::Error {
    let host = err
        .url()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "the server".to_string());
    let hint = tls_recovery_hint(&error_chain_text(&err), &host);
    let err = anyhow::Error::new(err);
    match hint {
        Some(hint) => err.context(format!("Failed to send Atlassian API request: {hint}")),
        None => err.context("Failed to send Atlassian API request"),
    }
}

#[derive(Clone)]
pub struct AtlassianClient {
    pub(crate) http: reqwest::Client,
    /// Lazily built no-redirect client for attachment downloads (shared by clones).
    pub(crate) download_http: Arc<OnceLock<reqwest::Client>>,
    pub(crate) config: AppConfig,
    pub(crate) auth: AuthManager,
    /// Separate credential for requests under the Confluence root (Data Center PATs are
    /// per application). "None" means every request uses "auth".
    pub(crate) confluence_auth: Option<AuthManager>,
    pub(crate) me_cache: Arc<RwLock<Option<UserSummary>>>,
    pub(crate) space_id_cache: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// Remembers per client when Cloud `/rest/api/3/project/search` returned 404/405/400 so
    /// subsequent `list_projects` calls go straight to `/rest/api/3/project`.
    pub(crate) project_search_fallback: Arc<AtomicBool>,
    pub(crate) site_id: String,
    pub(crate) cloud_id: Option<String>,
    pub(crate) api_gateway_base: String,
    pub(crate) oauth_auth_base: String,
}

impl AtlassianClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = build_shared_http_client()?;
        let site_id = config
            .default_site_profile()
            .map(|s| s.id)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        Ok(Self {
            http,
            download_http: Arc::new(OnceLock::new()),
            config,
            auth,
            confluence_auth: None,
            me_cache: Arc::new(RwLock::new(None)),
            space_id_cache: Arc::new(RwLock::new(HashMap::new())),
            project_search_fallback: Arc::new(AtomicBool::new(false)),
            site_id,
            cloud_id: None,
            api_gateway_base: DEFAULT_OAUTH_API_BASE.to_string(),
            oauth_auth_base: DEFAULT_OAUTH_AUTH_BASE.to_string(),
        })
    }

    pub fn with_site_id(mut self, site_id: impl Into<String>) -> Self {
        let trimmed = site_id.into().trim().to_string();
        if !trimmed.is_empty() {
            self.site_id = trimmed;
        }
        self
    }

    /// Creates a site-scoped client supporting OAuth 3LO gateway routing when `site.cloud_id` is `Some`.
    pub fn for_site(
        site: &SiteProfile,
        auth: AuthManager,
        api_gateway_base: Option<String>,
    ) -> Result<Self> {
        let http = build_shared_http_client()?;
        Ok(Self::for_site_with_http(site, auth, http, api_gateway_base))
    }

    /// Creates a site-scoped client sharing an existing `reqwest::Client` connection pool.
    pub fn for_site_with_http(
        site: &SiteProfile,
        auth: AuthManager,
        http: reqwest::Client,
        api_gateway_base: Option<String>,
    ) -> Self {
        let config = AppConfig {
            site_url: normalize_base_url(&site.site_url),
            confluence_url: normalize_base_url(&site.confluence_url),
            deployment: site.deployment,
            profile: site.account_id.clone(),
            ..AppConfig::default()
        };

        let gateway = api_gateway_base
            .as_deref()
            .map(normalize_base_url)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_OAUTH_API_BASE.to_string());
        let cloud_id = site
            .cloud_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Self {
            http,
            download_http: Arc::new(OnceLock::new()),
            config,
            auth,
            confluence_auth: None,
            me_cache: Arc::new(RwLock::new(None)),
            space_id_cache: Arc::new(RwLock::new(HashMap::new())),
            project_search_fallback: Arc::new(AtomicBool::new(false)),
            site_id: if site.id.trim().is_empty() {
                "default".to_string()
            } else {
                site.id.trim().to_string()
            },
            cloud_id,
            api_gateway_base: gateway,
            oauth_auth_base: DEFAULT_OAUTH_AUTH_BASE.to_string(),
        }
    }

    /// Overrides the OAuth token endpoint base URL (defaults to `https://auth.atlassian.com`).
    pub fn with_oauth_auth_base(mut self, auth_base: impl Into<String>) -> Self {
        let normalized = normalize_base_url(&auth_base.into());
        if !normalized.is_empty() {
            self.oauth_auth_base = normalized;
        }
        self
    }

    /// Convenience constructor for tests and programmatic callers with explicit base URLs.
    pub fn with_base_urls(
        site_url: impl Into<String>,
        confluence_url: impl Into<String>,
        deployment: DeploymentKind,
        auth: AuthManager,
    ) -> Result<Self> {
        let config = AppConfig {
            site_url: normalize_base_url(&site_url.into()),
            confluence_url: normalize_base_url(&confluence_url.into()),
            deployment,
            ..AppConfig::default()
        };
        Self::new(config, auth)
    }

    /// Copies workspace-wide defaults (default project/board/space, download directory,
    /// sync interval, OAuth client) from the loaded configuration into this site client.
    /// Site identity (URLs, deployment, profile, site lists) is left untouched.
    pub fn with_workspace_defaults(mut self, base: &AppConfig) -> Self {
        self.config.default_project = base.default_project.clone();
        self.config.default_board_id = base.default_board_id;
        self.config.default_space_key = base.default_space_key.clone();
        self.config.download_dir = base.download_dir.clone();
        self.config.sync_interval_secs = base.sync_interval_secs;
        self.config.oauth_client_id = base.oauth_client_id.clone();
        self.config.oauth_client_secret = base.oauth_client_secret.clone();
        self
    }

    /// Uses "auth" for every request under the Confluence root instead of the site credential.
    pub fn with_confluence_auth(mut self, auth: AuthManager) -> Self {
        self.confluence_auth = Some(auth);
        self
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    /// Credential used for Confluence requests (the site credential unless a separate one is set).
    pub fn confluence_auth(&self) -> &AuthManager {
        self.confluence_auth.as_ref().unwrap_or(&self.auth)
    }

    /// Account id of the separate Confluence credential, when one is configured.
    pub fn confluence_credential_account(&self) -> Option<&str> {
        self.confluence_auth.as_ref().map(|a| a.account_id())
    }

    /// True when "target_url" lies under this client's Confluence root at a path-segment
    /// boundary ("https://wiki.corp/confluence2" is not under "https://wiki.corp/confluence").
    pub(crate) fn is_confluence_url(&self, target_url: &str) -> bool {
        let site_root = normalize_base_url(self.site_url());
        [self.confluence_root_url(), self.confluence_api_base_url()]
            .iter()
            .map(|root| normalize_base_url(root))
            // A Confluence root identical to the Jira root cannot be told apart by URL.
            .filter(|root| !root.is_empty() && *root != site_root)
            .any(|root| strip_url_prefix(target_url, &root).is_some())
    }

    /// Selects the credential for "target_url": the Confluence credential only for URLs
    /// under the Confluence root, the site credential for everything else.
    pub(crate) fn auth_for_url(&self, target_url: &str) -> &AuthManager {
        match &self.confluence_auth {
            Some(conf) if self.is_confluence_url(target_url) => conf,
            _ => &self.auth,
        }
    }

    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    pub fn cloud_id(&self) -> Option<&str> {
        self.cloud_id.as_deref()
    }

    pub fn deployment(&self) -> DeploymentKind {
        self.config.deployment
    }

    /// Human-facing Atlassian site URL (always preserved for browser `web_url` links).
    pub fn site_url(&self) -> &str {
        &self.config.site_url
    }

    /// Human-facing Confluence root URL (always preserved for browser `web_url` links).
    pub fn confluence_root_url(&self) -> String {
        self.config.effective_confluence_root()
    }

    /// Effective Jira REST / Agile API base URL:
    /// - OAuth 3LO Cloud (`cloud_id` present): `{api_gateway_base}/ex/jira/{cloud_id}`
    /// - Direct Cloud / Data Center: `{site_url}`
    pub fn jira_api_base_url(&self) -> String {
        if let Some(cid) = self.cloud_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            format!(
                "{}/ex/jira/{}",
                normalize_base_url(&self.api_gateway_base),
                urlencoding::encode(cid)
            )
        } else {
            normalize_base_url(&self.config.site_url)
        }
    }

    /// Effective Confluence REST API base URL:
    /// - OAuth 3LO Cloud (`cloud_id` present): `{api_gateway_base}/ex/confluence/{cloud_id}/wiki`
    /// - Direct Cloud / Data Center: `{effective_confluence_root}`
    pub fn confluence_api_base_url(&self) -> String {
        if let Some(cid) = self.cloud_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            format!(
                "{}/ex/confluence/{}/wiki",
                normalize_base_url(&self.api_gateway_base),
                urlencoding::encode(cid)
            )
        } else {
            self.config.effective_confluence_root()
        }
    }

    /// Translates human-facing site / Confluence URLs into Atlassian OAuth 3LO Gateway URLs
    /// when `cloud_id` is configured, while leaving non-OAuth or already-routed URLs intact.
    pub(crate) fn resolve_api_url(&self, url: &str) -> String {
        let Some(cid) = self.cloud_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return url.to_string();
        };

        let gateway = normalize_base_url(&self.api_gateway_base);
        let gateway_ex_prefix = format!("{gateway}/ex/");
        if url.starts_with(&gateway_ex_prefix) {
            return url.to_string();
        }

        // Match Confluence root first because `{site_url}/wiki` is a prefix extension of `{site_url}`.
        let conf_root = self.confluence_root_url();
        if !conf_root.is_empty() {
            if let Some(rest) = strip_url_prefix(url, &conf_root) {
                return format!(
                    "{}/ex/confluence/{}/wiki{rest}",
                    gateway,
                    urlencoding::encode(cid)
                );
            }
        }

        let site_root = normalize_base_url(self.site_url());
        if !site_root.is_empty() {
            if let Some(rest) = strip_url_prefix(url, &site_root) {
                return format!(
                    "{}/ex/jira/{}{rest}",
                    gateway,
                    urlencoding::encode(cid)
                );
            }
        }

        url.to_string()
    }

    pub fn cached_me(&self) -> Option<UserSummary> {
        self.me_cache.read().ok().and_then(|g| g.clone())
    }

    pub fn set_cached_me(&self, me: UserSummary) {
        if let Ok(mut g) = self.me_cache.write() {
            *g = Some(me);
        }
    }

    pub(crate) fn require_site_url(&self) -> Result<String> {
        let url = normalize_base_url(self.site_url());
        if !url.is_empty() {
            return Ok(url);
        }
        let api_base = self.jira_api_base_url();
        if !api_base.is_empty() {
            return Ok(api_base);
        }
        bail!("Atlassian site URL is not configured. Run `atlassian login --url <URL>` or set ATLASSIAN_SITE_URL.");
    }

    pub(crate) fn require_confluence_root(&self) -> Result<String> {
        let url = self.confluence_root_url();
        if !url.trim().is_empty() {
            return Ok(url);
        }
        let api_base = self.confluence_api_base_url();
        if !api_base.trim().is_empty() {
            return Ok(api_base);
        }
        bail!("Confluence URL is not configured. Run `confluence login --url <URL>` or set ATLASSIAN_CONFLUENCE_URL.");
    }

    pub async fn get_myself(&self) -> Result<UserSummary> {
        let base = self.require_site_url()?;
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{base}/rest/api/3/myself"),
            DeploymentKind::DataCenter => format!("{base}/rest/api/2/myself"),
        };
        let val: Value = self.get_json(&url).await?;
        let me = parse_user_summary(&val, self.deployment());
        // An empty identity ("{}") is an anonymous answer, not a user: never cache it.
        if me.id.trim().is_empty() {
            return Err(self.anonymous_error(&self.resolve_api_url(&url), &AnonymousSignal::AnonymousPayload));
        }
        self.set_cached_me(me.clone());
        Ok(me)
    }

    /// Returns the cached signed-in Jira user for this client if already fetched, otherwise
    /// queries `/myself` and caches the verified identity.
    pub async fn get_myself_cached(&self) -> Result<UserSummary> {
        if let Some(me) = self.cached_me().filter(|u| !u.id.trim().is_empty()) {
            return Ok(me);
        }
        self.get_myself().await
    }

    /// "Confluence" or "Jira", for messages about "target_url".
    fn product_for_url(&self, target_url: &str) -> &'static str {
        let path = Url::parse(target_url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        let jira_path = ["/rest/api/2/", "/rest/api/3/", "/rest/agile/", "/rest/greenhopper/"]
            .iter()
            .any(|p| path.contains(p));
        let under_confluence = [self.confluence_root_url(), self.confluence_api_base_url()]
            .iter()
            .map(|root| normalize_base_url(root))
            .filter(|root| !root.is_empty())
            .any(|root| strip_url_prefix(target_url, &root).is_some());
        if self.is_confluence_url(target_url) || (under_confluence && !jira_path) {
            "Confluence"
        } else {
            "Jira"
        }
    }

    /// Builds the 401-style error for a 200-but-anonymous response, so the existing 401
    /// handling (auth pause, error formatting) applies. Never includes the credential.
    pub(crate) fn anonymous_error(&self, target_url: &str, signal: &AnonymousSignal) -> anyhow::Error {
        let host = Url::parse(target_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "the server".to_string());
        let product = self.product_for_url(target_url);
        let hint = if product == "Confluence" && self.deployment() == DeploymentKind::DataCenter {
            format!(
                "Data Center PATs are per application — a Jira PAT does not work on Confluence. Set ATLASSIAN_CONFLUENCE_PAT or run \"atlassian login --site {} --confluence-pat <PAT>\" (stored under confluence_account_id), then \"atlassian doctor --probe\".",
                self.site_id()
            )
        } else if self.deployment() == DeploymentKind::DataCenter {
            "Check the PAT and run \"atlassian login --url <URL> --pat <PAT>\" again, then \"atlassian doctor --probe\".".to_string()
        } else {
            "Run \"atlassian login\" again, then \"atlassian doctor --probe\".".to_string()
        };
        let message = format!(
            "{product} at {host} answered HTTP 200 as an anonymous user ({}): the credential was not accepted. {hint}",
            signal.evidence()
        );
        let kind = match signal {
            AnonymousSignal::HeaderAnonymous => "x-ausername",
            AnonymousSignal::SeraphLoginFailed(_) => "x-seraph-loginreason",
            AnonymousSignal::HtmlLoginPage => "html-login-page",
            AnonymousSignal::AnonymousPayload => "anonymous-payload",
        };
        HttpStatusError {
            status: StatusCode::UNAUTHORIZED,
            message,
            body: json!({ "anonymous_signal": kind }).to_string(),
        }
        .into()
    }

    /// Checks a successful JSON-API response for signs that it was served anonymously.
    fn anonymous_signal_for(&self, target_url: &str, resp: &reqwest::Response) -> Option<AnonymousSignal> {
        let headers = resp.headers();
        let has_body = headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .is_none_or(|len| len > 0);
        let content_type = if resp.status() == StatusCode::OK && has_body && is_json_rest_path(target_url) {
            headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
        } else {
            None
        };
        detect_anonymous_response(headers, content_type)
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let target_url = self.resolve_api_url(url);
        let resp = self
            .send_with_retry(&target_url, |auth_header| {
                Ok(self
                    .http
                    .get(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json"))
            })
            .await?;
        decode_json_response(resp, &target_url).await
    }

    pub(crate) async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T> {
        self.post_json_with_retry(url, body, true).await
    }

    pub(crate) async fn post_json_once<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T> {
        self.post_json_with_retry(url, body, false).await
    }

    async fn post_json_with_retry<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
        retry_statuses: bool,
    ) -> Result<T> {
        let target_url = self.resolve_api_url(url);
        let resp = self
            .send_with_retry_policy(
                &target_url,
                |auth_header| {
                    Ok(self
                        .http
                        .post(&target_url)
                        .header(AUTHORIZATION, auth_header)
                        .header(ACCEPT, "application/json")
                        .json(body))
                },
                retry_statuses,
            )
            .await?;
        decode_json_response(resp, &target_url).await
    }

    pub(crate) async fn post_json_empty<B: Serialize>(&self, url: &str, body: &B) -> Result<()> {
        let target_url = self.resolve_api_url(url);
        let _ = self
            .send_with_retry(&target_url, |auth_header| {
                Ok(self
                    .http
                    .post(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T> {
        let target_url = self.resolve_api_url(url);
        let resp = self
            .send_with_retry(&target_url, |auth_header| {
                Ok(self
                    .http
                    .put(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        decode_json_response(resp, &target_url).await
    }

    pub(crate) async fn put_json_empty<B: Serialize>(&self, url: &str, body: &B) -> Result<()> {
        let target_url = self.resolve_api_url(url);
        let _ = self
            .send_with_retry(&target_url, |auth_header| {
                Ok(self
                    .http
                    .put(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_req(&self, url: &str) -> Result<()> {
        let target_url = self.resolve_api_url(url);
        let _ = self
            .send_with_retry(&target_url, |auth_header| {
                Ok(self
                    .http
                    .delete(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json"))
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn put_without_body(&self, url: &str) -> Result<()> {
        let target_url = self.resolve_api_url(url);
        self.send_with_retry(&target_url, |auth_header| {
            Ok(self
                .http
                .put(&target_url)
                .header(AUTHORIZATION, auth_header)
                .header(ACCEPT, "application/json"))
        })
        .await?;
        Ok(())
    }

    pub(crate) async fn post_multipart(
        &self,
        url: &str,
        field_name: &str,
        file_name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
        extra_text: Option<(&str, &str)>,
    ) -> Result<Value> {
        let target_url = self.resolve_api_url(url);
        let resp = self
            .send_with_retry(&target_url, |auth_header| {
                let part = Part::bytes(bytes.clone())
                    .file_name(file_name.to_string())
                    .mime_str(mime_type)
                    .unwrap_or_else(|_| {
                        Part::bytes(bytes.clone()).file_name(file_name.to_string())
                    });
                let mut form = Form::new().part(field_name.to_string(), part);
                if let Some((name, value)) = extra_text {
                    form = form.text(name.to_owned(), value.to_owned());
                }
                Ok(self
                    .http
                    .post(&target_url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .header("X-Atlassian-Token", "no-check")
                    .multipart(form))
            })
            .await?;
        decode_json_response(resp, &target_url).await
    }

    /// Fetches binary bytes while preserving `Authorization` on same-origin redirects
    /// and stripping it on cross-origin redirects (e.g., signed Atlassian Media / S3 CDN URLs).
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let target_url = self.resolve_api_url(url);
        // Hop 0 picks the credential by target; cross-origin hops never receive it (below).
        let auth = self.auth_for_url(&target_url);
        let (mut auth_header, mut oauth_token) = self.resolve_auth_header(auth).await?;
        let initial_url = Url::parse(&target_url)
            .with_context(|| format!("Invalid attachment URL: {target_url}"))?;
        let no_redirect_client = self.download_http_client()?;

        let mut current_url = initial_url.clone();
        let mut refreshed_on_401 = false;
        for _hop in 0..=MAX_REDIRECT_HOPS {
            let send_auth = is_same_origin(&initial_url, &current_url);
            let resp = loop {
                let res = send_request_with_retry_client(&no_redirect_client, || {
                    let mut req = no_redirect_client
                        .request(Method::GET, current_url.clone())
                        .header(ACCEPT, "*/*");
                    if send_auth {
                        req = req.header(AUTHORIZATION, auth_header.as_str());
                    }
                    Ok(req)
                })
                .await;

                match res {
                    Ok(r) => break r,
                    Err(err) => {
                        let is_401 = err
                            .downcast_ref::<HttpStatusError>()
                            .is_some_and(|e| e.status == StatusCode::UNAUTHORIZED);
                        if is_401 && send_auth && oauth_token.is_some() && !refreshed_on_401 {
                            let refreshed = auth
                                .force_refresh_credentials(
                                    &self.http,
                                    &self.oauth_auth_base,
                                    oauth_token.as_deref(),
                                )
                                .await?;
                            auth_header = refreshed.authorization_header();
                            oauth_token = refreshed.oauth_access_token().map(str::to_string);
                            refreshed_on_401 = true;
                            continue;
                        }
                        return Err(err);
                    }
                }
            };

            let status = resp.status();
            if status.is_redirection() {
                let loc = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        anyhow!("Redirect response ({status}) is missing Location header")
                    })?;
                current_url = current_url
                    .join(loc)
                    .with_context(|| format!("Failed to resolve redirect URL: {loc}"))?;
                continue;
            }

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let message = extract_atlassian_error(status, &body);
                return Err(HttpStatusError {
                    status,
                    message,
                    body,
                }
                .into());
            }

            let bytes = resp
                .bytes()
                .await
                .context("Failed to read attachment response bytes")?;
            return Ok(bytes.to_vec());
        }

        bail!("Attachment download exceeded max redirects ({MAX_REDIRECT_HOPS}): {target_url}")
    }

    /// No-redirect client for attachment downloads, built once per client (it loads the
    /// OS trust store, which is too slow to repeat for every attachment).
    fn download_http_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.download_http.get() {
            return Ok(client.clone());
        }
        let client = build_http_client(|builder| {
            builder
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
        })
        .context("Failed to initialize download HTTP client")?;
        Ok(self.download_http.get_or_init(|| client).clone())
    }

    async fn resolve_auth_header(&self, auth: &AuthManager) -> Result<(String, Option<String>)> {
        let creds = auth.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `atlassian login` or `confluence login` first.")
        })?;
        match creds {
            StoredCredentials::CloudOAuth { .. } => {
                let fresh = auth
                    .ensure_fresh_credentials(&self.http, &self.oauth_auth_base)
                    .await?;
                let token = fresh.oauth_access_token().map(str::to_string);
                Ok((fresh.authorization_header(), token))
            }
            other => Ok((other.authorization_header(), None)),
        }
    }

    async fn send_with_retry<F>(&self, target_url: &str, build_req: F) -> Result<reqwest::Response>
    where
        F: FnMut(&str) -> Result<reqwest::RequestBuilder>,
    {
        self.send_with_retry_policy(target_url, build_req, true).await
    }

    /// Sends the request built by "build_req" with the credential that belongs to
    /// "target_url" (the already gateway-resolved URL the request goes to).
    async fn send_with_retry_policy<F>(
        &self,
        target_url: &str,
        mut build_req: F,
        retry_statuses: bool,
    ) -> Result<reqwest::Response>
    where
        F: FnMut(&str) -> Result<reqwest::RequestBuilder>,
    {
        let auth = self.auth_for_url(target_url);
        let (mut auth_header, mut oauth_token) = self.resolve_auth_header(auth).await?;
        let mut refreshed_on_401 = false;

        loop {
            let mut attempt = 1usize;
            let result = loop {
                let req = build_req(&auth_header)?;
                let resp = req.send().await.map_err(send_error)?;
                let status = resp.status();

                if status.is_success() {
                    // Every request on this path carries a credential and expects JSON, so
                    // an anonymous 2xx means the credential was not accepted.
                    if let Some(signal) = self.anonymous_signal_for(target_url, &resp) {
                        break Err(Some(self.anonymous_error(target_url, &signal)));
                    }
                    return Ok(resp);
                }
                if status.is_redirection() {
                    return Ok(resp);
                }

                if status == StatusCode::UNAUTHORIZED && oauth_token.is_some() && !refreshed_on_401
                {
                    break Err(None);
                }

                if retry_statuses && is_retryable_status(status) && attempt < MAX_RETRY_ATTEMPTS {
                    let delay = retry_delay(resp.headers().get(RETRY_AFTER), attempt);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    attempt += 1;
                    continue;
                }

                let body = resp.text().await.unwrap_or_default();
                let message = extract_atlassian_error(status, &body);
                break Err(Some(anyhow::Error::from(HttpStatusError {
                    status,
                    message,
                    body,
                })));
            };

            match result {
                Ok(resp) => return Ok(resp),
                Err(Some(err)) => return Err(err),
                Err(None) => {
                    let refreshed = auth
                        .force_refresh_credentials(
                            &self.http,
                            &self.oauth_auth_base,
                            oauth_token.as_deref(),
                        )
                        .await
                        .context("Failed to refresh expired Atlassian OAuth token after HTTP 401")?;
                    auth_header = refreshed.authorization_header();
                    oauth_token = refreshed.oauth_access_token().map(str::to_string);
                    refreshed_on_401 = true;
                }
            }
        }
    }
}

/// Manages per-site [`AtlassianClient`] instances sharing a single `reqwest::Client` connection pool.
#[derive(Clone)]
pub struct SiteClientPool {
    http: reqwest::Client,
    entries: Vec<(SiteProfile, AtlassianClient)>,
}

impl SiteClientPool {
    /// Builds a pool from all effective sites in `config`, sharing one `reqwest::Client`
    /// and deduplicating `AuthManager` instances per `account_id`.
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        Self::from_config_with_options(config, None, None, None)
    }

    /// Builds a pool from `config` with optional credentials fallback path and OAuth/gateway overrides.
    pub fn from_config_with_options(
        config: &AppConfig,
        fallback_path: Option<PathBuf>,
        api_gateway_base: Option<String>,
        oauth_auth_base: Option<String>,
    ) -> Result<Self> {
        // An isolated fallback file (tests, DI) never consults the process environment.
        let env = if fallback_path.is_none() {
            PoolEnv::from_process_env()
        } else {
            PoolEnv::default()
        };
        Self::from_config_with_env(config, fallback_path, api_gateway_base, oauth_auth_base, &env)
    }

    pub(crate) fn from_config_with_env(
        config: &AppConfig,
        fallback_path: Option<PathBuf>,
        api_gateway_base: Option<String>,
        oauth_auth_base: Option<String>,
        env: &PoolEnv,
    ) -> Result<Self> {
        let sites = config.effective_sites();
        let http = build_shared_http_client()?;
        let mut entries = Vec::with_capacity(sites.len());

        let account_key = |site: &SiteProfile| {
            if site.account_id.trim().is_empty() {
                config.profile.clone()
            } else {
                site.account_id.trim().to_string()
            }
        };

        // Accounts are shared between sites, so an account gets the strongest policy of
        // the sites using it.
        let mut policies: HashMap<String, EnvCredentialPolicy> = HashMap::new();
        for site in &sites {
            let policy = env.policy_for_site(site, sites.len());
            policies
                .entry(account_key(site))
                .and_modify(|p| *p = p.strongest(policy))
                .or_insert(policy);
        }

        let site_count = sites.len();
        let mut accounts: HashMap<String, AuthManager> = HashMap::new();
        for site in sites {
            let acct_key = account_key(&site);
            let policy = policies
                .get(&acct_key)
                .copied()
                .unwrap_or(EnvCredentialPolicy::Primary);
            let auth = accounts
                .entry(acct_key.clone())
                .or_insert_with(|| {
                    let mut mgr = AuthManager::for_account(acct_key).with_env_policy(policy);
                    if let Some(ref p) = fallback_path {
                        mgr = mgr.with_fallback_path(p.clone());
                    }
                    mgr
                })
                .clone();

            let mut client = AtlassianClient::for_site_with_http(
                &site,
                auth,
                http.clone(),
                api_gateway_base.clone(),
            )
            .with_workspace_defaults(config);
            if let Some(ref auth_base) = oauth_auth_base {
                client = client.with_oauth_auth_base(auth_base.clone());
            }
            if let Some(conf_auth) =
                confluence_auth_for_site(&site, site_count, env, fallback_path.as_ref())
            {
                client = client.with_confluence_auth(conf_auth);
            }
            entries.push((site, client));
        }

        Ok(Self { http, entries })
    }

    /// Builds a pool with a custom `AuthManager` resolver per site (useful for tests or DI).
    pub fn new(
        sites: Vec<SiteProfile>,
        auth_resolver: impl Fn(&SiteProfile) -> AuthManager,
        api_gateway_base: Option<String>,
        oauth_auth_base: Option<String>,
    ) -> Result<Self> {
        let http = build_shared_http_client()?;
        let mut entries = Vec::with_capacity(sites.len());
        for site in sites {
            let auth = auth_resolver(&site);
            let mut client = AtlassianClient::for_site_with_http(
                &site,
                auth,
                http.clone(),
                api_gateway_base.clone(),
            );
            if let Some(ref auth_base) = oauth_auth_base {
                client = client.with_oauth_auth_base(auth_base.clone());
            }
            entries.push((site, client));
        }
        Ok(Self { http, entries })
    }

    /// Returns a cloned [`AtlassianClient`] for `site_id` (case-insensitive match on site ID).
    pub fn client_for(&self, site_id: &str) -> Result<AtlassianClient> {
        let target = site_id.trim();
        self.entries
            .iter()
            .find(|(s, _)| s.id.eq_ignore_ascii_case(target))
            .map(|(_, c)| c.clone())
            .ok_or_else(|| anyhow!("Unknown site ID: '{target}'"))
    }

    /// Returns the [`SiteProfile`] metadata for `site_id` if present in this pool.
    pub fn site_for(&self, site_id: &str) -> Option<&SiteProfile> {
        let target = site_id.trim();
        self.entries
            .iter()
            .find(|(s, _)| s.id.eq_ignore_ascii_case(target))
            .map(|(s, _)| s)
    }

    /// Returns all registered `(SiteProfile, AtlassianClient)` entries in pool order.
    pub fn all_clients(&self) -> Vec<(SiteProfile, AtlassianClient)> {
        self.entries.clone()
    }

    /// Returns only enabled `(SiteProfile, AtlassianClient)` entries in pool order.
    pub fn enabled_clients(&self) -> Vec<(SiteProfile, AtlassianClient)> {
        self.entries
            .iter()
            .filter(|(s, _)| s.enabled)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.http
    }
}

/// Environment inputs that scope credentials to sites when a pool is built.
#[derive(Debug, Clone, Default)]
pub(crate) struct PoolEnv {
    /// Site URL of the environment credentials ("None" when the environment has none).
    pub credentials_site_url: Option<String>,
    /// ATLASSIAN_CONFLUENCE_PAT / CONFLUENCE_PAT.
    pub confluence_pat: Option<String>,
    /// ATLASSIAN_CONFLUENCE_URL / CONFLUENCE_URL (normalized).
    pub confluence_url: Option<String>,
}

impl PoolEnv {
    pub(crate) fn from_process_env() -> Self {
        Self {
            credentials_site_url: crate::auth::env_credentials_site_url(),
            confluence_pat: crate::auth::env_confluence_pat(),
            confluence_url: crate::auth::env_confluence_url(),
        }
    }

    /// True when the environment values are meant for "site": always for a single-site
    /// configuration, otherwise only for the site whose URL they name.
    fn applies_to_site(&self, site: &SiteProfile, site_count: usize) -> bool {
        if site_count <= 1 {
            return true;
        }
        let site_url = normalize_url_for_match(&site.site_url);
        let matches_site = self
            .credentials_site_url
            .as_deref()
            .map(normalize_url_for_match)
            .is_some_and(|env_url| !env_url.is_empty() && env_url == site_url);
        let conf_root = normalize_url_for_match(&site.effective_confluence_root());
        let matches_confluence = self
            .confluence_url
            .as_deref()
            .map(normalize_url_for_match)
            .is_some_and(|env_url| !env_url.is_empty() && env_url == conf_root);
        matches_site || matches_confluence
    }

    /// Unscoped environment credentials must not override every account: with several
    /// sites they stay primary only for the site whose URL they name.
    pub(crate) fn policy_for_site(&self, site: &SiteProfile, site_count: usize) -> EnvCredentialPolicy {
        if site_count <= 1 {
            return EnvCredentialPolicy::Primary;
        }
        let site_url = normalize_url_for_match(&site.site_url);
        let matches = self
            .credentials_site_url
            .as_deref()
            .map(normalize_url_for_match)
            .is_some_and(|env_url| !env_url.is_empty() && env_url == site_url);
        if matches {
            EnvCredentialPolicy::Primary
        } else {
            EnvCredentialPolicy::FallbackOnly
        }
    }

    /// Id of the site the environment credentials apply to as primary, if any.
    pub(crate) fn credentials_site_id(&self, sites: &[SiteProfile]) -> Option<String> {
        self.credentials_site_url.as_ref()?;
        sites
            .iter()
            .find(|site| self.policy_for_site(site, sites.len()) == EnvCredentialPolicy::Primary)
            .map(|site| site.id.clone())
    }
}

/// Separate Confluence credential for a Data Center site, in priority order:
/// ATLASSIAN_CONFLUENCE_PAT (scoped like other environment credentials), then the
/// site's "confluence_account_id". Cloud sites never get one.
fn confluence_auth_for_site(
    site: &SiteProfile,
    site_count: usize,
    env: &PoolEnv,
    fallback_path: Option<&PathBuf>,
) -> Option<AuthManager> {
    if site.deployment != DeploymentKind::DataCenter {
        return None;
    }
    let conf_root = site.effective_confluence_root();
    if conf_root.is_empty() {
        return None;
    }
    if let Some(token) = env.confluence_pat.as_deref().filter(|t| !t.trim().is_empty()) {
        if env.applies_to_site(site, site_count) {
            return Some(AuthManager::with_credentials(
                format!("{}#confluence", site.account_id.trim()),
                StoredCredentials::BearerPat {
                    site_url: conf_root,
                    token: token.trim().to_string(),
                    deployment: DeploymentKind::DataCenter,
                },
            ));
        }
    }
    let account = site
        .confluence_account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    // A stray ATLASSIAN_PAT (a Jira PAT) must never be sent to Confluence through this account.
    let mut mgr = AuthManager::for_account(account).with_env_policy(EnvCredentialPolicy::Never);
    if let Some(path) = fallback_path {
        mgr = mgr.with_fallback_path(path.clone());
    }
    Some(mgr)
}

fn strip_url_prefix<'a>(url: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = url.strip_prefix(prefix)?;
    if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') {
        Some(rest)
    } else {
        None
    }
}

async fn send_request_with_retry_client<F>(
    _client: &reqwest::Client,
    mut build_req: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> Result<reqwest::RequestBuilder>,
{
    for attempt in 1..=MAX_RETRY_ATTEMPTS {
        let req = build_req()?;
        let resp = req.send().await.map_err(send_error)?;
        let status = resp.status();

        if status.is_success() || status.is_redirection() {
            return Ok(resp);
        }

        if is_retryable_status(status) && attempt < MAX_RETRY_ATTEMPTS {
            let delay = retry_delay(resp.headers().get(RETRY_AFTER), attempt);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            continue;
        }

        let body = resp.text().await.unwrap_or_default();
        let message = extract_atlassian_error(status, &body);
        return Err(HttpStatusError {
            status,
            message,
            body,
        }
        .into());
    }

    unreachable!("retry loop always returns on final attempt")
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_delay(header: Option<&reqwest::header::HeaderValue>, attempt: usize) -> Duration {
    if let Some(val) = header.and_then(|v| v.to_str().ok()) {
        if let Ok(secs) = val.trim().parse::<u64>() {
            return Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS));
        }
    }
    let base_ms = if cfg!(test) { 20 } else { 300 };
    Duration::from_millis(base_ms * attempt as u64)
}

fn is_same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

async fn decode_json_response<T: DeserializeOwned>(
    resp: reqwest::Response,
    url: &str,
) -> Result<T> {
    let text = resp
        .text()
        .await
        .with_context(|| format!("Failed to read response body: {url}"))?;
    serde_json::from_str::<T>(&text).with_context(|| {
        let preview: String = text.chars().take(240).collect();
        format!("Failed to parse Atlassian JSON response ({url}): {preview}")
    })
}

pub(crate) fn extract_atlassian_error(status: StatusCode, body: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(val) = serde_json::from_str::<Value>(body) {
        if let Some(arr) = val.get("errorMessages").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    parts.push(s.to_string());
                }
            }
        }
        if let Some(errors) = val.get("errors") {
            if let Some(obj) = errors.as_object() {
                for (k, v) in obj {
                    if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        parts.push(format!("{k}: {s}"));
                    }
                }
            } else if let Some(arr) = errors.as_array() {
                for item in arr {
                    if let Some(s) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        parts.push(s.to_string());
                    } else {
                        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let detail = item.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                        let combined = match (title.trim(), detail.trim()) {
                            ("", "") => String::new(),
                            (t, "") => t.to_string(),
                            ("", d) => d.to_string(),
                            (t, d) => format!("{t}: {d}"),
                        };
                        if !combined.is_empty() {
                            parts.push(combined);
                        }
                    }
                }
            }
        }
        if let Some(msg) = val
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !parts.iter().any(|p| p == msg) {
                parts.push(msg.to_string());
            }
        }
        if let Some(err) = val
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !parts.iter().any(|p| p == err) {
                parts.push(err.to_string());
            }
        }
    }

    if parts.is_empty() {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            format!("Atlassian API request failed (HTTP {status})")
        } else {
            let preview: String = trimmed.chars().take(300).collect();
            format!("Atlassian API request failed (HTTP {status}): {preview}")
        }
    } else {
        format!(
            "Atlassian API request failed (HTTP {status}): {}",
            parts.join(" | ")
        )
    }
}

pub(crate) fn parse_user_summary(val: &Value, deployment: DeploymentKind) -> UserSummary {
    let id = match deployment {
        DeploymentKind::Cloud => first_nonempty_str(val, &["accountId", "id", "name", "key"]),
        DeploymentKind::DataCenter => {
            first_nonempty_str(val, &["name", "key", "accountId", "id"])
        }
    };
    let display_name = first_nonempty_str(val, &["displayName", "publicName", "name"]);
    let email = val
        .get("emailAddress")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let avatar_url = val.get("avatarUrls").and_then(|urls| {
        ["48x48", "32x32", "24x24", "16x16"]
            .iter()
            .find_map(|k| urls.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string)
    });
    let active = val.get("active").and_then(|v| v.as_bool()).unwrap_or(true);

    UserSummary {
        id,
        display_name,
        email,
        avatar_url,
        active,
    }
}

pub(crate) fn parse_attachment_item(val: &Value, base_url: &str) -> AttachmentItem {
    let id = json_id_to_string(val.get("id"));
    let filename = first_nonempty_str(val, &["filename", "title", "name"]);
    let mime_type = first_nonempty_str(val, &["mimeType", "mediaType"]).if_empty_then(|| {
        val.get("metadata")
            .and_then(|m| m.get("mediaType"))
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string()
    });
    let size = val
        .get("size")
        .or_else(|| val.get("fileSize"))
        .or_else(|| val.get("extensions").and_then(|e| e.get("fileSize")))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let author_name = val
        .get("author")
        .and_then(|a| a.get("displayName"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            val.get("version")
                .and_then(|v| v.get("by"))
                .and_then(|b| b.get("displayName"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string);
    let created_at = first_nonempty_str(val, &["created", "createdAt"]).if_empty_then(|| {
        val.get("version")
            .and_then(|v| v.get("when"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    });
    let raw_content = first_nonempty_str(val, &["content", "downloadLink"]).if_empty_then(|| {
        val.get("_links")
            .and_then(|l| l.get("download"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    });
    let content_url = join_url(base_url, &raw_content);
    let thumbnail_url = val
        .get("thumbnail")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|u| join_url(base_url, u));

    AttachmentItem {
        id,
        filename,
        mime_type,
        size,
        author_name,
        created_at,
        content_url,
        thumbnail_url,
    }
}

pub(crate) fn join_url(base: &str, rel_or_abs: &str) -> String {
    let raw = rel_or_abs.trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return raw.to_string();
    }
    let clean_base = base.trim().trim_end_matches('/');
    if clean_base.is_empty() {
        return raw.to_string();
    }
    if raw.starts_with('/') {
        // Avoid `/wiki/wiki/...` if base already ends with `/wiki` and `_links.webui` starts with `/wiki`.
        if let Ok(parsed) = Url::parse(clean_base) {
            let base_path = parsed.path().trim_end_matches('/');
            if !base_path.is_empty() && raw.starts_with(base_path) {
                let origin = format!(
                    "{}://{}{}",
                    parsed.scheme(),
                    parsed.host_str().unwrap_or(""),
                    parsed
                        .port()
                        .map(|p| format!(":{p}"))
                        .unwrap_or_default()
                );
                return format!("{origin}{raw}");
            }
        }
        format!("{clean_base}{raw}")
    } else {
        format!("{clean_base}/{raw}")
    }
}

pub(crate) fn json_id_to_string(val: Option<&Value>) -> String {
    match val {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn first_nonempty_str(val: &Value, keys: &[&str]) -> String {
    for &k in keys {
        if let Some(s) = val.get(k).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    String::new()
}

/// Paging state for Jira offset APIs (startAt / maxResults, optional isLast / total).
pub(crate) fn offset_page_state(
    val: &Value,
    start: usize,
    got: usize,
    max_results: usize,
) -> (Option<bool>, Option<String>) {
    if got == 0 {
        return (Some(false), None);
    }
    let has_more = if let Some(last) = val.get("isLast").and_then(Value::as_bool) {
        Some(!last)
    } else if let Some(total) = val.get("total").and_then(Value::as_u64) {
        Some(((start + got) as u64) < total)
    } else if got < max_results {
        Some(false)
    } else {
        None
    };
    let next = (has_more != Some(false)).then(|| (start + got).to_string());
    (has_more, next)
}

/// Paging state for Confluence list responses (the _links.next link).
pub(crate) fn next_link_page_state(
    val: &Value,
    cloud: bool,
    start: usize,
    got: usize,
) -> (Option<bool>, Option<String>) {
    let Some(link) = val.pointer("/_links/next").and_then(Value::as_str) else {
        return (Some(false), None);
    };
    let next = if cloud {
        link.split_once('?').and_then(|(_, query)| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("cursor="))
                .and_then(|raw| urlencoding::decode(raw).ok())
                .map(|c| c.into_owned())
        })
    } else {
        Some((start + got).to_string())
    };
    (Some(true), next)
}

trait StringExt {
    fn if_empty_then<F: FnOnce() -> String>(self, f: F) -> String;
}

impl StringExt for String {
    fn if_empty_then<F: FnOnce() -> String>(self, f: F) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{now_epoch_secs, StoredCredentials};
    use axum::{
        http::{HeaderMap, StatusCode as AxumStatus},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn response_headers(pairs: &[(&'static str, &'static str)]) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, reqwest::header::HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn detect_anonymous_header_x_ausername() {
        assert_eq!(
            detect_anonymous_response(&response_headers(&[("x-ausername", "anonymous")]), None),
            Some(AnonymousSignal::HeaderAnonymous)
        );
        // Header names and the value are case-insensitive.
        assert_eq!(
            detect_anonymous_response(&response_headers(&[("X-AUSERNAME", " Anonymous ")]), None),
            Some(AnonymousSignal::HeaderAnonymous)
        );
        assert_eq!(
            detect_anonymous_response(&response_headers(&[("x-ausername", "jdoe")]), None),
            None
        );
        assert_eq!(detect_anonymous_response(&response_headers(&[]), None), None);
    }

    #[test]
    fn detect_anonymous_seraph_login_reason_failed() {
        for reason in ["AUTHENTICATED_FAILED", "AUTHENTICATION_DENIED", "AUTHORISATION_FAILED"] {
            assert_eq!(
                detect_anonymous_response(
                    &response_headers(&[("x-seraph-loginreason", reason)]),
                    None
                ),
                Some(AnonymousSignal::SeraphLoginFailed(reason.to_string())),
                "{reason}"
            );
        }
        // Jira sends "OUT, AUTHENTICATED_FAILED" when a stale session was dropped first.
        assert!(matches!(
            detect_anonymous_response(
                &response_headers(&[("x-seraph-loginreason", "OUT, AUTHENTICATED_FAILED")]),
                None
            ),
            Some(AnonymousSignal::SeraphLoginFailed(_))
        ));
    }

    #[test]
    fn detect_anonymous_ignores_seraph_ok() {
        for reason in ["OK", "OUT", "OUT, OK"] {
            assert_eq!(
                detect_anonymous_response(
                    &response_headers(&[("x-seraph-loginreason", reason), ("x-ausername", "jdoe")]),
                    Some("application/json;charset=UTF-8")
                ),
                None,
                "{reason}"
            );
        }
    }

    #[test]
    fn detect_html_login_page_on_json_request() {
        assert_eq!(
            detect_anonymous_response(&response_headers(&[]), Some("text/html;charset=UTF-8")),
            Some(AnonymousSignal::HtmlLoginPage)
        );
        assert_eq!(
            detect_anonymous_response(&response_headers(&[]), Some("application/json")),
            None
        );
        // Only JSON REST paths expect JSON; page and download URLs may legitimately be HTML.
        assert!(is_json_rest_path("https://wiki.corp/rest/api/space?limit=1"));
        assert!(is_json_rest_path("https://acme.atlassian.net/wiki/api/v2/pages/1"));
        assert!(!is_json_rest_path("https://wiki.corp/download/attachments/1/a.html"));
    }

    #[tokio::test]
    async fn get_myself_rejects_empty_identity() {
        let app = Router::new().route(
            "/rest/api/2/myself",
            get(|| async { Json(serde_json::json!({})) }),
        );
        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "dc",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "pat-rejected".into(),
                deployment: DeploymentKind::DataCenter,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth).unwrap();
        let err = client.get_myself().await.expect_err("an empty identity is not a user");
        assert_eq!(http_status_code(&err), Some(StatusCode::UNAUTHORIZED));
        assert!(is_anonymous_error(&err));
        let text = format!("{err:#}");
        assert!(text.contains("anonymous"), "{text}");
        assert!(!text.contains("pat-rejected"), "the credential must never be printed: {text}");
        assert!(client.cached_me().is_none(), "an empty identity must not be cached");
    }

    #[tokio::test]
    async fn html_login_page_answer_to_json_request_is_unauthorized() {
        let app = Router::new().route(
            "/rest/api/2/project",
            get(|| async {
                (
                    [("content-type", "text/html;charset=UTF-8")],
                    "<html><body>Single sign-on</body></html>",
                )
            }),
        );
        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "dc",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "pat".into(),
                deployment: DeploymentKind::DataCenter,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth).unwrap();
        let err = client
            .get_json::<Value>(&format!("{base_url}/rest/api/2/project"))
            .await
            .expect_err("an HTML page is not a JSON answer");
        assert_eq!(http_status_code(&err), Some(StatusCode::UNAUTHORIZED));
        assert!(format!("{err:#}").contains("HTML login page"), "{err:#}");
    }

    #[test]
    fn identity_probe_login_user_by_persona() {
        use crate::config::Persona;
        let user = |id: &str| UserSummary {
            id: id.into(),
            display_name: id.into(),
            email: None,
            avatar_url: None,
            active: true,
        };
        let fail = |status: u16, anonymous: bool| ProbeFailure {
            status: Some(status),
            anonymous,
            message: format!("HTTP {status}"),
        };
        let both = IdentityProbe {
            jira: Some(Ok(user("jira-user"))),
            confluence: Some(Ok(user("wiki-user"))),
        };
        assert_eq!(both.login_user(Persona::Jira).unwrap().id, "jira-user");
        assert_eq!(both.login_user(Persona::Confluence).unwrap().id, "wiki-user");
        assert_eq!(both.status_labels(), ("ok", "ok"));

        let confluence_only = IdentityProbe {
            jira: Some(Err(fail(404, false))),
            confluence: Some(Ok(user("wiki-user"))),
        };
        assert_eq!(confluence_only.login_user(Persona::Jira).unwrap().id, "wiki-user");
        assert_eq!(confluence_only.status_labels(), ("not_found", "ok"));

        let rejected = IdentityProbe {
            jira: Some(Err(fail(401, false))),
            confluence: Some(Err(fail(401, true))),
        };
        let err = rejected.login_user(Persona::Jira).unwrap_err();
        assert_eq!(http_status_code(&err), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(rejected.status_labels(), ("unauthorized", "anonymous"));
        assert!(is_anonymous_error(&rejected.login_user(Persona::Confluence).unwrap_err()));
    }

    #[tokio::test]
    async fn test_get_myself_cloud_and_datacenter_caching() {
        let app = Router::new()
            .route(
                "/rest/api/3/myself",
                get(|headers: HeaderMap| async move {
                    assert!(headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with("Basic "));
                    Json(serde_json::json!({
                        "accountId": "acc-123",
                        "displayName": "Chulsoo Kim",
                        "emailAddress": "chulsoo@acme.com",
                        "avatarUrls": { "48x48": "https://cdn.example.com/48.png" },
                        "active": true
                    }))
                }),
            )
            .route(
                "/rest/api/2/myself",
                get(|headers: HeaderMap| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer pat-dc-1"
                    );
                    Json(serde_json::json!({
                        "name": "jdoe",
                        "key": "JIRAUSER100",
                        "displayName": "John Doe",
                        "emailAddress": "jdoe@corp.local",
                        "active": true
                    }))
                }),
            );
        let base_url = spawn_router(app).await;

        let cloud_auth = AuthManager::with_credentials(
            "cloud",
            StoredCredentials::ApiToken {
                site_url: base_url.clone(),
                email: "chulsoo@acme.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let cloud_client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth)
                .unwrap();
        assert!(cloud_client.cached_me().is_none());
        let me = cloud_client.get_myself().await.unwrap();
        assert_eq!(me.id, "acc-123");
        assert_eq!(me.display_name, "Chulsoo Kim");
        assert_eq!(
            me.avatar_url.as_deref(),
            Some("https://cdn.example.com/48.png")
        );
        assert_eq!(cloud_client.cached_me(), Some(me));

        let dc_auth = AuthManager::with_credentials(
            "dc",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "pat-dc-1".into(),
                deployment: DeploymentKind::DataCenter,
            },
        );
        let dc_client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, dc_auth)
                .unwrap();
        let dc_me = dc_client.get_myself().await.unwrap();
        assert_eq!(dc_me.id, "jdoe");
        assert_eq!(dc_me.display_name, "John Doe");
    }

    #[tokio::test]
    async fn test_retry_on_429_with_retry_after_and_error_extraction() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/myself",
                get(move || {
                    let attempts = attempts_clone.clone();
                    async move {
                        let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if n < 3 {
                            (
                                AxumStatus::TOO_MANY_REQUESTS,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({
                                    "errorMessages": ["Rate limit exceeded"]
                                })),
                            )
                        } else {
                            (
                                AxumStatus::OK,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({
                                    "accountId": "ok-after-retry",
                                    "displayName": "Retry User",
                                    "active": true
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/rest/api/3/fail",
                get(|| async {
                    (
                        AxumStatus::BAD_REQUEST,
                        Json(serde_json::json!({
                            "errorMessages": ["Invalid project key."],
                            "errors": { "summary": "Summary is required." }
                        })),
                    )
                }),
            );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let me = client.get_myself().await.unwrap();
        assert_eq!(me.id, "ok-after-retry");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        let err = client
            .get_json::<Value>(&format!("{base_url}/rest/api/3/fail"))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("400"));
        assert!(msg.contains("Invalid project key."));
        assert!(msg.contains("summary: Summary is required."));
    }

    #[tokio::test]
    async fn test_oauth_gateway_routing_preserves_human_web_urls() {
        let app = Router::new()
            .route(
                "/ex/jira/cloud-abc/rest/api/3/myself",
                get(|headers: HeaderMap| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer oauth-tok-1"
                    );
                    Json(serde_json::json!({
                        "accountId": "oauth-acc-1",
                        "displayName": "OAuth Alice",
                        "active": true
                    }))
                }),
            )
            .route(
                "/ex/jira/cloud-abc/rest/api/3/search/jql",
                post(|| async {
                    Json(serde_json::json!({
                        "issues": [{
                            "id": "1001",
                            "key": "ENG-42",
                            "fields": {
                                "summary": "OAuth Gateway Issue",
                                "status": { "id": "1", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                                "issuetype": { "id": "1", "name": "Task", "subtask": false }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/ex/confluence/cloud-abc/wiki/api/v2/spaces",
                get(|| async {
                    Json(serde_json::json!({
                        "results": [{
                            "id": "sp-1",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "_links": { "webui": "/spaces/ENG" }
                        }]
                    }))
                }),
            );
        let gateway_base = spawn_router(app).await;

        let site = SiteProfile {
            id: "acme".into(),
            name: "Acme Cloud".into(),
            site_url: "https://acme.atlassian.net".into(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: Some("cloud-abc".into()),
            account_id: "oauth-main".into(),
            enabled: true,
            scopes: vec!["read:jira-work".into()],
            confluence_account_id: None,
        };
        let auth = AuthManager::with_credentials(
            "oauth-main",
            StoredCredentials::CloudOAuth {
                email: Some("alice@acme.com".into()),
                access_token: "oauth-tok-1".into(),
                refresh_token: Some("ref-1".into()),
                expires_at_epoch: now_epoch_secs() + 3600,
                client_id: "cid".into(),
                client_secret: Some("csec".into()),
                scopes: vec!["offline_access".into()],
            },
        );

        let client = AtlassianClient::for_site(&site, auth, Some(gateway_base.clone())).unwrap();
        assert_eq!(client.site_url(), "https://acme.atlassian.net");
        assert_eq!(
            client.jira_api_base_url(),
            format!("{gateway_base}/ex/jira/cloud-abc")
        );
        assert_eq!(
            client.confluence_api_base_url(),
            format!("{gateway_base}/ex/confluence/cloud-abc/wiki")
        );

        let me = client.get_myself().await.unwrap();
        assert_eq!(me.id, "oauth-acc-1");

        // Verify Jira search hits gateway while preserving human-facing web_url
        let page = client
            .search_issues_jql("project = ENG", 10, None, None)
            .await
            .unwrap();
        assert_eq!(page.issues.len(), 1);
        assert_eq!(page.issues[0].web_url, "https://acme.atlassian.net/browse/ENG-42");

        // Verify Confluence spaces hit gateway while preserving human-facing web_url
        let spaces = client.list_spaces(10).await.unwrap();
        assert_eq!(spaces.len(), 1);
        assert_eq!(
            spaces[0].web_url,
            "https://acme.atlassian.net/wiki/spaces/ENG"
        );
    }

    #[tokio::test]
    async fn test_site_client_pool_401_single_flight_auto_refresh() {
        let refresh_count = Arc::new(AtomicUsize::new(0));
        let refresh_clone = refresh_count.clone();

        let app = Router::new()
            .route(
                "/oauth/token",
                post(move |Json(body): Json<Value>| {
                    let count = refresh_clone.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        assert_eq!(body["grant_type"], "refresh_token");
                        assert_eq!(body["refresh_token"], "ref-stale");
                        Json(serde_json::json!({
                            "access_token": "tok-fresh-2",
                            "refresh_token": "ref-rotated-2",
                            "expires_in": 3600
                        }))
                    }
                }),
            )
            .route(
                "/ex/jira/cloud-1/rest/api/3/myself",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer tok-fresh-2" {
                        (
                            AxumStatus::OK,
                            Json(serde_json::json!({
                                "accountId": "u-site1",
                                "displayName": "Site 1 User",
                                "active": true
                            })),
                        )
                    } else {
                        (
                            AxumStatus::UNAUTHORIZED,
                            Json(serde_json::json!({ "message": "Token expired" })),
                        )
                    }
                }),
            )
            .route(
                "/ex/jira/cloud-2/rest/api/3/myself",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer tok-fresh-2" {
                        (
                            AxumStatus::OK,
                            Json(serde_json::json!({
                                "accountId": "u-site2",
                                "displayName": "Site 2 User",
                                "active": true
                            })),
                        )
                    } else {
                        (
                            AxumStatus::UNAUTHORIZED,
                            Json(serde_json::json!({ "message": "Token expired" })),
                        )
                    }
                }),
            );

        let mock_base = spawn_router(app).await;

        let shared_oauth = AuthManager::with_credentials(
            "oauth-main",
            StoredCredentials::CloudOAuth {
                email: Some("dev@acme.com".into()),
                access_token: "tok-stale-1".into(),
                refresh_token: Some("ref-stale".into()),
                expires_at_epoch: now_epoch_secs() + 3600, // not locally expired yet -> triggers 401 path!
                client_id: "cid".into(),
                client_secret: Some("csec".into()),
                scopes: vec!["offline_access".into()],
            },
        );

        let sites = vec![
            SiteProfile {
                id: "site-1".into(),
                name: "Site One".into(),
                site_url: "https://one.atlassian.net".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: Some("cloud-1".into()),
                account_id: "oauth-main".into(),
                enabled: true,
                scopes: Vec::new(),
                confluence_account_id: None,
            },
            SiteProfile {
                id: "site-2".into(),
                name: "Site Two".into(),
                site_url: "https://two.atlassian.net".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: Some("cloud-2".into()),
                account_id: "oauth-main".into(),
                enabled: true,
                scopes: Vec::new(),
                confluence_account_id: None,
            },
            SiteProfile {
                id: "disabled-dc".into(),
                name: "Disabled DC".into(),
                site_url: "https://dc.local".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::DataCenter,
                cloud_id: None,
                account_id: "dc".into(),
                enabled: false,
                scopes: Vec::new(),
                confluence_account_id: None,
            },
        ];

        let pool = SiteClientPool::new(
            sites,
            |_site| shared_oauth.clone(),
            Some(mock_base.clone()),
            Some(mock_base.clone()),
        )
        .unwrap();

        assert_eq!(pool.all_clients().len(), 3);
        assert_eq!(pool.enabled_clients().len(), 2);
        assert!(pool.client_for("nonexistent").is_err());

        let c1 = pool.client_for("site-1").unwrap();
        let c2 = pool.client_for("site-2").unwrap();

        // Fire concurrent requests on two sites sharing the same OAuth account -> both get 401,
        // single-flight refresh executes ONLY ONCE, and both retry and succeed!
        let (r1, r2) = tokio::join!(c1.get_myself(), c2.get_myself());
        assert_eq!(r1.unwrap().id, "u-site1");
        assert_eq!(r2.unwrap().id, "u-site2");
        assert_eq!(refresh_count.load(Ordering::SeqCst), 1);

        // Verify rotated refresh token was saved back to AuthManager
        match shared_oauth.load_credentials().unwrap().unwrap() {
            StoredCredentials::CloudOAuth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "tok-fresh-2");
                assert_eq!(refresh_token.as_deref(), Some("ref-rotated-2"));
            }
            _ => panic!("expected CloudOAuth"),
        }
    }

    #[test]
    fn offset_and_next_link_page_state() {
        assert_eq!(
            offset_page_state(&serde_json::json!({ "isLast": true }), 0, 50, 50),
            (Some(false), None)
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({ "isLast": false }), 0, 50, 50),
            (Some(true), Some("50".to_string()))
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({ "total": 120 }), 0, 100, 100),
            (Some(true), Some("100".to_string()))
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({ "total": 120 }), 100, 20, 100),
            (Some(false), None)
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({}), 0, 50, 50),
            (None, Some("50".to_string()))
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({}), 0, 12, 50),
            (Some(false), None)
        );
        assert_eq!(
            offset_page_state(&serde_json::json!({ "isLast": false }), 0, 0, 50),
            (Some(false), None)
        );

        assert_eq!(
            next_link_page_state(
                &serde_json::json!({ "_links": { "next": "/wiki/api/v2/pages/100/children?limit=2&cursor=abc%3D" } }),
                true,
                0,
                2
            ),
            (Some(true), Some("abc=".to_string()))
        );
        assert_eq!(
            next_link_page_state(
                &serde_json::json!({ "_links": { "next": "/rest/api/content/100/child/page?limit=2&start=2" } }),
                false,
                0,
                2
            ),
            (Some(true), Some("2".to_string()))
        );
        assert_eq!(
            next_link_page_state(&serde_json::json!({ "_links": {} }), true, 0, 2),
            (Some(false), None)
        );
    }

    /// Two self-signed test roots (EC P-256, generated offline; not trusted by anything).
    const TEST_ROOT_CA_BUNDLE: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBnzCCAUWgAwIBAgIUBqsPDduJ62TWB9cnMPdS94mretIwCgYIKoZIzj0EAwIw\n\
JDEiMCAGA1UEAwwZYXRsYXNzaWFuLWNsaSB0ZXN0IHJvb3QgYTAgFw0yNjExMjMw\n\
NzMyNDBaGA8yMTI2MTAzMDA3MzI0MFowJDEiMCAGA1UEAwwZYXRsYXNzaWFuLWNs\n\
aSB0ZXN0IHJvb3QgYTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABIgTZhTPsP4X\n\
Q1bsDndVMfOReC3bvMA82Uzpi+jqYBRMueTdLY9ev6oXH/50ZCO3TGe/l250Ednv\n\
7U/BDm9gY6CjUzBRMB0GA1UdDgQWBBSY6VjApxUeMeHqzt10Zm9tHMLJXTAfBgNV\n\
HSMEGDAWgBSY6VjApxUeMeHqzt10Zm9tHMLJXTAPBgNVHRMBAf8EBTADAQH/MAoG\n\
CCqGSM49BAMCA0gAMEUCIQCxU9Ru5mBL34Bi/OEf8qK69xsBWxmI0fg8LF5GMoei\n\
rAIgNhumCc0M8Hq6m+g6DZb0H4HpmfSjASfc2BPgHUEwTI0=\n\
-----END CERTIFICATE-----\n\
-----BEGIN CERTIFICATE-----\n\
MIIBnjCCAUWgAwIBAgIUI9q+MMnklGTAJLkxMrVD6DlGyHgwCgYIKoZIzj0EAwIw\n\
JDEiMCAGA1UEAwwZYXRsYXNzaWFuLWNsaSB0ZXN0IHJvb3QgYjAgFw0yNjExMjMw\n\
NzMyNDBaGA8yMTI2MTAzMDA3MzI0MFowJDEiMCAGA1UEAwwZYXRsYXNzaWFuLWNs\n\
aSB0ZXN0IHJvb3QgYjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABJYjnZHEM4Vt\n\
68nfh26wEc4nLoIuRWfE996OkuCYhloaG2bye2L9G7kRWi60j13B5+r5Cv+qs5vz\n\
k4IA3lq5LG+jUzBRMB0GA1UdDgQWBBT01YUGSYigUsUINk1+KaaVhhVnTTAfBgNV\n\
HSMEGDAWgBT01YUGSYigUsUINk1+KaaVhhVnTTAPBgNVHRMBAf8EBTADAQH/MAoG\n\
CCqGSM49BAMCA0cAMEQCIEhCliJhSOr4D5Sy/jvBAmbUxk73UbD+oqhcxg6hMuP0\n\
AiA+cNCCx29pol4ocp9J/G0R/45YiLiv6VdSopzRWi3hcw==\n\
-----END CERTIFICATE-----\n\
";

    fn env_lookup<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn extra_ca_reads_pem_bundle_from_env_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corp-roots.pem");
        std::fs::write(&path, TEST_ROOT_CA_BUNDLE).unwrap();
        let path = path.display().to_string();

        let pairs = [("ATLASSIAN_EXTRA_CA_PEM", path.clone())];
        let (certs, warnings) = extra_ca_certificates_with(env_lookup(&pairs));
        assert_eq!(certs.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");

        // Every variable that is set is loaded, not only the first one.
        let pairs = [
            ("ATLASSIAN_EXTRA_CA_PEM", path.clone()),
            ("SSL_CERT_FILE", path.clone()),
            ("NODE_EXTRA_CA_CERTS", path.clone()),
        ];
        let (certs, sources) = load_extra_ca_sources(env_lookup(&pairs));
        assert_eq!(certs.len(), 6);
        assert_eq!(
            sources.iter().map(|s| s.env).collect::<Vec<_>>(),
            vec!["ATLASSIAN_EXTRA_CA_PEM", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"]
        );
        assert!(sources.iter().all(|s| s.certificates == 2 && s.error.is_none() && s.path == path));

        // The loaded roots are accepted by the TLS backend.
        let mut builder = base_http_client_builder();
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
        builder.build().expect("client with extra roots");
    }

    #[test]
    fn extra_ca_accepts_inline_pem() {
        let pairs = [("ATLASSIAN_EXTRA_CA_PEM", TEST_ROOT_CA_BUNDLE.to_string())];
        let (certs, sources) = load_extra_ca_sources(env_lookup(&pairs));
        assert_eq!(certs.len(), 2);
        assert_eq!(sources[0].path, "<inline PEM>");
        assert!(sources[0].error.is_none());

        // Inline text is only accepted in ATLASSIAN_EXTRA_CA_PEM; the others are file paths.
        let pairs = [("SSL_CERT_FILE", TEST_ROOT_CA_BUNDLE.to_string())];
        let (certs, warnings) = extra_ca_certificates_with(env_lookup(&pairs));
        assert!(certs.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn extra_ca_missing_file_warns_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.pem").display().to_string();
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "this is not a certificate").unwrap();

        let pairs = [("ATLASSIAN_EXTRA_CA_PEM", missing.clone())];
        let (certs, warnings) = extra_ca_certificates_with(env_lookup(&pairs));
        assert!(certs.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ATLASSIAN_EXTRA_CA_PEM"), "{warnings:?}");
        assert!(warnings[0].contains(&missing), "{warnings:?}");

        let pairs = [("NODE_EXTRA_CA_CERTS", garbage.display().to_string())];
        let (certs, warnings) = extra_ca_certificates_with(env_lookup(&pairs));
        assert!(certs.is_empty());
        assert_eq!(warnings.len(), 1);

        // Unset or blank variables are not sources at all.
        let pairs = [("SSL_CERT_FILE", "   ".to_string())];
        let (certs, sources) = load_extra_ca_sources(env_lookup(&pairs));
        assert!(certs.is_empty() && sources.is_empty());

        // A bad extra bundle never prevents building a client.
        build_http_client(|builder| builder).expect("client still builds");
    }

    #[test]
    fn tls_recovery_hint_matches_unknown_issuer_chain() {
        let err = anyhow!("invalid peer certificate: UnknownIssuer")
            .context("client error (Connect)")
            .context("error sending request for url (https://jira.corp.example/rest/api/2/myself)");
        let chain = format!("{err:#}");
        let hint = tls_recovery_hint(&chain, "jira.corp.example").expect("hint");
        assert!(hint.contains("jira.corp.example"));
        assert!(hint.contains("UnknownIssuer"));
        assert!(hint.contains("ATLASSIAN_EXTRA_CA_PEM"));
        assert!(hint.contains("SSL_CERT_FILE"));
        assert!(hint.contains("NODE_EXTRA_CA_CERTS"));
        assert!(hint.contains("atlassian doctor --probe"));

        assert!(tls_recovery_hint("self-signed certificate in certificate chain", "h").is_some());
        assert!(tls_recovery_hint("connection refused", "h").is_none());
        assert!(tls_recovery_hint("operation timed out", "h").is_none());
    }

    fn dc_site(id: &str, site_url: &str, confluence_url: &str) -> SiteProfile {
        SiteProfile {
            id: id.into(),
            name: id.into(),
            site_url: site_url.into(),
            confluence_url: confluence_url.into(),
            deployment: DeploymentKind::DataCenter,
            account_id: id.into(),
            ..SiteProfile::default()
        }
    }

    fn pat(token: &str, site_url: &str) -> StoredCredentials {
        StoredCredentials::BearerPat {
            site_url: site_url.into(),
            token: token.into(),
            deployment: DeploymentKind::DataCenter,
        }
    }

    #[test]
    fn pool_clients_inherit_workspace_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig {
            default_space_key: Some("DOCS".into()),
            default_board_id: Some(42),
            default_project: Some("ENG".into()),
            sync_interval_secs: 90,
            default_site: Some("corp".into()),
            sites: vec![dc_site("corp", "https://jira.corp.example/", "")],
            ..AppConfig::default()
        };

        let pool = SiteClientPool::from_config_with_options(
            &cfg,
            Some(dir.path().join("credentials.json")),
            None,
            None,
        )
        .unwrap();
        let client = pool.client_for("corp").unwrap();
        assert_eq!(client.config().default_space_key.as_deref(), Some("DOCS"));
        assert_eq!(client.config().default_board_id, Some(42));
        assert_eq!(client.config().default_project.as_deref(), Some("ENG"));
        assert_eq!(client.config().sync_interval_secs, 90);
        // Site identity is still the site's own, and site lists are not copied in.
        assert_eq!(client.site_url(), "https://jira.corp.example");
        assert_eq!(client.deployment(), DeploymentKind::DataCenter);
        assert!(client.config().sites.is_empty());
        assert!(client.config().default_site.is_none());
    }

    #[test]
    fn auth_for_url_routes_confluence_root_to_confluence_auth() {
        let site = dc_site("corp", "https://jira.corp.example", "https://wiki.corp.example/confluence");
        let jira_auth = AuthManager::with_credentials("corp", pat("PAT-J", "https://jira.corp.example"));
        let conf_auth = AuthManager::with_credentials(
            "corp-confluence",
            pat("PAT-C", "https://wiki.corp.example/confluence"),
        );
        let plain = AtlassianClient::for_site(&site, jira_auth.clone(), None).unwrap();
        // Without a separate credential everything uses the site credential.
        assert_eq!(
            plain
                .auth_for_url("https://wiki.corp.example/confluence/rest/api/space")
                .account_id(),
            "corp"
        );
        assert_eq!(plain.confluence_auth().account_id(), "corp");
        assert_eq!(plain.confluence_credential_account(), None);

        let split = plain.clone().with_confluence_auth(conf_auth);
        assert_eq!(split.confluence_credential_account(), Some("corp-confluence"));
        for url in [
            "https://wiki.corp.example/confluence",
            "https://wiki.corp.example/confluence/rest/api/space?limit=250",
            "https://wiki.corp.example/confluence?x=1",
        ] {
            assert_eq!(split.auth_for_url(url).account_id(), "corp-confluence", "{url}");
        }
        for url in [
            "https://jira.corp.example/rest/api/2/myself",
            "https://jira.corp.example/rest/agile/1.0/board",
            "https://cdn.example.net/confluence/file.png",
        ] {
            assert_eq!(split.auth_for_url(url).account_id(), "corp", "{url}");
        }
        assert_eq!(
            split.auth_for_url("https://jira.corp.example/rest/api/2/myself").authorization_header().unwrap(),
            "Bearer PAT-J"
        );
        assert_eq!(
            split
                .auth_for_url("https://wiki.corp.example/confluence/rest/api/space")
                .authorization_header()
                .unwrap(),
            "Bearer PAT-C"
        );
    }

    #[test]
    fn auth_for_url_requires_segment_boundary() {
        let site = dc_site("corp", "https://corp.example/jira", "https://wiki.corp/confluence");
        let client = AtlassianClient::for_site(
            &site,
            AuthManager::with_credentials("corp", pat("PAT-J", "https://corp.example/jira")),
            None,
        )
        .unwrap()
        .with_confluence_auth(AuthManager::with_credentials(
            "corp-confluence",
            pat("PAT-C", "https://wiki.corp/confluence"),
        ));
        assert_eq!(
            client.auth_for_url("https://wiki.corp/confluence2/rest/api/space").account_id(),
            "corp"
        );
        assert_eq!(
            client.auth_for_url("https://wiki.corp/confluence.evil/rest/api/space").account_id(),
            "corp"
        );
        assert_eq!(
            client.auth_for_url("https://wiki.corp/confluence/rest/api/space").account_id(),
            "corp-confluence"
        );

        // A Confluence root identical to the Jira root cannot be separated by URL: the
        // Confluence credential is then never substituted for the site credential.
        let same_root = dc_site("one", "https://corp.example", "https://corp.example");
        let client = AtlassianClient::for_site(
            &same_root,
            AuthManager::with_credentials("one", pat("PAT-J", "https://corp.example")),
            None,
        )
        .unwrap()
        .with_confluence_auth(AuthManager::with_credentials(
            "one-confluence",
            pat("PAT-C", "https://corp.example"),
        ));
        assert_eq!(
            client.auth_for_url("https://corp.example/rest/api/space").account_id(),
            "one"
        );
    }

    #[test]
    fn multi_site_env_pat_applies_only_to_matching_site_url() {
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");
        let env = PoolEnv {
            credentials_site_url: Some("https://jira.b.example/".into()),
            confluence_pat: Some("PAT-C".into()),
            confluence_url: None,
        };

        let cfg = AppConfig {
            sites: vec![
                dc_site("a", "https://jira.a.example", "https://wiki.a.example"),
                dc_site("b", "https://jira.b.example", "https://wiki.b.example"),
            ],
            ..AppConfig::default()
        };
        let pool =
            SiteClientPool::from_config_with_env(&cfg, Some(cred_path.clone()), None, None, &env)
                .unwrap();
        let a = pool.client_for("a").unwrap();
        let b = pool.client_for("b").unwrap();
        assert_eq!(a.auth().env_policy(), EnvCredentialPolicy::FallbackOnly);
        assert_eq!(b.auth().env_policy(), EnvCredentialPolicy::Primary);
        assert_eq!(env.credentials_site_id(&cfg.effective_sites()).as_deref(), Some("b"));
        // The Confluence PAT follows the same scoping and is only ever built for site b.
        assert_eq!(a.confluence_credential_account(), None);
        assert_eq!(b.confluence_credential_account(), Some("b#confluence"));
        assert_eq!(
            b.auth_for_url("https://wiki.b.example/rest/api/space")
                .authorization_header()
                .unwrap(),
            "Bearer PAT-C"
        );

        // A single-site configuration keeps today's behaviour: environment first.
        let single = AppConfig {
            sites: vec![dc_site("a", "https://jira.a.example", "https://wiki.a.example")],
            ..AppConfig::default()
        };
        let pool =
            SiteClientPool::from_config_with_env(&single, Some(cred_path.clone()), None, None, &env)
                .unwrap();
        let only = pool.client_for("a").unwrap();
        assert_eq!(only.auth().env_policy(), EnvCredentialPolicy::Primary);
        assert_eq!(only.confluence_credential_account(), Some("a#confluence"));

        // An account shared by both sites gets the strongest policy of its sites.
        let mut shared = cfg.clone();
        for site in &mut shared.sites {
            site.account_id = "shared".into();
        }
        let pool =
            SiteClientPool::from_config_with_env(&shared, Some(cred_path.clone()), None, None, &env)
                .unwrap();
        assert_eq!(
            pool.client_for("a").unwrap().auth().env_policy(),
            EnvCredentialPolicy::Primary
        );

        // No environment credentials: nothing is primary, and Cloud sites never get a
        // separate Confluence credential.
        let mut mixed = cfg.clone();
        mixed.sites[0].deployment = DeploymentKind::Cloud;
        mixed.sites[0].confluence_account_id = Some("ignored-on-cloud".into());
        mixed.sites[1].confluence_account_id = Some("b-wiki".into());
        let pool = SiteClientPool::from_config_with_env(
            &mixed,
            Some(cred_path),
            None,
            None,
            &PoolEnv::default(),
        )
        .unwrap();
        assert_eq!(PoolEnv::default().credentials_site_id(&mixed.effective_sites()), None);
        assert_eq!(pool.client_for("a").unwrap().confluence_credential_account(), None);
        let b = pool.client_for("b").unwrap();
        assert_eq!(b.confluence_credential_account(), Some("b-wiki"));
        // The stored Confluence account can never pick up a stray ATLASSIAN_PAT.
        assert_eq!(b.confluence_auth().env_policy(), EnvCredentialPolicy::Never);
    }

    #[tokio::test]
    async fn probe_identity_queries_jira_and_confluence_concurrently() {
        use axum::{routing::get, Json, Router};
        use tokio::{net::TcpListener, sync::Barrier};

        let barrier = Arc::new(Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let app = Router::new()
            .route(
                "/rest/api/3/myself",
                get(move || {
                    let b = b1.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "accountId": "acc-1", "displayName": "Alice" }))
                    }
                }),
            )
            .route(
                "/wiki/rest/api/user/current",
                get(move || {
                    let b = b2.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "accountId": "acc-1", "displayName": "Alice" }))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let auth = AuthManager::with_credentials(
            "probe-par",
            StoredCredentials::ApiToken {
                site_url: base_url.clone(),
                email: "alice@example.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::with_base_urls(
            &base_url,
            format!("{base_url}/wiki"),
            DeploymentKind::Cloud,
            auth,
        )
        .unwrap();

        let probe = tokio::time::timeout(Duration::from_secs(3), client.probe_identity())
            .await
            .expect("probe_identity must overlap Jira and Confluence requests");
        assert_eq!(probe.jira.unwrap().unwrap().id, "acc-1");
        assert_eq!(probe.confluence.unwrap().unwrap().id, "acc-1");
        srv.abort();
    }

    #[tokio::test]
    async fn get_myself_cached_reuses_verified_identity_across_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use axum::{routing::get, Json, Router};
        use tokio::net::TcpListener;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/rest/api/3/myself",
            get(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "accountId": "acc-cached", "displayName": "Cached User" }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let auth = AuthManager::with_credentials(
            "me-cache",
            StoredCredentials::ApiToken {
                site_url: base_url.clone(),
                email: "alice@example.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();
        let cloned = client.clone();

        let first = client.get_myself_cached().await.unwrap();
        let second = cloned.get_myself_cached().await.unwrap();
        assert_eq!(first.id, "acc-cached");
        assert_eq!(second.id, "acc-cached");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        srv.abort();
    }
}
