//! Application configuration, Persona detection (`jira` / `confluence` / `conf`), and Cloud vs Data Center deployment settings.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_GITHUB_REPO: &str = "shinick-dev/atlassian";
pub const DEFAULT_SERVE_PORT: u16 = 8789;

/// Binary invocation persona (`jira` vs `confluence` / `conf`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    #[default]
    Jira,
    Confluence,
}

impl Persona {
    /// Detects persona from `ATLASSIAN_PERSONA` env var first, then `argv[0]` stem (`confluence` / `conf`).
    pub fn detect() -> Self {
        if let Ok(raw) = std::env::var("ATLASSIAN_PERSONA") {
            let trimmed = raw.trim().to_ascii_lowercase();
            if trimmed == "confluence" || trimmed == "conf" || trimmed == "wiki" {
                return Self::Confluence;
            }
            if trimmed == "jira" {
                return Self::Jira;
            }
        }
        if let Some(arg0) = std::env::args_os().next() {
            if let Some(stem) = Path::new(&arg0).file_stem().and_then(|s| s.to_str()) {
                let lower = stem.to_ascii_lowercase();
                if lower.starts_with("confluence") || lower == "conf" {
                    return Self::Confluence;
                }
            }
        }
        Self::Jira
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jira => "jira",
            Self::Confluence => "confluence",
        }
    }

    pub fn display_title(self) -> &'static str {
        match self {
            Self::Jira => "Atlassian Jira",
            Self::Confluence => "Atlassian Confluence",
        }
    }
}

/// Target Atlassian deployment flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentKind {
    /// Atlassian Cloud (`https://<site>.atlassian.net`): Jira v3 (`/search/jql`), Agile v1, Confluence v2 (`/wiki/api/v2`).
    #[default]
    Cloud,
    /// Jira / Confluence Data Center or Server (on-premise): Jira v2 (`/rest/api/2`), Agile v1, Confluence v1 (`/rest/api/content`).
    DataCenter,
}

impl DeploymentKind {
    /// Auto-infers deployment flavor from the site URL when not explicitly specified.
    pub fn infer_from_url(site_url: &str) -> Self {
        let lower = site_url.trim().to_ascii_lowercase();
        if lower.contains(".atlassian.net") || lower.contains(".jira.com") {
            Self::Cloud
        } else {
            Self::DataCenter
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::DataCenter => "datacenter",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Base Atlassian site URL (e.g. `https://acme.atlassian.net` or `https://jira.company.com`).
    pub site_url: String,
    /// Optional separate Confluence base URL for Data Center setups where Jira and Confluence live on different hosts
    /// (e.g. `https://confluence.company.com`). When empty, derived from `site_url` (`{site_url}/wiki` on Cloud or `{site_url}` on DC).
    pub confluence_url: String,
    /// Cloud vs Data Center API dialect.
    pub deployment: DeploymentKind,
    /// Default Jira project key (e.g. `PROJ`).
    pub default_project: Option<String>,
    /// Default Agile board ID.
    pub default_board_id: Option<i64>,
    /// Default Confluence space key (e.g. `ENG`).
    pub default_space_key: Option<String>,
    /// Poll interval in seconds for background sync.
    pub sync_interval_secs: u64,
    /// Enable desktop notifications on newly assigned/updated issues.
    pub notify: bool,
    /// Show leftmost app rail in wide terminals.
    pub show_nav_rail: bool,
    /// Automatically show right detail/TOC drawer in wide terminals.
    pub auto_detail_drawer: bool,
    /// Render inline image thumbnails in TUI and CLI when supported.
    pub inline_images: bool,
    /// Download destination directory for attachments.
    pub download_dir: Option<PathBuf>,
    /// GitHub repository (`owner/repo`) for `jira update` / `confluence update`.
    pub github_repo: String,
    /// Active profile name (`default` unless overridden).
    pub profile: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            site_url: String::new(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            default_project: None,
            default_board_id: None,
            default_space_key: None,
            sync_interval_secs: 45,
            notify: true,
            show_nav_rail: true,
            auto_detail_drawer: true,
            inline_images: true,
            download_dir: None,
            github_repo: DEFAULT_GITHUB_REPO.to_string(),
            profile: "default".to_string(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_file_path();
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("설정 파일 읽기 실패: {}", path.display()))?;
            toml::from_str::<AppConfig>(&raw)
                .with_context(|| format!("설정 파일 파싱 실패: {}", path.display()))?
        } else {
            Self::default()
        };

        if let Ok(url) = std::env::var("ATLASSIAN_SITE_URL")
            .or_else(|_| std::env::var("JIRA_URL"))
        {
            if !url.trim().is_empty() {
                cfg.site_url = normalize_base_url(&url);
                if std::env::var("ATLASSIAN_DEPLOYMENT").is_err() && !path.exists() {
                    cfg.deployment = DeploymentKind::infer_from_url(&cfg.site_url);
                }
            }
        }
        if let Ok(conf_url) = std::env::var("ATLASSIAN_CONFLUENCE_URL")
            .or_else(|_| std::env::var("CONFLUENCE_URL"))
        {
            if !conf_url.trim().is_empty() {
                cfg.confluence_url = normalize_base_url(&conf_url);
            }
        }
        if let Ok(dep) = std::env::var("ATLASSIAN_DEPLOYMENT") {
            match dep.trim().to_ascii_lowercase().as_str() {
                "cloud" => cfg.deployment = DeploymentKind::Cloud,
                "datacenter" | "dc" | "server" => cfg.deployment = DeploymentKind::DataCenter,
                _ => {}
            }
        }
        if let Ok(proj) = std::env::var("JIRA_DEFAULT_PROJECT") {
            if !proj.trim().is_empty() {
                cfg.default_project = Some(proj.trim().to_string());
            }
        }
        if let Ok(space) = std::env::var("CONFLUENCE_DEFAULT_SPACE") {
            if !space.trim().is_empty() {
                cfg.default_space_key = Some(space.trim().to_string());
            }
        }
        if let Ok(repo) = std::env::var("ATLASSIAN_GITHUB_REPO") {
            if !repo.trim().is_empty() {
                cfg.github_repo = repo.trim().to_string();
            }
        }
        cfg.site_url = normalize_base_url(&cfg.site_url);
        cfg.confluence_url = normalize_base_url(&cfg.confluence_url);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).context("설정 직렬화 실패")?;
        std::fs::write(&path, body)
            .with_context(|| format!("설정 파일 저장 실패: {}", path.display()))
    }

    pub fn jira_base_url(&self) -> String {
        normalize_base_url(&self.site_url)
    }

    /// Returns the root URL for Confluence.
    /// - Cloud (`https://acme.atlassian.net`): `https://acme.atlassian.net/wiki` (unless `confluence_url` overrides it)
    /// - Data Center: `confluence_url` if non-empty, otherwise `site_url`
    pub fn effective_confluence_root(&self) -> String {
        if !self.confluence_url.trim().is_empty() {
            return normalize_base_url(&self.confluence_url);
        }
        let base = normalize_base_url(&self.site_url);
        if base.is_empty() {
            return String::new();
        }
        match self.deployment {
            DeploymentKind::Cloud => {
                if base.ends_with("/wiki") {
                    base
                } else {
                    format!("{base}/wiki")
                }
            }
            DeploymentKind::DataCenter => base,
        }
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if let Some(ref d) = self.download_dir {
            return d.clone();
        }
        dirs::download_dir()
            .unwrap_or_else(|| config_dir().join("downloads"))
            .join("atlassian")
    }
}

pub fn normalize_base_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("ATLASSIAN_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("atlassian-cli")
}

pub fn data_dir(profile: &str) -> PathBuf {
    if let Ok(override_dir) = std::env::var("ATLASSIAN_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir).join(profile);
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(config_dir)
        .join("atlassian-cli")
        .join(profile)
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credentials_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn drafts_file_path() -> PathBuf {
    config_dir().join("drafts.json")
}

pub fn cache_db_path(profile: &str) -> PathBuf {
    data_dir(profile).join("cache.db")
}
