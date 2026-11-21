//! Core HTTP client with Cloud/DataCenter routing, OAuth 2.0 (3LO) gateway routing,
//! `401` single-flight OAuth refresh, `429 Retry-After` backoff, and `SiteClientPool`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    header::{ACCEPT, AUTHORIZATION, LOCATION, RETRY_AFTER},
    multipart::{Form, Part},
    Method, StatusCode,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::{
    auth::{AuthManager, StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE},
    config::{normalize_base_url, AppConfig, DeploymentKind, SiteProfile},
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

pub(crate) fn http_status_code(err: &anyhow::Error) -> Option<StatusCode> {
    err.downcast_ref::<HttpStatusError>().map(|e| e.status)
}

pub(crate) fn is_not_found_error(err: &anyhow::Error) -> bool {
    http_status_code(err) == Some(StatusCode::NOT_FOUND)
}

fn build_shared_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("atlassian-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to initialize HTTP client")
}

#[derive(Clone)]
pub struct AtlassianClient {
    pub(crate) http: reqwest::Client,
    pub(crate) config: AppConfig,
    pub(crate) auth: AuthManager,
    pub(crate) me_cache: Arc<RwLock<Option<UserSummary>>>,
    pub(crate) space_id_cache: Arc<RwLock<HashMap<String, (String, String)>>>,
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
            config,
            auth,
            me_cache: Arc::new(RwLock::new(None)),
            space_id_cache: Arc::new(RwLock::new(HashMap::new())),
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
        let mut config = AppConfig::default();
        config.site_url = normalize_base_url(&site.site_url);
        config.confluence_url = normalize_base_url(&site.confluence_url);
        config.deployment = site.deployment;
        config.profile = site.account_id.clone();

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
            config,
            auth,
            me_cache: Arc::new(RwLock::new(None)),
            space_id_cache: Arc::new(RwLock::new(HashMap::new())),
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
        let mut config = AppConfig::default();
        config.site_url = normalize_base_url(&site_url.into());
        config.confluence_url = normalize_base_url(&confluence_url.into());
        config.deployment = deployment;
        Self::new(config, auth)
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
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
        bail!("Atlassian site URL is not configured. Run `jira login --site <URL>` or set ATLASSIAN_SITE_URL.");
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
        bail!("Confluence URL is not configured. Run `confluence login --site <URL>` or set ATLASSIAN_CONFLUENCE_URL.");
    }

    pub async fn get_myself(&self) -> Result<UserSummary> {
        let base = self.require_site_url()?;
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{base}/rest/api/3/myself"),
            DeploymentKind::DataCenter => format!("{base}/rest/api/2/myself"),
        };
        let val: Value = self.get_json(&url).await?;
        let me = parse_user_summary(&val, self.deployment());
        self.set_cached_me(me.clone());
        Ok(me)
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let target_url = self.resolve_api_url(url);
        let resp = self
            .send_with_retry(|auth_header| {
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
            .send_with_retry(|auth_header| {
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
            .send_with_retry(|auth_header| {
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
            .send_with_retry(|auth_header| {
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
            .send_with_retry(|auth_header| {
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
        self.send_with_retry(|auth_header| {
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
            .send_with_retry(|auth_header| {
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
        let (mut auth_header, mut oauth_token) = self.resolve_auth_header().await?;
        let initial_url = Url::parse(&target_url)
            .with_context(|| format!("Invalid attachment URL: {target_url}"))?;
        let no_redirect_client = reqwest::Client::builder()
            .user_agent(format!("atlassian-cli/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to initialize download HTTP client")?;

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
                            let refreshed = self
                                .auth
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

    async fn resolve_auth_header(&self) -> Result<(String, Option<String>)> {
        let creds = self.auth.load_credentials()?.ok_or_else(|| {
            anyhow!("Not signed in. Run `jira login` or `confluence login` first.")
        })?;
        match creds {
            StoredCredentials::CloudOAuth { .. } => {
                let fresh = self
                    .auth
                    .ensure_fresh_credentials(&self.http, &self.oauth_auth_base)
                    .await?;
                let token = fresh.oauth_access_token().map(str::to_string);
                Ok((fresh.authorization_header(), token))
            }
            other => Ok((other.authorization_header(), None)),
        }
    }

    async fn send_with_retry<F>(&self, build_req: F) -> Result<reqwest::Response>
    where
        F: FnMut(&str) -> Result<reqwest::RequestBuilder>,
    {
        self.send_with_retry_policy(build_req, true).await
    }

    async fn send_with_retry_policy<F>(
        &self,
        mut build_req: F,
        retry_statuses: bool,
    ) -> Result<reqwest::Response>
    where
        F: FnMut(&str) -> Result<reqwest::RequestBuilder>,
    {
        let (mut auth_header, mut oauth_token) = self.resolve_auth_header().await?;
        let mut refreshed_on_401 = false;

        loop {
            let mut attempt = 1usize;
            let result = loop {
                let req = build_req(&auth_header)?;
                let resp = req
                    .send()
                    .await
                    .context("Failed to send Atlassian API request")?;
                let status = resp.status();

                if status.is_success() || status.is_redirection() {
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
                    let refreshed = self
                        .auth
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
        let mut accounts: HashMap<String, AuthManager> = HashMap::new();
        let sites = config.effective_sites();
        let http = build_shared_http_client()?;
        let mut entries = Vec::with_capacity(sites.len());

        for site in sites {
            let acct_key = if site.account_id.trim().is_empty() {
                config.profile.clone()
            } else {
                site.account_id.trim().to_string()
            };
            let auth = accounts
                .entry(acct_key.clone())
                .or_insert_with(|| {
                    let mut mgr = AuthManager::for_account(acct_key);
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
            );
            if let Some(ref auth_base) = oauth_auth_base {
                client = client.with_oauth_auth_base(auth_base.clone());
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
        let resp = req
            .send()
            .await
            .context("Failed to send Atlassian API request")?;
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
}
