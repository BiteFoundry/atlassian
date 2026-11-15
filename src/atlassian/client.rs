//! Core HTTP client with Cloud/DataCenter routing, `429 Retry-After` backoff, and auth header injection.

use std::{
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
    auth::AuthManager,
    config::{normalize_base_url, AppConfig, DeploymentKind},
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

#[derive(Clone)]
pub struct AtlassianClient {
    pub(crate) http: reqwest::Client,
    pub(crate) config: AppConfig,
    pub(crate) auth: AuthManager,
    pub(crate) me_cache: Arc<RwLock<Option<UserSummary>>>,
}

impl AtlassianClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("atlassian-cli/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("HTTP 클라이언트 초기화 실패")?;
        Ok(Self {
            http,
            config,
            auth,
            me_cache: Arc::new(RwLock::new(None)),
        })
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

    pub fn deployment(&self) -> DeploymentKind {
        self.config.deployment
    }

    pub fn site_url(&self) -> &str {
        &self.config.site_url
    }

    pub fn confluence_root_url(&self) -> String {
        self.config.effective_confluence_root()
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
        if url.is_empty() {
            bail!("Atlassian 사이트 URL이 설정되지 않았습니다. `jira login --site <URL>` 또는 ATLASSIAN_SITE_URL을 설정하세요.");
        }
        Ok(url)
    }

    pub(crate) fn require_confluence_root(&self) -> Result<String> {
        let url = self.confluence_root_url();
        if url.trim().is_empty() {
            bail!("Confluence URL이 설정되지 않았습니다. `confluence login --site <URL>` 또는 ATLASSIAN_CONFLUENCE_URL을 설정하세요.");
        }
        Ok(url)
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
        let resp = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .get(url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json"))
            })
            .await?;
        decode_json_response(resp, url).await
    }

    pub(crate) async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .post(url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        decode_json_response(resp, url).await
    }

    pub(crate) async fn post_json_empty<B: Serialize>(&self, url: &str, body: &B) -> Result<()> {
        let _ = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .post(url)
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
        let resp = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .put(url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        decode_json_response(resp, url).await
    }

    pub(crate) async fn put_json_empty<B: Serialize>(&self, url: &str, body: &B) -> Result<()> {
        let _ = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .put(url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .json(body))
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_req(&self, url: &str) -> Result<()> {
        let _ = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                Ok(self
                    .http
                    .delete(url)
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
    ) -> Result<Value> {
        let resp = self
            .send_with_retry(|| {
                let auth_header = self.auth.authorization_header()?;
                let part = Part::bytes(bytes.clone())
                    .file_name(file_name.to_string())
                    .mime_str(mime_type)
                    .unwrap_or_else(|_| Part::bytes(bytes.clone()).file_name(file_name.to_string()));
                let form = Form::new().part(field_name.to_string(), part);
                Ok(self
                    .http
                    .post(url)
                    .header(AUTHORIZATION, auth_header)
                    .header(ACCEPT, "application/json")
                    .header("X-Atlassian-Token", "no-check")
                    .multipart(form))
            })
            .await?;
        decode_json_response(resp, url).await
    }

    /// Fetches binary bytes while preserving `Authorization` on same-origin redirects
    /// and stripping it on cross-origin redirects (e.g., signed Atlassian Media / S3 CDN URLs).
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let auth_header = self.auth.authorization_header()?;
        let initial_url =
            Url::parse(url).with_context(|| format!("잘못된 첨부파일 URL: {url}"))?;
        let no_redirect_client = reqwest::Client::builder()
            .user_agent(format!("atlassian-cli/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("다운로드 HTTP 클라이언트 초기화 실패")?;

        let mut current_url = initial_url.clone();
        for _hop in 0..=MAX_REDIRECT_HOPS {
            let send_auth = is_same_origin(&initial_url, &current_url);
            let resp = send_request_with_retry_client(&no_redirect_client, || {
                let mut req = no_redirect_client
                    .request(Method::GET, current_url.clone())
                    .header(ACCEPT, "*/*");
                if send_auth {
                    req = req.header(AUTHORIZATION, auth_header.as_str());
                }
                Ok(req)
            })
            .await?;

            let status = resp.status();
            if status.is_redirection() {
                let loc = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("리다이렉트 응답({status})에 Location 헤더가 없습니다."))?;
                current_url = current_url
                    .join(loc)
                    .with_context(|| format!("리다이렉트 URL 해석 실패: {loc}"))?;
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
                .context("첨부파일 바이트 응답 읽기 실패")?;
            return Ok(bytes.to_vec());
        }

        bail!("첨부파일 다운로드 리다이렉트 횟수({MAX_REDIRECT_HOPS}회)를 초과했습니다: {url}")
    }

    async fn send_with_retry<F>(&self, build_req: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> Result<reqwest::RequestBuilder>,
    {
        send_request_with_retry_client(&self.http, build_req).await
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
        let resp = req.send().await.context("Atlassian API 요청 전송 실패")?;
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
        .with_context(|| format!("응답 본문 읽기 실패: {url}"))?;
    serde_json::from_str::<T>(&text).with_context(|| {
        let preview: String = text.chars().take(240).collect();
        format!("Atlassian JSON 응답 파싱 실패 ({url}): {preview}")
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
            format!("Atlassian API 요청 실패 (HTTP {status})")
        } else {
            let preview: String = trimmed.chars().take(300).collect();
            format!("Atlassian API 요청 실패 (HTTP {status}): {preview}")
        }
    } else {
        format!(
            "Atlassian API 요청 실패 (HTTP {status}): {}",
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
    let mime_type = first_nonempty_str(val, &["mimeType", "mediaType"])
        .if_empty_then(|| {
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
    use crate::auth::StoredCredentials;
    use axum::{
        http::{HeaderMap, StatusCode as AxumStatus},
        routing::get,
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
                        "displayName": "김철수",
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
        assert_eq!(me.display_name, "김철수");
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
                            "errorMessages": ["프로젝트 키가 올바르지 않습니다."],
                            "errors": { "summary": "필수 입력 항목입니다." }
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
        assert!(msg.contains("프로젝트 키가 올바르지 않습니다."));
        assert!(msg.contains("summary: 필수 입력 항목입니다."));
    }
}
