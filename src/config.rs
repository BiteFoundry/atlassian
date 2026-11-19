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

pub const DEFAULT_GITHUB_REPO: &str = "shinick-dev/atlassian";
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
    /// GitHub repository (`owner/repo`) for `jira update` / `confluence update`.
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
        Ok(cfg)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str::<AppConfig>(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, body.as_bytes())
            .with_context(|| format!("Failed to save config file: {}", path.display()))
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
    ensure_parent(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
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

    if let Err(rename_err) = fs::rename(&tmp, path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, path, parent, name, pid, seq, rename_err)
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

fn expand_tilde(input: &str) -> PathBuf {
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
}
