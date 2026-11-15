//! Atlassian authentication manager: Cloud (`Basic base64(email:api_token)`) + Data Center (`Bearer PAT` or `Basic`) + OS Keyring + file fallback.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::config::{credentials_fallback_path, normalize_base_url, AppConfig, DeploymentKind};

const KEYRING_SERVICE: &str = "atlassian-cli";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "auth_kind", rename_all = "snake_case")]
pub enum StoredCredentials {
    /// Atlassian Cloud (`email` + API Token -> `Authorization: Basic base64(email:token)`).
    ApiToken {
        site_url: String,
        email: String,
        api_token: String,
        deployment: DeploymentKind,
    },
    /// Personal Access Token or OAuth 2.0 Bearer token (`Authorization: Bearer <token>`).
    BearerPat {
        site_url: String,
        token: String,
        deployment: DeploymentKind,
    },
}

impl StoredCredentials {
    pub fn site_url(&self) -> &str {
        match self {
            Self::ApiToken { site_url, .. } | Self::BearerPat { site_url, .. } => site_url,
        }
    }

    pub fn deployment(&self) -> DeploymentKind {
        match self {
            Self::ApiToken { deployment, .. } | Self::BearerPat { deployment, .. } => *deployment,
        }
    }

    pub fn authorization_header(&self) -> String {
        match self {
            Self::ApiToken {
                email, api_token, ..
            } => {
                let raw = format!("{}:{}", email.trim(), api_token.trim());
                format!("Basic {}", BASE64_STANDARD.encode(raw.as_bytes()))
            }
            Self::BearerPat { token, .. } => format!("Bearer {}", token.trim()),
        }
    }

    pub fn status_summary(&self) -> String {
        match self {
            Self::ApiToken {
                email, deployment, ..
            } => match deployment {
                DeploymentKind::Cloud => format!("로그인됨 (Cloud API Token: {email})"),
                DeploymentKind::DataCenter => format!("로그인됨 (Data Center Basic: {email})"),
            },
            Self::BearerPat { deployment, .. } => match deployment {
                DeploymentKind::Cloud => "로그인됨 (Cloud Bearer PAT)".to_string(),
                DeploymentKind::DataCenter => "로그인됨 (Data Center PAT)".to_string(),
            },
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

#[derive(Debug, Clone)]
pub struct AuthManager {
    profile: String,
    direct_credentials: Option<StoredCredentials>,
    fallback_path_override: Option<PathBuf>,
}

impl AuthManager {
    pub fn new(profile: impl Into<String>) -> Self {
        let profile = profile.into();
        let profile = if profile.trim().is_empty() {
            "default".to_string()
        } else {
            profile.trim().to_string()
        };
        Self {
            profile,
            direct_credentials: None,
            fallback_path_override: None,
        }
    }

    /// Creates an in-memory `AuthManager` with fixed credentials (never touches OS keyring or disk).
    pub fn with_credentials(profile: impl Into<String>, creds: StoredCredentials) -> Self {
        let mut mgr = Self::new(profile);
        mgr.direct_credentials = Some(creds);
        mgr
    }

    /// Alias kept for convenience when constructing static credentials in tests or callers.
    pub fn from_static(profile: impl Into<String>, creds: StoredCredentials) -> Self {
        Self::with_credentials(profile, creds)
    }

    /// Overrides the fallback credentials file path (and bypasses OS keyring) for isolated testing.
    pub fn with_fallback_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.fallback_path_override = Some(path.into());
        self
    }

    pub fn profile(&self) -> &str {
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

    pub fn load_credentials(&self) -> Result<Option<StoredCredentials>> {
        if let Some(ref creds) = self.direct_credentials {
            return Ok(Some(creds.clone()));
        }

        if let Some(env_creds) = load_credentials_from_env() {
            return Ok(Some(env_creds));
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
        load_profile_from_file(&fallback, &self.profile)
    }

    pub fn save_credentials(&self, creds: &StoredCredentials) -> Result<()> {
        let fallback = self.effective_fallback_path();
        if !self.use_keyring() {
            return save_profile_to_file(&fallback, &self.profile, creds);
        }

        let json = serde_json::to_string_pretty(creds).context("자격 증명 직렬화 실패")?;
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
            .ok_or_else(|| anyhow!("API 토큰 또는 PAT를 입력하세요."))?;
        let email = opts
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("API 토큰 로그인에는 이메일(또는 사용자명)이 필요합니다."))?;

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
            anyhow!("로그인되어 있지 않습니다. `jira login` 또는 `confluence login`을 먼저 실행하세요.")
        })?;
        Ok(creds.authorization_header())
    }

    pub fn is_logged_in(&self) -> bool {
        self.load_credentials().ok().flatten().is_some()
    }

    pub fn status_summary(&self) -> String {
        match self.load_credentials() {
            Ok(Some(creds)) => creds.status_summary(),
            _ => "미로그인".to_string(),
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
        .with_context(|| format!("자격 증명 파일 읽기 실패: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str::<CredentialsMap>(&raw)
        .with_context(|| format!("자격 증명 파일 파싱 실패: {}", path.display()))
}

fn load_profile_from_file(path: &Path, profile: &str) -> Result<Option<StoredCredentials>> {
    let map = load_credentials_map(path)?;
    Ok(map.get(profile).cloned())
}

fn save_profile_to_file(path: &Path, profile: &str, creds: &StoredCredentials) -> Result<()> {
    let mut map = load_credentials_map(path).unwrap_or_default();
    map.insert(profile.to_string(), creds.clone());
    let json = serde_json::to_vec_pretty(&map).context("자격 증명 맵 직렬화 실패")?;
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
            .with_context(|| format!("자격 증명 파일 삭제 실패: {}", path.display()))?;
        return Ok(());
    }
    let json = serde_json::to_vec_pretty(&map).context("자격 증명 맵 직렬화 실패")?;
    write_atomic(path, &json)
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("디렉터리 생성 실패: {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data).with_context(|| format!("임시 파일 쓰기 실패: {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, path) {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("기존 파일 교체 실패: {}", path.display()))?;
            fs::rename(&tmp, path)
                .with_context(|| format!("원자적 저장 실패: {}", path.display()))?;
        } else {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("파일 저장 실패: {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
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
        assert_eq!(summary, "로그인됨 (Cloud API Token: user@acme.com)");
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
        assert_eq!(summary, "로그인됨 (Data Center PAT)");
        assert!(!summary.contains("pat-secret-xyz"));
    }

    #[test]
    fn test_isolated_file_fallback_multi_profile_roundtrip_and_delete() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::clear(ALL_AUTH_ENVS);

        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");

        let default_mgr = AuthManager::new("default").with_fallback_path(cred_path.clone());
        let dc_mgr = AuthManager::new("dc").with_fallback_path(cred_path.clone());

        assert!(!default_mgr.is_logged_in());
        assert_eq!(default_mgr.status_summary(), "미로그인");
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
        // ATLASSIAN_SITE_URL still set, so remove it to test JIRA_URL fallback
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
}
