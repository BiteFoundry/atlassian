//! Application configuration, Persona detection (`jira` / `confluence` / `conf`),
//! Cloud vs Data Center deployment settings, UI preferences, and persistent drafts (`drafts.json`).

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/atlassian";
pub const DEFAULT_SERVE_PORT: u16 = 8789;

/// Layout density mode in TUI (`Cozy` spacious default vs `Compact` high-density rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    #[default]
    Cozy,
    Compact,
}

impl DensityMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cozy => Self::Compact,
            Self::Compact => Self::Cozy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cozy => "Cozy",
            Self::Compact => "Compact",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cozy" => Some(Self::Cozy),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    #[default]
    Auto,
    Dark,
    Light,
    Ansi256,
    Ansi16,
    #[serde(alias = "no-color", alias = "no_color")]
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Dark => "Midnight Dark",
            Self::Light => "Daylight Light",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "dark" | "midnight" | "midnight_dark" => Some(Self::Dark),
            "light" | "daylight" | "daylight_light" => Some(Self::Light),
            "ansi256" | "256" => Some(Self::Ansi256),
            "ansi16" | "16" => Some(Self::Ansi16),
            "nocolor" | "no-color" | "no_color" | "none" => Some(Self::NoColor),
            _ => None,
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(rename = "iterm2", alias = "iterm")]
    ITerm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Kitty,
            Self::Kitty => Self::ITerm2,
            Self::ITerm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Auto,
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::ITerm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::ITerm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::ITerm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }
}

/// Binary invocation persona (`atlassian` vs `confluence` / `conf`).
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
            if trimmed == "jira" || trimmed == "atlassian" {
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

    pub fn command_name(self) -> &'static str {
        match self {
            Self::Jira => "atlassian",
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

/// A registered Atlassian Cloud or Data Center site profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteProfile {
    /// Stable slug identifier (e.g. `"acme"`, `"corp-dc"`, `"default"`).
    pub id: String,
    /// Human-readable site display name (e.g. `"Acme Cloud"`).
    pub name: String,
    /// Human-facing Jira / site base URL (e.g. `https://acme.atlassian.net`).
    pub site_url: String,
    /// Optional separate Confluence base URL (primarily for Data Center setups).
    pub confluence_url: String,
    /// Cloud vs Data Center API dialect.
    pub deployment: DeploymentKind,
    /// Atlassian Cloud `cloudId` discovered via OAuth 2.0 (3LO) `/accessible-resources`.
    /// When `Some`, API calls route through `https://api.atlassian.com/ex/{jira,confluence}/{cloud_id}`.
    pub cloud_id: Option<String>,
    /// Account identifier in `AuthManager` / OS Keyring (multiple sites can share one account).
    pub account_id: String,
    /// Whether this site participates in federated sync and search.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// OAuth scopes granted for this Cloud site (empty for API token / PAT sites).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Separate credential account for Confluence (Data Center PATs are per application).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confluence_account_id: Option<String>,
}

impl Default for SiteProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            site_url: String::new(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: None,
            account_id: "default".to_string(),
            enabled: true,
            scopes: Vec::new(),
            confluence_account_id: None,
        }
    }
}

impl SiteProfile {
    pub fn normalized_site_url(&self) -> String {
        normalize_base_url(&self.site_url)
    }

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

    pub fn display_name(&self) -> &str {
        let trimmed = self.name.trim();
        if !trimmed.is_empty() {
            trimmed
        } else {
            &self.id
        }
    }

    pub fn is_oauth_cloud(&self) -> bool {
        self.cloud_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Base Atlassian site URL (e.g. `https://acme.atlassian.net` or `https://jira.company.com`).
    pub site_url: String,
    /// Optional separate Confluence base URL for Data Center setups where Jira and Confluence live on different hosts
    /// (e.g. `https://confluence.company.com`). When empty, derived from `site_url` (`{site_url}/wiki` on Cloud or `{site_url}` on DC).
    pub confluence_url: String,
    /// Cloud vs Data Center API dialect.
    pub deployment: DeploymentKind,
    /// Multi-site profiles (`[[sites]]` in `config.toml`).
    #[serde(default)]
    pub sites: Vec<SiteProfile>,
    /// Active TUI site filter (`None` = All Sites federated view).
    #[serde(default)]
    pub active_site: Option<String>,
    /// Default target site ID for single-site CLI / MCP / HTTP commands when `--site` is omitted.
    #[serde(default)]
    pub default_site: Option<String>,
    /// Optional Atlassian Cloud OAuth 2.0 (3LO) client ID (overridden by `ATLASSIAN_OAUTH_CLIENT_ID`).
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Optional Atlassian Cloud OAuth 2.0 (3LO) client secret (overridden by `ATLASSIAN_OAUTH_CLIENT_SECRET`).
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    /// Default Jira project key (e.g. `PROJ`).
    pub default_project: Option<String>,
    /// Default Agile board ID.
    pub default_board_id: Option<i64>,
    /// Default Confluence space key (e.g. `ENG`).
    pub default_space_key: Option<String>,
    /// Poll interval in seconds for background sync.
    pub sync_interval_secs: u64,
    /// TUI color theme preset (`auto`, `dark`, `light`, `ansi256`, `ansi16`, `nocolor`).
    #[serde(default)]
    pub theme: ThemePreset,
    /// TUI layout density (`cozy` or `compact`).
    #[serde(default)]
    pub density: DensityMode,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    #[serde(default)]
    pub image_protocol: ImageProtocolPreference,
    /// Render inline image thumbnails in TUI and CLI when supported.
    #[serde(default = "default_true")]
    pub inline_images: bool,
    /// Enable desktop notifications on newly assigned/updated issues.
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Show left navigation sidebar in wide/medium terminals.
    #[serde(default = "default_true")]
    pub show_nav_rail: bool,
    /// Automatically show right detail/TOC drawer in wide terminals.
    #[serde(default = "default_true")]
    pub auto_detail_drawer: bool,
    /// Download destination directory for attachments.
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// GitHub repository (`owner/repo`) for `atlassian update` / `confluence update`.
    pub github_repo: String,
    /// Active profile name (`default` unless overridden).
    pub profile: String,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            site_url: String::new(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            sites: Vec::new(),
            active_site: None,
            default_site: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            default_project: None,
            default_board_id: None,
            default_space_key: None,
            sync_interval_secs: 45,
            theme: std::env::var("ATLASSIAN_THEME")
                .ok()
                .and_then(|s| ThemePreset::parse_str(&s))
                .unwrap_or_default(),
            density: std::env::var("ATLASSIAN_DENSITY")
                .ok()
                .and_then(|s| DensityMode::parse_str(&s))
                .unwrap_or_default(),
            image_protocol: std::env::var("ATLASSIAN_IMAGE_PROTOCOL")
                .ok()
                .and_then(|s| ImageProtocolPreference::parse_str(&s))
                .unwrap_or_default(),
            inline_images: true,
            notify: true,
            show_nav_rail: true,
            auto_detail_drawer: true,
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
            Self::load_from(&path)?
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
        if let Ok(theme) = std::env::var("ATLASSIAN_THEME") {
            if let Some(parsed) = ThemePreset::parse_str(&theme) {
                cfg.theme = parsed;
            }
        }
        if let Ok(density) = std::env::var("ATLASSIAN_DENSITY") {
            if let Some(parsed) = DensityMode::parse_str(&density) {
                cfg.density = parsed;
            }
        }
        if let Ok(proto) = std::env::var("ATLASSIAN_IMAGE_PROTOCOL") {
            if !proto.trim().is_empty() {
                if let Some(parsed) = ImageProtocolPreference::parse_str(&proto) {
                    cfg.image_protocol = parsed;
                }
            }
        }
        cfg.site_url = normalize_base_url(&cfg.site_url);
        cfg.confluence_url = normalize_base_url(&cfg.confluence_url);
        for site in &mut cfg.sites {
            site.site_url = normalize_base_url(&site.site_url);
            site.confluence_url = normalize_base_url(&site.confluence_url);
            if site.id.trim().is_empty() {
                site.id = slugify_site_id(if !site.site_url.is_empty() {
                    &site.site_url
                } else {
                    &site.name
                });
            }
            if site.account_id.trim().is_empty() {
                site.account_id = cfg.profile.clone();
            }
        }
        Ok(cfg)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str::<AppConfig>(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        #[cfg(test)]
        if std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, body.as_bytes())
            .with_context(|| format!("Failed to save config file: {}", path.display()))
    }

    /// Returns all configured sites, transparently synthesizing a `"default"` site from
    /// legacy `site_url` / `confluence_url` / `deployment` when `sites` is empty and
    /// `site_url` is non-empty.
    pub fn effective_sites(&self) -> Vec<SiteProfile> {
        if !self.sites.is_empty() {
            return self
                .sites
                .iter()
                .cloned()
                .map(|mut s| {
                    s.site_url = normalize_base_url(&s.site_url);
                    s.confluence_url = normalize_base_url(&s.confluence_url);
                    if s.id.trim().is_empty() {
                        s.id = slugify_site_id(if !s.site_url.is_empty() {
                            &s.site_url
                        } else {
                            &s.name
                        });
                    }
                    if s.account_id.trim().is_empty() {
                        s.account_id = if self.profile.trim().is_empty() {
                            "default".to_string()
                        } else {
                            self.profile.trim().to_string()
                        };
                    }
                    s
                })
                .collect();
        }

        let legacy_url = normalize_base_url(&self.site_url);
        if legacy_url.is_empty() {
            return Vec::new();
        }

        let account_id = if self.profile.trim().is_empty() {
            "default".to_string()
        } else {
            self.profile.trim().to_string()
        };
        let derived_name = slugify_site_id(&legacy_url);
        vec![SiteProfile {
            id: "default".to_string(),
            name: if derived_name.is_empty() {
                "default".to_string()
            } else {
                derived_name
            },
            site_url: legacy_url,
            confluence_url: normalize_base_url(&self.confluence_url),
            deployment: self.deployment,
            cloud_id: None,
            account_id,
            enabled: true,
            scopes: Vec::new(),
            confluence_account_id: None,
        }]
    }

    /// Inserts or updates a `SiteProfile` (matching by `id`, then `cloud_id`, then normalized `site_url`).
    pub fn upsert_site(&mut self, mut site: SiteProfile) {
        site.site_url = normalize_base_url(&site.site_url);
        site.confluence_url = normalize_base_url(&site.confluence_url);
        if site.id.trim().is_empty() {
            site.id = slugify_site_id(if !site.site_url.is_empty() {
                &site.site_url
            } else {
                &site.name
            });
        } else {
            site.id = site.id.trim().to_string();
        }
        if site.name.trim().is_empty() {
            site.name = site.id.clone();
        }
        if site.account_id.trim().is_empty() {
            site.account_id = if self.profile.trim().is_empty() {
                "default".to_string()
            } else {
                self.profile.trim().to_string()
            };
        }

        let incoming_cloud_id = site
            .cloud_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        if let Some(existing) = self
            .sites
            .iter_mut()
            .find(|s| s.id.eq_ignore_ascii_case(&site.id))
        {
            *existing = site;
            return;
        }
        if let Some(ref cid) = incoming_cloud_id {
            if let Some(existing) = self.sites.iter_mut().find(|s| {
                s.cloud_id
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|ecid| ecid == cid)
            }) {
                *existing = site;
                return;
            }
        }
        if !site.site_url.is_empty() {
            if let Some(existing) = self
                .sites
                .iter_mut()
                .find(|s| normalize_base_url(&s.site_url).eq_ignore_ascii_case(&site.site_url))
            {
                *existing = site;
                return;
            }
        }

        self.sites.push(site);
    }

    /// Copies a legacy single-site configuration (top-level site_url) into "sites" before a
    /// first [Self::upsert_site], so adding a site never drops the one configured the old way.
    pub fn materialize_legacy_site(&mut self) {
        if self.sites.is_empty() && !self.site_url.trim().is_empty() {
            self.sites = self.effective_sites();
        }
    }

    /// Makes the site matching "query" the default site and mirrors it into the legacy
    /// site_url / confluence_url / deployment fields. A persisted TUI selection
    /// (active_site) naming another site would shadow the new default, so it is cleared;
    /// the cleared id is returned.
    pub fn set_default_site(&mut self, query: &str) -> Result<Option<String>> {
        self.materialize_legacy_site();
        let found = self
            .find_effective_site(query)
            .ok_or_else(|| anyhow::anyhow!("Site '{}' not found.", query.trim()))?;
        self.default_site = Some(found.id.clone());
        self.site_url = found.site_url.clone();
        self.confluence_url = found.confluence_url.clone();
        self.deployment = found.deployment;
        let shadows = self
            .active_site
            .as_deref()
            .map(|active| {
                self.find_effective_site(active)
                    .is_none_or(|site| !site.id.eq_ignore_ascii_case(&found.id))
            })
            .unwrap_or(false);
        Ok(if shadows { self.active_site.take() } else { None })
    }

    /// The site scope of one request (MCP tool call, HTTP request). A per-call "site" wins:
    /// "all" means every site (Ok(None)), a configured site its canonical id, anything else is
    /// an error naming the configured sites. Without a per-call site the launch-time --site
    /// (active_site) applies, else every site. "current_site_id" (the serving client's site)
    /// and the legacy "default" id are always accepted.
    pub fn resolve_site_scope(
        &self,
        raw_site: Option<&str>,
        current_site_id: &str,
    ) -> std::result::Result<Option<String>, String> {
        match raw_site.map(str::trim).filter(|s| !s.is_empty()) {
            Some(site) if site.eq_ignore_ascii_case("all") => Ok(None),
            Some(site) => self
                .canonical_site_id(site, current_site_id)
                .map(Some)
                .ok_or_else(|| self.unknown_site_message(site)),
            None => Ok(self.active_site.as_deref().map(|active| {
                self.canonical_site_id(active, current_site_id)
                    .unwrap_or_else(|| active.trim().to_string())
            })),
        }
    }

    fn canonical_site_id(&self, query: &str, current_site_id: &str) -> Option<String> {
        let q = query.trim();
        if let Some(found) = self.find_effective_site(q) {
            return Some(found.id);
        }
        if q.eq_ignore_ascii_case(current_site_id) {
            return Some(current_site_id.to_string());
        }
        q.eq_ignore_ascii_case("default").then(|| "default".to_string())
    }

    /// "unknown site 'x'; configured sites: a, b" (the error of a mistyped per-call site).
    pub fn unknown_site_message(&self, raw_site: &str) -> String {
        let ids: Vec<String> = self.effective_sites().into_iter().map(|s| s.id).collect();
        if ids.is_empty() {
            format!("unknown site '{}'; no sites are configured", raw_site.trim())
        } else {
            format!("unknown site '{}'; configured sites: {}", raw_site.trim(), ids.join(", "))
        }
    }

    /// A write goes to exactly one site, so "site=all" is refused with the configured sites.
    pub fn reject_all_sites_for_write(&self, raw_site: Option<&str>) -> std::result::Result<(), String> {
        if raw_site.map(str::trim).is_some_and(|s| s.eq_ignore_ascii_case("all")) {
            let ids: Vec<String> = self.effective_sites().into_iter().map(|s| s.id).collect();
            return Err(format!(
                "site 'all' cannot be used for a write; name one site ({})",
                if ids.is_empty() { "none configured".to_string() } else { ids.join(", ") }
            ));
        }
        Ok(())
    }

    /// Removes the site matching "query" (id, name or URL) and returns it with its canonical
    /// id, for deleting its cache. Removing the last site also clears the legacy URL fields,
    /// which would otherwise bring it back as the implicit "default" site.
    pub fn remove_effective_site(&mut self, query: &str) -> Option<SiteProfile> {
        self.materialize_legacy_site();
        let found = self.find_effective_site(query)?;
        if !self.remove_site(&found.id) {
            return None;
        }
        if self.sites.is_empty() {
            self.site_url.clear();
            self.confluence_url.clear();
        } else if let Some(next) = self.default_site_profile() {
            // Keep the legacy mirror pointing at a site that still exists.
            if normalize_base_url(&self.site_url) == found.site_url {
                self.site_url = next.site_url.clone();
                self.confluence_url = next.confluence_url.clone();
                self.deployment = next.deployment;
            }
        }
        Some(found)
    }

    /// Removes a site profile by ID (case-insensitive). Returns `true` if removed.
    pub fn remove_site(&mut self, id: &str) -> bool {
        let target = id.trim();
        if target.is_empty() {
            return false;
        }
        let before = self.sites.len();
        self.sites.retain(|s| !s.id.eq_ignore_ascii_case(target));
        let removed = self.sites.len() < before;
        if removed {
            if self
                .active_site
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(target))
            {
                self.active_site = None;
            }
            if self
                .default_site
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(target))
            {
                self.default_site = None;
            }
        }
        removed
    }

    /// Finds a configured site by `id`, `name`, or normalized URL (case-insensitive).
    pub fn find_site(&self, query: &str) -> Option<&SiteProfile> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        if let Some(s) = self.sites.iter().find(|s| s.id.eq_ignore_ascii_case(q)) {
            return Some(s);
        }
        if let Some(s) = self
            .sites
            .iter()
            .find(|s| s.name.trim().eq_ignore_ascii_case(q))
        {
            return Some(s);
        }
        let norm_q = normalize_url_for_match(q);
        if let Some(s) = self.sites.iter().find(|s| {
            let norm_site = normalize_url_for_match(&s.site_url);
            !norm_site.is_empty() && norm_site == norm_q
        }) {
            return Some(s);
        }
        self.sites.iter().find(|s| {
            s.cloud_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|cid| cid.eq_ignore_ascii_case(q))
        })
    }

    /// Same matching rules as [`Self::find_site`], but also considers the synthesized legacy `"default"` site.
    pub fn find_effective_site(&self, query: &str) -> Option<SiteProfile> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        let sites = self.effective_sites();
        if let Some(s) = sites.iter().find(|s| s.id.eq_ignore_ascii_case(q)) {
            return Some(s.clone());
        }
        if let Some(s) = sites
            .iter()
            .find(|s| s.name.trim().eq_ignore_ascii_case(q))
        {
            return Some(s.clone());
        }
        let norm_q = normalize_url_for_match(q);
        if let Some(s) = sites.iter().find(|s| {
            let norm_site = normalize_url_for_match(&s.site_url);
            !norm_site.is_empty() && norm_site == norm_q
        }) {
            return Some(s.clone());
        }
        sites
            .into_iter()
            .find(|s| {
                s.cloud_id
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|cid| cid.eq_ignore_ascii_case(q))
            })
    }

    /// Returns all enabled sites from [`Self::effective_sites`].
    pub fn enabled_sites(&self) -> Vec<SiteProfile> {
        self.effective_sites()
            .into_iter()
            .filter(|s| s.enabled)
            .collect()
    }

    /// Resolves the default site profile (`default_site` -> `active_site` -> first enabled site).
    pub fn default_site_profile(&self) -> Option<SiteProfile> {
        if let Some(ref def_id) = self.default_site {
            if let Some(site) = self.find_effective_site(def_id) {
                return Some(site);
            }
        }
        if let Some(ref active_id) = self.active_site {
            if let Some(site) = self.find_effective_site(active_id) {
                return Some(site);
            }
        }
        self.enabled_sites().into_iter().next()
    }

    /// Resolves the effective target site profile in priority order:
    /// explicit `active_site` (`--site`), configured `default_site`, then first enabled/legacy site.
    pub fn resolved_site_profile(&self) -> Option<SiteProfile> {
        if let Some(ref active_id) = self.active_site {
            if let Some(site) = self.find_effective_site(active_id) {
                return Some(site);
            }
        }
        self.default_site_profile()
    }

    /// Resolves OAuth 2.0 (3LO) client ID (`ATLASSIAN_OAUTH_CLIENT_ID` env var takes precedence).
    pub fn effective_oauth_client_id(&self) -> Option<String> {
        if let Ok(env_val) = std::env::var("ATLASSIAN_OAUTH_CLIENT_ID") {
            let trimmed = env_val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        self.oauth_client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Resolves OAuth 2.0 (3LO) client secret (`ATLASSIAN_OAUTH_CLIENT_SECRET` env var takes precedence).
    pub fn effective_oauth_client_secret(&self) -> Option<String> {
        if let Ok(env_val) = std::env::var("ATLASSIAN_OAUTH_CLIENT_SECRET") {
            let trimmed = env_val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        self.oauth_client_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
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
            let s = d.to_string_lossy();
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return expand_tilde(trimmed);
            }
        }
        dirs::download_dir()
            .unwrap_or_else(|| config_dir().join("downloads"))
            .join("atlassian")
    }
}

/// Persistent draft store backed by `drafts.json` (`0600` permissions on Unix).
/// Keys follow conventions like `"comment:PROJ-123"`, `"issue:PROJ"`, `"page:ENG:12345"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DraftStore {
    pub drafts: HashMap<String, String>,
}

impl DraftStore {
    pub fn load() -> Self {
        Self::load_from(&drafts_file_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read drafts file: {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let map: HashMap<String, String> = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse drafts file: {}", path.display()))?;
        Ok(Self { drafts: map })
    }

    pub fn save(&self) -> Result<()> {
        #[cfg(test)]
        if std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.save_to(&drafts_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.drafts)
            .context("Failed to serialize drafts")?;
        write_atomic_0600(path, &bytes)
            .with_context(|| format!("Failed to save drafts file: {}", path.display()))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.drafts.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if value.trim().is_empty() {
            self.drafts.remove(&key);
        } else {
            self.drafts.insert(key, value);
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.drafts.remove(key)
    }

    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }
}

pub fn normalize_base_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// Normalizes a URL or host string for case-insensitive site matching (strips scheme and trailing slashes).
pub(crate) fn normalize_url_for_match(raw: &str) -> String {
    let trimmed = normalize_base_url(raw).to_ascii_lowercase();
    trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(&trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// Derives a clean, stable lowercase slug identifier from a site URL or display name.
///
/// Examples:
/// - `"https://acme-corp.atlassian.net"` -> `"acme-corp"`
/// - `"https://jira.company.com/"` -> `"jira-company-com"`
/// - `"Acme Prod (EU)"` -> `"acme-prod-eu"`
pub fn slugify_site_id(url_or_name: &str) -> String {
    let trimmed = url_or_name.trim();
    if trimmed.is_empty() {
        return "site".to_string();
    }

    let candidate = if trimmed.contains("://") {
        url::Url::parse(trimmed)
            .ok()
            .and_then(|u| {
                let host = u.host_str()?.to_ascii_lowercase();
                if let Some(sub) = host.strip_suffix(".atlassian.net") {
                    if !sub.is_empty() {
                        return Some(sub.to_string());
                    }
                }
                if let Some(sub) = host.strip_suffix(".jira.com") {
                    if !sub.is_empty() {
                        return Some(sub.to_string());
                    }
                }
                if let Some(port) = u.port() {
                    Some(format!("{host}-{port}"))
                } else {
                    Some(host)
                }
            })
            .unwrap_or_else(|| trimmed.to_string())
    } else if !trimmed.contains(char::is_whitespace) && trimmed.contains('.') {
        let lower = trimmed.to_ascii_lowercase();
        if let Some(sub) = lower.strip_suffix(".atlassian.net") {
            sub.to_string()
        } else if let Some(sub) = lower.strip_suffix(".jira.com") {
            sub.to_string()
        } else {
            lower
        }
    } else {
        trimmed.to_string()
    };

    let mut out = String::with_capacity(candidate.len());
    let mut prev_dash = false;
    for ch in candidate.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "site".to_string()
    } else {
        out
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("ATLASSIAN_CONFIG_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("atlassian-cli")
}

pub fn data_dir(profile: &str) -> PathBuf {
    if let Ok(override_dir) = std::env::var("ATLASSIAN_DATA_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed).join(profile);
        }
    }
    if let Ok(override_dir) = std::env::var("ATLASSIAN_CONFIG_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed).join(profile);
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(config_dir)
        .join("atlassian-cli")
        .join(profile)
}

/// Canonicalizes the deepest existing ancestor of `path` and re-appends the remaining
/// non-existent tail components, falling back to `std::path::absolute`.
pub fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(canon) = fs::canonicalize(&abs) {
        return canon;
    }

    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        let Some(comp) = cur.components().next_back() else {
            break;
        };
        tail.push(comp.as_os_str().to_os_string());
        cur = parent;
        if cur.as_os_str().is_empty() {
            break;
        }
        if let Ok(mut canon) = fs::canonicalize(cur) {
            for part in tail.into_iter().rev() {
                canon.push(part);
            }
            return canon;
        }
    }
    abs
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

pub fn drafts_path() -> PathBuf {
    drafts_file_path()
}

pub fn cache_db_path(profile: &str) -> PathBuf {
    data_dir(profile).join("cache.db")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` via a unique temp file. Files containing
/// `"credentials"` or `"drafts"` default to `0o600` permissions on Unix.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let lower = n.to_ascii_lowercase();
            lower.contains("credentials") || lower.contains("drafts")
        })
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    let target_path: PathBuf = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if mode_0600 {
                anyhow::bail!(
                    "Refusing to write secret file through symlink: {}",
                    path.display()
                );
            }
            let resolved = fs::canonicalize(path).with_context(|| {
                format!(
                    "Refusing to write through dangling symlink: {}",
                    path.display()
                )
            })?;
            if resolved.is_dir() {
                anyhow::bail!(
                    "Refusing to overwrite directory through symlink: {} -> {}",
                    path.display(),
                    resolved.display()
                );
            }
            resolved
        }
        _ => path.to_path_buf(),
    };

    ensure_parent(&target_path)?;
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, &target_path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, &target_path, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err)
            .with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle_site(id: &str, url: &str, deployment: DeploymentKind) -> SiteProfile {
        SiteProfile {
            id: id.into(),
            name: id.into(),
            site_url: url.into(),
            deployment,
            account_id: id.into(),
            ..SiteProfile::default()
        }
    }

    fn two_site_config() -> AppConfig {
        AppConfig {
            sites: vec![
                lifecycle_site("acme", "https://acme.atlassian.net", DeploymentKind::Cloud),
                lifecycle_site("corp", "https://jira.corp.example", DeploymentKind::DataCenter),
            ],
            default_site: Some("acme".into()),
            site_url: "https://acme.atlassian.net".into(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn set_default_site_clears_stale_active_site() {
        let mut cfg = two_site_config();
        cfg.active_site = Some("acme".into());
        let cleared = cfg.set_default_site("corp").unwrap();
        assert_eq!(cleared.as_deref(), Some("acme"));
        assert!(cfg.active_site.is_none());
        assert_eq!(cfg.resolved_site_profile().unwrap().id, "corp");
    }

    #[test]
    fn set_default_site_syncs_legacy_url_fields() {
        let mut cfg = two_site_config();
        cfg.set_default_site("CORP").unwrap();
        assert_eq!(cfg.default_site.as_deref(), Some("corp"));
        assert_eq!(cfg.site_url, "https://jira.corp.example");
        assert_eq!(cfg.deployment, DeploymentKind::DataCenter);
        assert!(cfg.set_default_site("nope").is_err());
    }

    #[test]
    fn set_default_site_keeps_matching_active_site() {
        let mut cfg = two_site_config();
        cfg.active_site = Some("corp".into());
        assert_eq!(cfg.set_default_site("corp").unwrap(), None);
        assert_eq!(cfg.active_site.as_deref(), Some("corp"));
    }

    #[test]
    fn oauth_login_keeps_existing_legacy_dc_site() {
        // A legacy single-site Data Center configuration, then a discovered Cloud site.
        let mut cfg = AppConfig {
            site_url: "https://jira.corp.example".into(),
            deployment: DeploymentKind::DataCenter,
            ..AppConfig::default()
        };
        cfg.materialize_legacy_site();
        cfg.upsert_site(lifecycle_site("acme", "https://acme.atlassian.net", DeploymentKind::Cloud));
        let urls: Vec<String> = cfg.effective_sites().into_iter().map(|s| s.site_url).collect();
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(urls.contains(&"https://jira.corp.example".to_string()));
        assert!(urls.contains(&"https://acme.atlassian.net".to_string()));

        // Without the migration the legacy site would be shadowed by "sites".
        let mut plain = AppConfig {
            site_url: "https://jira.corp.example".into(),
            ..AppConfig::default()
        };
        plain.upsert_site(lifecycle_site("acme", "https://acme.atlassian.net", DeploymentKind::Cloud));
        assert_eq!(plain.effective_sites().len(), 1);
    }

    #[test]
    fn sites_remove_uses_canonical_id_for_cache_delete() {
        let mut cfg = two_site_config();
        let removed = cfg.remove_effective_site("ACME").expect("found case-insensitively");
        assert_eq!(removed.id, "acme", "the cache is keyed by the canonical id");
        assert!(cfg.default_site.is_none());
        // The legacy mirror no longer names the removed site.
        assert_eq!(cfg.site_url, "https://jira.corp.example");
        assert!(cfg.remove_effective_site("acme").is_none());
    }

    #[test]
    fn removing_last_site_clears_legacy_urls() {
        let mut cfg = AppConfig {
            sites: vec![lifecycle_site("corp", "https://jira.corp.example", DeploymentKind::DataCenter)],
            site_url: "https://jira.corp.example".into(),
            confluence_url: "https://wiki.corp.example".into(),
            default_site: Some("corp".into()),
            active_site: Some("corp".into()),
            ..AppConfig::default()
        };
        cfg.remove_effective_site("corp").unwrap();
        assert!(cfg.site_url.is_empty() && cfg.confluence_url.is_empty());
        assert!(cfg.effective_sites().is_empty(), "no implicit 'default' site comes back");
        assert!(cfg.default_site.is_none() && cfg.active_site.is_none());
    }


    #[test]
    fn density_mode_toggle_and_labels() {
        assert_eq!(DensityMode::default(), DensityMode::Cozy);
        assert_eq!(DensityMode::Cozy.label(), "Cozy");
        assert_eq!(DensityMode::Compact.label(), "Compact");
        assert_eq!(DensityMode::Cozy.toggle(), DensityMode::Compact);
        assert_eq!(DensityMode::Compact.toggle(), DensityMode::Cozy);
    }

    #[test]
    fn theme_preset_cycle_and_labels() {
        let mut p = ThemePreset::default();
        assert_eq!(p, ThemePreset::Auto);
        let expected = [
            (ThemePreset::Auto, "Auto"),
            (ThemePreset::Dark, "Midnight Dark"),
            (ThemePreset::Light, "Daylight Light"),
            (ThemePreset::Ansi256, "ANSI 256"),
            (ThemePreset::Ansi16, "ANSI 16"),
            (ThemePreset::NoColor, "No Color"),
        ];
        for (preset, label) in expected {
            assert_eq!(p, preset);
            assert_eq!(p.label(), label);
            p = p.next();
        }
        assert_eq!(p, ThemePreset::Auto);
    }

    #[test]
    fn image_protocol_preference_cycle_parse_and_roundtrip() {
        let mut proto = ImageProtocolPreference::default();
        assert_eq!(proto, ImageProtocolPreference::Auto);
        let expected = [
            (ImageProtocolPreference::Auto, "Auto", "auto"),
            (ImageProtocolPreference::Kitty, "Kitty", "kitty"),
            (ImageProtocolPreference::ITerm2, "iTerm2", "iterm2"),
            (ImageProtocolPreference::Sixel, "Sixel", "sixel"),
            (ImageProtocolPreference::Halfblock, "Halfblock", "halfblock"),
        ];
        for (variant, label, as_s) in expected {
            assert_eq!(proto, variant);
            assert_eq!(proto.label(), label);
            assert_eq!(proto.as_str(), as_s);
            assert_eq!(ImageProtocolPreference::parse_str(as_s), Some(variant));
            proto = proto.next();
        }
        assert_eq!(proto, ImageProtocolPreference::Auto);
        assert_eq!(
            ImageProtocolPreference::parse_str("iterm"),
            Some(ImageProtocolPreference::ITerm2)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("halfblocks"),
            Some(ImageProtocolPreference::Halfblock)
        );
    }

    #[test]
    fn app_config_toml_roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            theme: ThemePreset::Light,
            density: DensityMode::Compact,
            image_protocol: ImageProtocolPreference::Sixel,
            inline_images: false,
            notify: false,
            show_nav_rail: false,
            download_dir: Some(PathBuf::from("/tmp/dl")),
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();

        let reloaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(reloaded.theme, ThemePreset::Light);
        assert_eq!(reloaded.density, DensityMode::Compact);
        assert_eq!(reloaded.image_protocol, ImageProtocolPreference::Sixel);
        assert!(!reloaded.inline_images);
        assert!(!reloaded.notify);
        assert!(!reloaded.show_nav_rail);
        assert_eq!(reloaded.download_dir, Some(PathBuf::from("/tmp/dl")));

        // Partial legacy TOML without new fields falls back to defaults cleanly
        let legacy_path = dir.path().join("legacy.toml");
        fs::write(&legacy_path, "site_url = \"https://example.atlassian.net\"\n").unwrap();
        let legacy = AppConfig::load_from(&legacy_path).unwrap();
        assert_eq!(legacy.theme, ThemePreset::Auto);
        assert_eq!(legacy.density, DensityMode::Cozy);
        assert_eq!(legacy.image_protocol, ImageProtocolPreference::Auto);
        assert!(legacy.inline_images);
        assert!(legacy.notify);
        assert!(legacy.show_nav_rail);

        cfg.image_protocol = ImageProtocolPreference::ITerm2;
        cfg.save_to(&path).unwrap();
        assert_eq!(
            AppConfig::load_from(&path).unwrap().image_protocol,
            ImageProtocolPreference::ITerm2
        );
    }

    #[test]
    fn draft_store_crud_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drafts.json");

        let mut store = DraftStore::load_from(&path).unwrap();
        assert!(store.is_empty());

        store.set("comment:ATL-1", "Working on the fix.");
        store.set("page:ENG:new", "# Architecture Notes");
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("comment:ATL-1"), Some("Working on the fix."));

        // Blank values remove the draft
        store.set("page:ENG:new", "   ");
        assert_eq!(store.get("page:ENG:new"), None);
        assert_eq!(store.len(), 1);

        store.save_to(&path).unwrap();
        let loaded = DraftStore::load_from(&path).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.get("comment:ATL-1"), Some("Working on the fix."));

        let mut loaded = loaded;
        assert_eq!(
            loaded.remove("comment:ATL-1"),
            Some("Working on the fix.".to_string())
        );
        assert!(loaded.is_empty());
    }

    #[test]
    fn slugify_site_id_handles_urls_and_names() {
        assert_eq!(slugify_site_id("https://acme.atlassian.net"), "acme");
        assert_eq!(slugify_site_id("https://My-Team.atlassian.net/"), "my-team");
        assert_eq!(
            slugify_site_id("https://jira.company.com:8443/"),
            "jira-company-com-8443"
        );
        assert_eq!(slugify_site_id("Acme Corp (Prod)"), "acme-corp-prod");
        assert_eq!(slugify_site_id("side-project.atlassian.net"), "side-project");
        assert_eq!(slugify_site_id("   "), "site");
    }

    #[test]
    fn app_config_legacy_migration_and_multisite_helpers() {
        // Legacy single-site config transparently synthesizes "default" site
        let legacy = AppConfig {
            site_url: "https://legacy.atlassian.net/".into(),
            deployment: DeploymentKind::Cloud,
            profile: "work".into(),
            ..Default::default()
        };
        let eff = legacy.effective_sites();
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].id, "default");
        assert_eq!(eff[0].site_url, "https://legacy.atlassian.net");
        assert_eq!(
            eff[0].effective_confluence_root(),
            "https://legacy.atlassian.net/wiki"
        );
        assert_eq!(eff[0].account_id, "work");
        assert!(eff[0].enabled);
        assert_eq!(
            legacy
                .find_effective_site("https://legacy.atlassian.net")
                .map(|s| s.id),
            Some("default".into())
        );

        // Multi-site CRUD, find_site, enabled_sites, and TOML roundtrip
        let mut cfg = AppConfig::default();
        assert!(cfg.effective_sites().is_empty());

        cfg.upsert_site(SiteProfile {
            id: "acme".into(),
            name: "Acme Cloud".into(),
            site_url: "https://acme.atlassian.net/".into(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: Some("cloud-aaa".into()),
            account_id: "oauth-main".into(),
            enabled: true,
            scopes: vec!["read:jira-work".into()],
            confluence_account_id: None,
        });
        cfg.upsert_site(SiteProfile {
            id: "corp-dc".into(),
            name: "Corp On-Prem".into(),
            site_url: "https://jira.corp.local/".into(),
            confluence_url: "https://wiki.corp.local/".into(),
            deployment: DeploymentKind::DataCenter,
            cloud_id: None,
            account_id: "dc-pat".into(),
            enabled: false,
            scopes: Vec::new(),
            confluence_account_id: None,
        });
        cfg.active_site = Some("acme".into());
        cfg.default_site = Some("acme".into());

        // Match by id, name, full URL, scheme-less URL, and cloud_id
        assert_eq!(cfg.find_site("ACME").map(|s| s.id.as_str()), Some("acme"));
        assert_eq!(
            cfg.find_site("Corp On-Prem").map(|s| s.id.as_str()),
            Some("corp-dc")
        );
        assert_eq!(
            cfg.find_site("https://acme.atlassian.net/")
                .map(|s| s.id.as_str()),
            Some("acme")
        );
        assert_eq!(
            cfg.find_site("jira.corp.local").map(|s| s.id.as_str()),
            Some("corp-dc")
        );
        assert_eq!(
            cfg.find_site("cloud-aaa").map(|s| s.id.as_str()),
            Some("acme")
        );
        assert!(cfg.find_site("missing").is_none());

        assert_eq!(cfg.enabled_sites().len(), 1);
        assert_eq!(cfg.enabled_sites()[0].id, "acme");

        // Upsert by matching cloud_id updates in place without duplicating
        cfg.upsert_site(SiteProfile {
            id: "acme-renamed".into(),
            name: "Acme Updated".into(),
            site_url: "https://acme.atlassian.net".into(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: Some("cloud-aaa".into()),
            account_id: "oauth-main".into(),
            enabled: true,
            scopes: vec!["read:jira-work".into(), "write:jira-work".into()],
            confluence_account_id: None,
        });
        assert_eq!(cfg.sites.len(), 2);
        assert_eq!(cfg.find_site("acme-renamed").unwrap().name, "Acme Updated");

        // TOML persistence roundtrip
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multisite.toml");
        cfg.save_to(&path).unwrap();
        let reloaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(reloaded.sites.len(), 2);
        assert_eq!(reloaded.sites[0].cloud_id.as_deref(), Some("cloud-aaa"));
        assert_eq!(reloaded.sites[1].confluence_url, "https://wiki.corp.local");

        // Removing active/default site clears references
        cfg.active_site = Some("acme-renamed".into());
        cfg.default_site = Some("acme-renamed".into());
        assert!(cfg.remove_site("acme-renamed"));
        assert!(!cfg.remove_site("acme-renamed"));
        assert_eq!(cfg.active_site, None);
        assert_eq!(cfg.default_site, None);
        assert_eq!(cfg.sites.len(), 1);
    }

    #[test]
    fn effective_oauth_client_config_prefers_env_over_file() {
        static OAUTH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = OAUTH_ENV_LOCK.lock().unwrap();

        let prev_id = std::env::var("ATLASSIAN_OAUTH_CLIENT_ID").ok();
        let prev_secret = std::env::var("ATLASSIAN_OAUTH_CLIENT_SECRET").ok();
        std::env::remove_var("ATLASSIAN_OAUTH_CLIENT_ID");
        std::env::remove_var("ATLASSIAN_OAUTH_CLIENT_SECRET");

        let cfg = AppConfig {
            oauth_client_id: Some("file-cid".into()),
            oauth_client_secret: Some("file-csec".into()),
            ..Default::default()
        };
        assert_eq!(cfg.effective_oauth_client_id().as_deref(), Some("file-cid"));
        assert_eq!(
            cfg.effective_oauth_client_secret().as_deref(),
            Some("file-csec")
        );

        std::env::set_var("ATLASSIAN_OAUTH_CLIENT_ID", "env-cid");
        std::env::set_var("ATLASSIAN_OAUTH_CLIENT_SECRET", "env-csec");
        assert_eq!(cfg.effective_oauth_client_id().as_deref(), Some("env-cid"));
        assert_eq!(
            cfg.effective_oauth_client_secret().as_deref(),
            Some("env-csec")
        );

        match prev_id {
            Some(v) => std::env::set_var("ATLASSIAN_OAUTH_CLIENT_ID", v),
            None => std::env::remove_var("ATLASSIAN_OAUTH_CLIENT_ID"),
        }
        match prev_secret {
            Some(v) => std::env::set_var("ATLASSIAN_OAUTH_CLIENT_SECRET", v),
            None => std::env::remove_var("ATLASSIAN_OAUTH_CLIENT_SECRET"),
        }
    }

    #[test]
    fn site_profile_without_confluence_account_roundtrips_unchanged() {
        // A 0.8.0-style config: no confluence_account_id anywhere.
        let legacy = r#"
site_url = "https://jira.corp.example"
confluence_url = "https://wiki.corp.example"
deployment = "datacenter"
sync_interval_secs = 45
github_repo = "BiteFoundry/atlassian"
profile = "default"

[[sites]]
id = "corp"
name = "Corp DC"
site_url = "https://jira.corp.example"
confluence_url = "https://wiki.corp.example"
deployment = "datacenter"
account_id = "corp"
enabled = true
scopes = []
"#;
        let cfg: AppConfig = toml::from_str(legacy).expect("0.8.0 config loads unchanged");
        assert_eq!(cfg.sites.len(), 1);
        assert_eq!(cfg.sites[0].confluence_account_id, None);

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !serialized.contains("confluence_account_id"),
            "an unset Confluence account must not be written: {serialized}"
        );
        let reparsed: AppConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.sites, cfg.sites);

        // When set, it is written and read back.
        let mut split = cfg.clone();
        split.sites[0].confluence_account_id = Some("corp-confluence".into());
        let serialized = toml::to_string_pretty(&split).unwrap();
        assert!(serialized.contains("confluence_account_id = \"corp-confluence\""));
        let reparsed: AppConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.sites[0].confluence_account_id.as_deref(),
            Some("corp-confluence")
        );
    }

    #[test]
    fn canonicalize_existing_ancestor_resolves_tail_and_normalizes_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let real_sub = dir.path().join("real");
        fs::create_dir(&real_sub).unwrap();

        let existing = canonicalize_existing_ancestor(&real_sub);
        assert_eq!(existing, fs::canonicalize(&real_sub).unwrap());

        let missing_tail = real_sub.join("deep").join("nonexistent").join("profile");
        let resolved = canonicalize_existing_ancestor(&missing_tail);
        assert_eq!(
            resolved,
            fs::canonicalize(&real_sub)
                .unwrap()
                .join("deep")
                .join("nonexistent")
                .join("profile")
        );

        let via_dotdot = real_sub.join("..").join("real").join("deep").join("nonexistent");
        assert_eq!(
            canonicalize_existing_ancestor(&via_dotdot),
            fs::canonicalize(&real_sub)
                .unwrap()
                .join("deep")
                .join("nonexistent")
        );

        let link = dir.path().join("link_to_real");
        let linked = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_sub, &link).unwrap();
                true
            }
            #[cfg(windows)]
            {
                match std::os::windows::fs::symlink_dir(&real_sub, &link) {
                    Ok(()) => true,
                    Err(e) if e.raw_os_error() == Some(1314) => false,
                    Err(e) => panic!("unexpected symlink_dir error: {e}"),
                }
            }
        };
        if linked {
            let via_link = link.join("future").join("leaf");
            let via_real = real_sub.join("future").join("leaf");
            assert_eq!(
                canonicalize_existing_ancestor(&via_link),
                canonicalize_existing_ancestor(&via_real)
            );
        }
    }

    #[test]
    fn env_dir_overrides_expand_tilde() {
        static DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = DIR_ENV_LOCK.lock().unwrap();

        let prev_cfg = std::env::var("ATLASSIAN_CONFIG_DIR").ok();
        let prev_data = std::env::var("ATLASSIAN_DATA_DIR").ok();

        if let Some(home) = dirs::home_dir() {
            std::env::set_var("ATLASSIAN_CONFIG_DIR", "~/.atlassian-custom-cfg");
            std::env::remove_var("ATLASSIAN_DATA_DIR");
            assert_eq!(config_dir(), home.join(".atlassian-custom-cfg"));
            assert_eq!(
                data_dir("work"),
                home.join(".atlassian-custom-cfg").join("work")
            );

            std::env::set_var("ATLASSIAN_DATA_DIR", "~/custom-atlassian-data");
            assert_eq!(
                data_dir("work"),
                home.join("custom-atlassian-data").join("work")
            );
        }

        match prev_cfg {
            Some(v) => std::env::set_var("ATLASSIAN_CONFIG_DIR", v),
            None => std::env::remove_var("ATLASSIAN_CONFIG_DIR"),
        }
        match prev_data {
            Some(v) => std::env::set_var("ATLASSIAN_DATA_DIR", v),
            None => std::env::remove_var("ATLASSIAN_DATA_DIR"),
        }
    }

    #[test]
    fn write_atomic_symlink_policy_for_config_and_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let cfg_home = dir.path().join("config");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&cfg_home).unwrap();

        let real_cfg = dotfiles.join("config.toml");
        fs::write(&real_cfg, b"old = true\n").unwrap();
        let linked_cfg = cfg_home.join("config.toml");

        let real_creds = dotfiles.join("credentials.json");
        fs::write(&real_creds, b"{}\n").unwrap();
        let linked_creds = cfg_home.join("credentials.json");

        let dangling_cfg = cfg_home.join("dangling.toml");
        let missing_target = dotfiles.join("missing.toml");

        let symlinks_supported = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_cfg, &linked_cfg).unwrap();
                std::os::unix::fs::symlink(&real_creds, &linked_creds).unwrap();
                std::os::unix::fs::symlink(&missing_target, &dangling_cfg).unwrap();
                true
            }
            #[cfg(windows)]
            {
                match std::os::windows::fs::symlink_file(&real_cfg, &linked_cfg) {
                    Ok(()) => {
                        std::os::windows::fs::symlink_file(&real_creds, &linked_creds).unwrap();
                        std::os::windows::fs::symlink_file(&missing_target, &dangling_cfg)
                            .unwrap();
                        true
                    }
                    Err(e) if e.raw_os_error() == Some(1314) => false,
                    Err(e) => panic!("unexpected symlink_file error: {e}"),
                }
            }
        };

        if !symlinks_supported {
            return;
        }

        // Non-secret config through valid symlink updates real target and preserves the symlink.
        write_atomic(&linked_cfg, b"updated = true\n").unwrap();
        assert!(fs::symlink_metadata(&linked_cfg)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real_cfg).unwrap(), "updated = true\n");

        // Dangling symlink is rejected cleanly for non-secret atomic writes.
        let dangling_err = write_atomic(&dangling_cfg, b"nope = true\n").unwrap_err();
        assert!(
            format!("{dangling_err:#}").contains("dangling symlink"),
            "unexpected error: {dangling_err:#}"
        );

        // Secret files (0600) refuse writing through any symlink.
        let secret_err = write_atomic_0600(&linked_creds, b"{\"secret\":1}").unwrap_err();
        assert!(
            format!("{secret_err:#}").contains("Refusing to write secret file through symlink"),
            "unexpected error: {secret_err:#}"
        );
        assert_eq!(fs::read_to_string(&real_creds).unwrap(), "{}\n");

        // `write_atomic` also classifies `credentials.json` as 0600 secret and refuses symlink.
        let auto_secret_err = write_atomic(&linked_creds, b"{\"secret\":2}").unwrap_err();
        assert!(
            format!("{auto_secret_err:#}").contains("Refusing to write secret file through symlink"),
            "unexpected error: {auto_secret_err:#}"
        );
    }
}
