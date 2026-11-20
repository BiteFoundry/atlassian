//! Application & UI state, Kanban grouping, Confluence page tree, cross-app history, and async tasks.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use regex::Regex;
use tokio::sync::mpsc;

use crate::{
    adf,
    atlassian::{AtlassianClient, SiteClientPool},
    auth::{
        fetch_accessible_resources, run_oauth_loopback_login, AuthManager, OAuthLoopbackOptions,
        StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{
        AppConfig, DensityMode, DeploymentKind, DraftStore,
        ImageProtocolPreference as ConfigImageProtocol, Persona, SiteProfile,
    },
    img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps},
    models::{
        AttachmentItem, BoardColumn, BoardSummary, ConfluenceComment, CoreEvent, CrossRefTarget,
        DownloadedFile, IssueComment, IssueDetail, IssueStatus, IssueSummary, IssueTransition,
        IssueUpdate, OutgoingIssue, OutgoingPage, PageDetail, PageSummary, PageUpdate,
        ProjectSummary, SavedFilter, SiteScope, SpaceSummary, SprintSummary, StatusCategory,
        UnifiedSearchHit, UpdateCheckResult, UserSummary,
    },
    storage::{CacheCounts, SiteRecord, Storage, DEFAULT_SITE_ID},
    sync::{self, MultiSiteSyncOutcome, SiteSyncTarget, SyncController},
    ui::{
        directory_view::ConnectedSiteRow,
        hitmap::HitMap,
        keymap::Keymap,
        notification,
        overlays::{
            assignee::AssigneeOverlay,
            comment::{CommentOverlay, CommentTarget},
            html_preview::HtmlPreviewOverlay,
            issue_form::IssueFormOverlay,
            page_form::PageFormOverlay,
            path_input::{PathInputOverlay, UploadTarget},
            site_switcher::{SiteSwitcherCommand, SiteSwitcherEntry},
            transition::TransitionOverlay,
            Overlay,
        },
        text_edit::TextBuffer,
        theme::Theme,
        toast::{TaskBoard, ToastQueue},
    },
    updater,
};

pub const TAB_LABELS: [&str; 6] = [
    "1:Board",
    "2:Issues",
    "3:My Work",
    "4:Wiki",
    "5:Search",
    "6:Directory",
];

pub fn config_protocol_to_img_pref(
    pref: ConfigImageProtocol,
) -> img_preview::ImageProtocolPreference {
    match pref {
        ConfigImageProtocol::Auto => img_preview::ImageProtocolPreference::Auto,
        ConfigImageProtocol::Kitty => img_preview::ImageProtocolPreference::Kitty,
        ConfigImageProtocol::ITerm2 => img_preview::ImageProtocolPreference::Iterm2,
        ConfigImageProtocol::Sixel => img_preview::ImageProtocolPreference::Sixel,
        ConfigImageProtocol::Halfblock => img_preview::ImageProtocolPreference::Halfblock,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTab {
    Board = 0,
    Issues = 1,
    MyWork = 2,
    Confluence = 3,
    Search = 4,
    Directory = 5,
}

impl WorkspaceTab {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => Self::Board,
            1 => Self::Issues,
            2 => Self::MyWork,
            3 => Self::Confluence,
            4 => Self::Search,
            5 => Self::Directory,
            _ => Self::Board,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_persona(persona: Persona) -> Self {
        match persona {
            Persona::Jira => Self::Board,
            Persona::Confluence => Self::Confluence,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFocus {
    Nav,
    Main,
    Detail,
    JqlBar,
    SearchBar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalEditorTarget {
    IssueFormDescription,
    PageFormBody,
    CommentBody,
}

#[derive(Debug, Clone)]
pub struct KanbanColumn {
    pub title: String,
    pub category: StatusCategory,
    pub issues: Vec<IssueSummary>,
}

#[derive(Debug, Clone)]
pub struct TreeRow {
    pub page: PageSummary,
    pub depth: usize,
    pub expanded: bool,
    pub has_children: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BoardUiState {
    pub selected_board: usize,
    pub selected_sprint: usize,
    /// `0` = boards list, `1` = sprints list inside left nav.
    pub nav_section: usize,
    pub selected_col: usize,
    pub selected_row: usize,
    pub col_scroll: usize,
    pub drawer_open: bool,
    pub detail_scroll: u16,
}

#[derive(Debug, Clone)]
pub struct IssueUiState {
    pub jql: TextBuffer,
    pub local_filter: TextBuffer,
    pub local_filter_active: bool,
    pub selected_filter: usize,
    pub selected_issue: usize,
    /// `0` = saved filters, `1` = issue list inside left nav.
    pub nav_section: usize,
    pub detail_scroll: u16,
}

impl Default for IssueUiState {
    fn default() -> Self {
        Self {
            jql: TextBuffer::from_text("ORDER BY updated DESC"),
            local_filter: TextBuffer::new(),
            local_filter_active: false,
            selected_filter: 0,
            selected_issue: 0,
            nav_section: 1,
            detail_scroll: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MyWorkUiState {
    /// `0` = My Open Issues, `1` = Recent Confluence Pages.
    pub active_column: usize,
    pub selected_issue: usize,
    pub selected_page: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ConfluenceUiState {
    pub selected_space: usize,
    pub selected_tree_row: usize,
    /// `0` = Space selector, `1` = Page tree inside left nav.
    pub nav_section: usize,
    pub toc_open: bool,
    pub selected_toc: usize,
    pub reader_scroll: u16,
}

#[derive(Debug, Clone, Default)]
pub struct SearchUiState {
    pub query: TextBuffer,
    pub selected_hit: usize,
    pub is_remote: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryUiState {
    /// `0` = Projects, `1` = Spaces.
    pub section: usize,
    pub selected_project: usize,
    pub selected_space: usize,
    pub last_update: Option<UpdateCheckResult>,
}

#[derive(Debug)]
pub enum UiMsg {
    MeLoaded(Result<UserSummary, String>),
    BootstrapFinished(Result<String, String>),
    BoardsLoaded(Result<Vec<BoardSummary>, String>),
    BoardDataLoaded {
        board_id: i64,
        columns: Vec<BoardColumn>,
        sprints: Vec<SprintSummary>,
        issues: Vec<IssueSummary>,
    },
    BoardLoadFailed(String),
    IssuesLoaded {
        issues: Vec<IssueSummary>,
        label: String,
    },
    IssuesLoadFailed(String),
    SavedFiltersLoaded(Vec<SavedFilter>),
    IssueDetailLoaded {
        key: String,
        detail: Result<IssueDetail, String>,
    },
    IssueMutated {
        detail: IssueDetail,
        toast: String,
    },
    IssueTransitioned {
        key: String,
        to_status: IssueStatus,
        toast: String,
    },
    IssueTransitionFailed {
        key: String,
        previous_status: Option<IssueStatus>,
        error: String,
    },
    IssueAssignFailed {
        key: String,
        previous_assignee: Option<Option<UserSummary>>,
        task_label: String,
        error: String,
    },
    IssueActionFailed(String),
    TransitionsLoaded {
        issue_key: String,
        transitions: Result<Vec<IssueTransition>, String>,
    },
    AssignableUsersLoaded {
        query: String,
        users: Result<Vec<UserSummary>, String>,
    },
    CommentAdded {
        issue_key: Option<String>,
        page_id: Option<String>,
        optimistic_id: Option<String>,
        task_label: String,
        result: Result<(), String>,
    },
    SpacesLoaded(Result<Vec<SpaceSummary>, String>),
    RootPagesLoaded {
        space_key: String,
        pages: Result<Vec<PageSummary>, String>,
    },
    ChildPagesLoaded {
        parent_id: String,
        children: Result<Vec<PageSummary>, String>,
    },
    PageDetailLoaded {
        page_id: String,
        detail: Result<PageDetail, String>,
    },
    PageMutated {
        detail: PageDetail,
        toast: String,
    },
    PageActionFailed(String),
    MyWorkLoaded {
        issues: Vec<IssueSummary>,
        issue_site_labels: Vec<Option<String>>,
        pages: Vec<PageSummary>,
        page_site_labels: Vec<Option<String>>,
    },
    RemoteSearchLoaded {
        query: String,
        hits: Result<(Vec<UnifiedSearchHit>, Vec<String>, Vec<Option<String>>), String>,
    },
    CloudSitesDiscovered(Result<Vec<SiteProfile>, String>),
    MultiSiteSyncFinished(MultiSiteSyncOutcome),
    ImageLoaded {
        cache_key: String,
        rendered: Result<RenderedTerminalImage, String>,
    },
    HtmlAttachmentLoaded {
        title: String,
        source_url: String,
        result: Result<String, String>,
    },
    PreviewImageSniffedAsHtml {
        title: String,
        source_url: String,
        html: String,
    },
    HtmlServed(Result<String, String>),
    AttachmentDownloaded(Result<DownloadedFile, String>),
    AttachmentUploaded {
        issue_key: Option<String>,
        page_id: Option<String>,
        result: Result<Vec<AttachmentItem>, String>,
    },
    UpdateChecked(Result<UpdateCheckResult, String>),
}

pub struct UiState {
    pub active_tab: WorkspaceTab,
    pub focus: PaneFocus,
    pub theme: Theme,
    pub density: DensityMode,
    pub notify: bool,
    pub graphics_caps: TerminalGraphicsCaps,
    pub keymap: Keymap,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub overlays: Vec<Overlay>,
    pub term_width: u16,
    pub term_height: u16,
    pub show_nav_rail: bool,
    pub auto_detail_drawer: bool,
    pub inline_images: bool,
    pub show_hrefs: bool,
    pub editor_request: Option<(ExternalEditorTarget, String)>,
    pub board_ui: BoardUiState,
    pub issue_ui: IssueUiState,
    pub my_work_ui: MyWorkUiState,
    pub confluence_ui: ConfluenceUiState,
    pub search_ui: SearchUiState,
    pub directory_ui: DirectoryUiState,
    /// Collapsed sidebar site group keys (`"{tab}:{site_id}"`) in multi-site All Sites mode.
    pub collapsed_site_groups: HashSet<String>,
    /// Per-frame mapping from `HitTarget::SiteGroupHeader(idx)` to group collapse key.
    pub site_group_keys: Vec<String>,
}

impl UiState {
    pub fn from_config_with_persona(config: &AppConfig, persona: Persona) -> Self {
        let active_tab = WorkspaceTab::from_persona(persona);
        let graphics_caps =
            img_preview::detect_graphics_caps(config_protocol_to_img_pref(config.image_protocol));
        Self {
            active_tab,
            focus: PaneFocus::Main,
            theme: Theme::detect(config.theme),
            density: config.density,
            notify: config.notify,
            graphics_caps,
            keymap: Keymap,
            hitmap: HitMap::new(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            overlays: Vec::new(),
            term_width: 140,
            term_height: 40,
            show_nav_rail: config.show_nav_rail,
            auto_detail_drawer: config.auto_detail_drawer,
            inline_images: config.inline_images,
            show_hrefs: false,
            editor_request: None,
            board_ui: BoardUiState {
                drawer_open: config.auto_detail_drawer,
                ..Default::default()
            },
            issue_ui: IssueUiState::default(),
            my_work_ui: MyWorkUiState::default(),
            confluence_ui: ConfluenceUiState {
                toc_open: false,
                ..Default::default()
            },
            search_ui: SearchUiState::default(),
            directory_ui: DirectoryUiState::default(),
            collapsed_site_groups: HashSet::new(),
            site_group_keys: Vec::new(),
        }
    }

    pub fn has_overlay(&self) -> bool {
        !self.overlays.is_empty()
    }

    pub fn accepts_text_input(&self) -> bool {
        if let Some(top) = self.overlays.last() {
            return top.accepts_text();
        }
        matches!(self.focus, PaneFocus::JqlBar | PaneFocus::SearchBar)
            || (self.active_tab == WorkspaceTab::Issues && self.issue_ui.local_filter_active)
    }

    pub fn push_overlay(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    pub fn pop_overlay(&mut self) {
        self.overlays.pop();
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.success(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.error(msg);
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub persona: Persona,
    pub auth: AuthManager,
    pub client: AtlassianClient,
    pub site_pool: SiteClientPool,
    pub site_scope: SiteScope,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,

    pub me: Option<UserSummary>,
    pub syncing: bool,
    pub sync_summary: String,

    pub projects: Vec<ProjectSummary>,
    pub boards: Vec<BoardSummary>,
    pub board_site_ids: Vec<String>,
    pub sprints: Vec<SprintSummary>,
    pub board_issues: Vec<IssueSummary>,

    pub saved_filters: Vec<SavedFilter>,
    pub issues: Vec<IssueSummary>,
    pub issue_site_ids: Vec<String>,
    pub active_issue: Option<IssueDetail>,

    pub spaces: Vec<SpaceSummary>,
    pub space_site_ids: Vec<String>,
    pub root_pages_by_space: HashMap<String, Vec<PageSummary>>,
    pub page_children_map: HashMap<String, Vec<PageSummary>>,
    pub expanded_pages: HashSet<String>,
    pub active_page: Option<PageDetail>,

    pub my_issues: Vec<IssueSummary>,
    pub my_issue_site_labels: Vec<Option<String>>,
    pub my_pages: Vec<PageSummary>,
    pub my_page_site_labels: Vec<Option<String>>,

    pub search_hits: Vec<UnifiedSearchHit>,
    pub search_hit_site_ids: Vec<String>,
    pub search_hit_site_labels: Vec<Option<String>>,
    pub cache_counts: CacheCounts,

    /// Cross-App navigation stack (`Jira <-> Confluence`).
    pub nav_history: Vec<CrossRefTarget>,

    /// Decoded image cache (`"{att_id}:{cols}x{rows}[:{protocol}]"` -> `RenderedTerminalImage`).
    pub image_cache: HashMap<String, RenderedTerminalImage>,

    /// Lazy localhost Axum server for viewing HTML documents in the default browser (`0 MB` idle).
    pub html_server: bitefoundry_html::NativeHtmlServer,
    /// Last localhost URL returned by `html_server.serve_html`.
    pub last_served_html_url: Option<String>,

    /// Persistent unsent drafts (`drafts.json`) for comments, issues, and Confluence pages.
    pub drafts: DraftStore,

    /// Optional file path overrides for isolated persistence in tests.
    pub config_path_override: Option<PathBuf>,
    pub drafts_path_override: Option<PathBuf>,
}

impl AppState {
    pub fn new_with_persona(
        config: AppConfig,
        persona: Persona,
        auth: AuthManager,
        client: AtlassianClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let saved_filters = builtin_filters();
        let drafts = if cfg!(test) && std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            DraftStore::default()
        } else {
            DraftStore::load()
        };
        let site_scope = SiteScope::from_optional(config.active_site.as_deref());
        let site_pool = SiteClientPool::from_config(&config).unwrap_or_else(|_| {
            SiteClientPool::new(Vec::new(), |_| auth.clone(), None, None)
                .expect("empty site pool initialization")
        });
        Self {
            config,
            persona,
            auth,
            client,
            site_pool,
            site_scope,
            storage,
            sync_ctrl,
            ui_tx,
            me: None,
            syncing: false,
            sync_summary: String::new(),
            projects: Vec::new(),
            boards: Vec::new(),
            board_site_ids: Vec::new(),
            sprints: Vec::new(),
            board_issues: Vec::new(),
            saved_filters,
            issues: Vec::new(),
            issue_site_ids: Vec::new(),
            active_issue: None,
            spaces: Vec::new(),
            space_site_ids: Vec::new(),
            root_pages_by_space: HashMap::new(),
            page_children_map: HashMap::new(),
            expanded_pages: HashSet::new(),
            active_page: None,
            my_issues: Vec::new(),
            my_issue_site_labels: Vec::new(),
            my_pages: Vec::new(),
            my_page_site_labels: Vec::new(),
            search_hits: Vec::new(),
            search_hit_site_ids: Vec::new(),
            search_hit_site_labels: Vec::new(),
            cache_counts: CacheCounts::default(),
            nav_history: Vec::new(),
            image_cache: HashMap::new(),
            html_server: bitefoundry_html::NativeHtmlServer::new(),
            last_served_html_url: None,
            drafts,
            config_path_override: None,
            drafts_path_override: None,
        }
    }

    pub fn save_config(&self) -> anyhow::Result<()> {
        if let Some(ref path) = self.config_path_override {
            return self.config.save_to(path);
        }
        if cfg!(test) && std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.config.save()
    }

    pub fn save_drafts(&self) -> anyhow::Result<()> {
        if let Some(ref path) = self.drafts_path_override {
            return self.drafts.save_to(path);
        }
        if cfg!(test) && std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.drafts.save()
    }

    pub fn sync_ui_from_config(&self, ui: &mut UiState) {
        ui.theme = Theme::detect(self.config.theme);
        ui.density = self.config.density;
        ui.show_nav_rail = self.config.show_nav_rail;
        ui.inline_images = self.config.inline_images;
        ui.notify = self.config.notify;
        ui.graphics_caps = img_preview::detect_graphics_caps(config_protocol_to_img_pref(
            self.config.image_protocol,
        ));
    }

    fn notify_desktop(&self, summary: &str, body: &str) {
        if self.config.notify && !cfg!(test) {
            notification::show(summary, &notification::truncate(body, notification::MAX_BODY_CHARS));
        }
    }

    fn notify_desktop_issue(&self, issue_key: &str, summary: &str) {
        if !cfg!(test) {
            notification::notify_issue(self.config.notify, false, false, issue_key, summary);
        }
    }

    fn notify_desktop_page(&self, space_key: &str, title: &str) {
        if !cfg!(test) {
            notification::notify_page(self.config.notify, false, false, space_key, title);
        }
    }

    pub fn image_cache_key(att_id: &str, cols: u16, rows: u16) -> String {
        format!("{att_id}:{cols}x{rows}")
    }

    pub fn site_display(&self) -> String {
        if let Some(label) = self.active_scope_label() {
            return label;
        }
        let enabled = self.config.enabled_sites();
        if enabled.len() > 1 {
            return format!("All Sites ({})", enabled.len());
        }
        if let Some(first) = enabled.first() {
            return first.display_name().to_string();
        }
        let raw = self.config.site_url.trim();
        if raw.is_empty() {
            "Not configured".to_string()
        } else {
            raw.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        }
    }

    /// Rebuilds the shared `SiteClientPool` from `self.config` and updates `self.client`
    /// when a specific site scope is active.
    pub fn rebuild_site_pool(&mut self) {
        if let Ok(pool) = SiteClientPool::from_config(&self.config) {
            self.site_pool = pool;
        }
        if let SiteScope::Only(ref id) = self.site_scope {
            if let Ok(scoped_client) = self.site_pool.client_for(id) {
                self.client = scoped_client;
            }
        } else if let Some(def) = self.config.default_site_profile() {
            if let Ok(def_client) = self.site_pool.client_for(&def.id) {
                self.client = def_client;
            }
        }
    }

    /// Resolves a human-friendly site label for `site_id` (site `name` or fallback `site_id`).
    pub fn site_display_label(&self, site_id: &str) -> String {
        if let Some(site) = self.config.find_effective_site(site_id) {
            return site.display_name().to_string();
        }
        site_id.to_string()
    }

    /// Returns `None` in `All Sites` federated mode, or `Some(display_name)` when scoped to 1 site.
    pub fn active_scope_label(&self) -> Option<String> {
        self.site_scope
            .site_id()
            .map(|id| self.site_display_label(id))
    }

    /// Label rendered inside the clickable top-header site switcher pill.
    pub fn header_site_pill_label(&self) -> String {
        match &self.site_scope {
            SiteScope::All => {
                let enabled = self.config.enabled_sites();
                if enabled.len() > 1 {
                    format!("All Sites ({})", enabled.len())
                } else if self.config.sites.is_empty() && !self.config.site_url.trim().is_empty() {
                    self.config
                        .site_url
                        .trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .to_string()
                } else {
                    "All Sites".to_string()
                }
            }
            SiteScope::Only(id) => self.site_display_label(id),
        }
    }

    /// True when in `All Sites` mode and multiple enabled sites exist (or multiple distinct sites are loaded).
    pub fn is_multi_site_all_mode(&self) -> bool {
        if !self.site_scope.is_all() {
            return false;
        }
        if self.config.enabled_sites().len() > 1 {
            return true;
        }
        let distinct_boards: HashSet<&str> = self.board_site_ids.iter().map(String::as_str).collect();
        let distinct_issues: HashSet<&str> = self.issue_site_ids.iter().map(String::as_str).collect();
        let distinct_spaces: HashSet<&str> = self.space_site_ids.iter().map(String::as_str).collect();
        distinct_boards.len() > 1 || distinct_issues.len() > 1 || distinct_spaces.len() > 1
    }

    fn site_is_enabled_in_config(&self, site_id: &str) -> bool {
        if self.config.sites.is_empty() {
            return true;
        }
        match self.config.find_site(site_id) {
            Some(s) => s.enabled,
            None => true,
        }
    }

    fn site_auth_label(&self, site: &SiteProfile) -> String {
        let mgr = self.auth.with_account(&site.account_id);
        let creds = mgr.load_credentials().ok().flatten().or_else(|| {
            if site.account_id == self.auth.account_id() {
                self.auth.load_credentials().ok().flatten()
            } else {
                None
            }
        });
        match creds {
            Some(StoredCredentials::CloudOAuth { .. }) => "Cloud OAuth".to_string(),
            Some(StoredCredentials::CloudAccountToken { .. }) => "Cloud Token".to_string(),
            Some(StoredCredentials::ApiToken { deployment, .. }) => match deployment {
                DeploymentKind::Cloud => "Cloud Token".to_string(),
                DeploymentKind::DataCenter => "DC Basic".to_string(),
            },
            Some(StoredCredentials::BearerPat { deployment, .. }) => match deployment {
                DeploymentKind::Cloud => "Cloud Bearer".to_string(),
                DeploymentKind::DataCenter => "DC PAT".to_string(),
            },
            None => {
                if site.is_oauth_cloud() {
                    "Cloud OAuth".to_string()
                } else {
                    "Offline".to_string()
                }
            }
        }
    }

    /// Builds `SiteSwitcherOverlay` rows from all effective configured sites and SQLite cache counts.
    pub fn build_site_switcher_entries(&self) -> Vec<SiteSwitcherEntry> {
        let sites = self.config.effective_sites();
        let mut entries = Vec::with_capacity(sites.len());
        for site in sites {
            let counts = self
                .storage
                .cache_counts_for_site(&site.id)
                .unwrap_or_default();
            let deployment_label = match site.deployment {
                DeploymentKind::Cloud => "Cloud".to_string(),
                DeploymentKind::DataCenter => "Data Center".to_string(),
            };
            let auth_label = self.site_auth_label(&site);
            let is_active = self
                .site_scope
                .site_id()
                .is_some_and(|id| id.eq_ignore_ascii_case(&site.id));
            entries.push(SiteSwitcherEntry {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment_label,
                auth_label,
                enabled: site.enabled,
                is_active,
                issue_count: counts.issues,
                page_count: counts.pages,
            });
        }
        entries
    }

    /// Builds **Connected Sites & Accounts** rows for the Directory tab.
    pub fn build_connected_site_rows(&self) -> Vec<ConnectedSiteRow> {
        let sites = self.config.effective_sites();
        let mut rows = Vec::with_capacity(sites.len());
        for site in sites {
            let db_rec = self.storage.get_site(&site.id).ok().flatten();
            let deployment_label = match site.deployment {
                DeploymentKind::Cloud => "Cloud".to_string(),
                DeploymentKind::DataCenter => "Data Center".to_string(),
            };
            let auth_label = self.site_auth_label(&site);
            let is_active = match &self.site_scope {
                SiteScope::All => site.enabled,
                SiteScope::Only(id) => id.eq_ignore_ascii_case(&site.id),
            };
            let sync_status = if !site.enabled {
                "Disabled".to_string()
            } else if self.syncing {
                "Syncing…".to_string()
            } else if let Some(ref rec) = db_rec {
                if let Some(ref err) = rec.last_error {
                    format!("Error: {}", err.chars().take(24).collect::<String>())
                } else if let Some(ref ts) = rec.last_synced_at {
                    format!("Synced {}", ts.chars().take(16).collect::<String>())
                } else {
                    "Idle".to_string()
                }
            } else if auth_label == "Offline" {
                "Offline".to_string()
            } else {
                "Ready".to_string()
            };
            rows.push(ConnectedSiteRow {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment_label,
                auth_label,
                enabled: site.enabled,
                is_active,
                sync_status,
            });
        }
        rows
    }

    /// Applies a `SiteSwitcherCommand` emitted by `SiteSwitcherOverlay`, persisting
    /// `active_site` / `enabled` to `config.toml` and refreshing federated views.
    pub fn apply_site_switcher_command(&mut self, cmd: SiteSwitcherCommand, ui: &mut UiState) {
        match cmd {
            SiteSwitcherCommand::SelectAllSites => {
                self.site_scope = SiteScope::All;
                self.config.active_site = None;
                self.rebuild_site_pool();
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                ui.board_ui.selected_board = 0;
                ui.issue_ui.selected_issue = 0;
                ui.confluence_ui.selected_space = 0;
                ui.success("Scope: All Sites (Federated)");
            }
            SiteSwitcherCommand::SelectSite(site_id) => {
                let label = self.site_display_label(&site_id);
                self.site_scope = SiteScope::Only(site_id.clone());
                self.config.active_site = Some(site_id);
                self.rebuild_site_pool();
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                ui.board_ui.selected_board = 0;
                ui.issue_ui.selected_issue = 0;
                ui.confluence_ui.selected_space = 0;
                ui.success(format!("Active site: {label}"));
            }
            SiteSwitcherCommand::ToggleSiteEnabled(site_id) => {
                if self.config.sites.is_empty() {
                    self.config.sites = self.config.effective_sites();
                }
                let mut now_enabled = None;
                let mut label = site_id.clone();
                for site in &mut self.config.sites {
                    if site.id.eq_ignore_ascii_case(&site_id) {
                        site.enabled = !site.enabled;
                        now_enabled = Some(site.enabled);
                        label = site.display_name().to_string();
                        let _ = self.storage.upsert_site(&SiteRecord {
                            site_id: site.id.clone(),
                            name: site.display_name().to_string(),
                            site_url: site.site_url.clone(),
                            deployment: site.deployment.as_str().to_string(),
                            enabled: site.enabled,
                            last_synced_at: None,
                            last_error: None,
                        });
                        break;
                    }
                }
                self.rebuild_site_pool();
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                if let Some(enabled) = now_enabled {
                    ui.info(format!(
                        "{}: {}",
                        label,
                        if enabled { "Enabled" } else { "Disabled" }
                    ));
                }
            }
            SiteSwitcherCommand::DiscoverCloudSites => {
                self.request_discover_cloud_sites(ui);
            }
            SiteSwitcherCommand::SyncAllSites => {
                self.request_multi_site_sync(ui);
            }
        }
    }

    /// Triggers Cloud OAuth 2.0 (3LO) `accessible-resources` site discovery in the background.
    pub fn request_discover_cloud_sites(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.info("Run `jira login --oauth` or `jira sites discover` to discover Cloud sites");
            return;
        }
        // Check if any configured account already holds Cloud OAuth credentials.
        let mut oauth_account: Option<AuthManager> = None;
        if let Ok(Some(StoredCredentials::CloudOAuth { .. })) = self.auth.load_credentials() {
            oauth_account = Some(self.auth.clone());
        }
        if oauth_account.is_none() {
            for (site, client) in self.site_pool.all_clients() {
                if let Ok(Some(StoredCredentials::CloudOAuth { .. })) =
                    client.auth().load_credentials()
                {
                    oauth_account = Some(client.auth().clone());
                    let _ = site;
                    break;
                }
            }
        }

        if let Some(auth_mgr) = oauth_account {
            ui.tasks.begin("Discover Cloud sites");
            let http = self.site_pool.http_client().clone();
            let tx = self.ui_tx.clone();
            let account_id = auth_mgr.account_id().to_string();
            tokio::spawn(async move {
                let res = async {
                    let fresh = auth_mgr
                        .ensure_fresh_credentials(&http, DEFAULT_OAUTH_AUTH_BASE)
                        .await?;
                    let token = fresh
                        .oauth_access_token()
                        .ok_or_else(|| anyhow::anyhow!("OAuth access token missing"))?;
                    let resources =
                        fetch_accessible_resources(&http, DEFAULT_OAUTH_API_BASE, token).await?;
                    let profiles = resources
                        .into_iter()
                        .map(|r| r.to_site_profile(account_id.clone()))
                        .collect::<Vec<_>>();
                    Ok::<Vec<SiteProfile>, anyhow::Error>(profiles)
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::CloudSitesDiscovered(res));
            });
            return;
        }

        if let Some(client_id) = self.config.effective_oauth_client_id() {
            ui.tasks.begin("Discover Cloud sites");
            let http = self.site_pool.http_client().clone();
            let tx = self.ui_tx.clone();
            let auth_mgr = self.auth.clone();
            let client_secret = self.config.effective_oauth_client_secret();
            tokio::spawn(async move {
                let opts = OAuthLoopbackOptions {
                    client_id,
                    client_secret,
                    ..Default::default()
                };
                let res = async {
                    let (creds, resources) = run_oauth_loopback_login(&http, &opts).await?;
                    auth_mgr.save_credentials(&creds)?;
                    let account_id = auth_mgr.account_id().to_string();
                    let profiles = resources
                        .into_iter()
                        .map(|r| r.to_site_profile(account_id.clone()))
                        .collect::<Vec<_>>();
                    Ok::<Vec<SiteProfile>, anyhow::Error>(profiles)
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::CloudSitesDiscovered(res));
            });
            return;
        }

        ui.info(
            "No OAuth session found. Run `jira login --oauth --client-id <ID>` to discover Cloud sites.",
        );
    }

    /// Triggers parallel multi-site sync across all enabled sites (or falls back to single-site sync).
    pub fn request_multi_site_sync(&mut self, ui: &mut UiState) {
        self.syncing = true;
        let enabled = self.site_pool.enabled_clients();
        if enabled.len() > 1 && tokio::runtime::Handle::try_current().is_ok() {
            ui.tasks.begin("Sync all sites");
            let targets: Vec<SiteSyncTarget> = enabled
                .into_iter()
                .map(|(site, client)| SiteSyncTarget {
                    site_id: site.id.clone(),
                    site_name: site.display_name().to_string(),
                    client,
                })
                .collect();
            let storage = self.storage.clone();
            let tx = self.ui_tx.clone();
            tokio::spawn(async move {
                let mut breakers = sync::CircuitBreakerMap::new();
                let outcome = sync::run_multi_site_sync(
                    &targets,
                    &storage,
                    &mut breakers,
                    sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                    None,
                )
                .await;
                let _ = tx.send(UiMsg::MultiSiteSyncFinished(outcome));
            });
            ui.info("Syncing all enabled sites in parallel…");
        } else {
            let _ = self.sync_ctrl.request(sync::SyncCommand::RefreshAll);
            ui.info("Requested full workspace sync");
        }
    }

    pub fn load_cached_initial(&mut self) {
        let show_pills = self.site_scope.is_all();
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                if let Ok(projs) = self.storage.list_projects_for_site(site_id) {
                    self.projects = projs;
                }
                if let Ok(boards) = self.storage.list_boards_for_site(site_id, None) {
                    self.board_site_ids = vec![site_id.clone(); boards.len()];
                    self.boards = boards;
                    if let Some(first_board) = self.boards.first() {
                        let bid = first_board.id;
                        if let Ok(sprints) = self.storage.list_sprints_for_site(site_id, bid) {
                            self.sprints = sprints;
                        }
                        if let Ok(bi) =
                            self.storage
                                .list_board_issues_for_site(site_id, bid, None, 150)
                        {
                            self.board_issues = bi;
                        }
                    } else {
                        self.sprints.clear();
                        self.board_issues.clear();
                    }
                }
                if let Ok(mut filters) = self.storage.list_saved_filters_for_site(site_id) {
                    let mut combined = builtin_filters();
                    for f in filters.drain(..) {
                        if !combined.iter().any(|b| b.id == f.id) {
                            combined.push(f);
                        }
                    }
                    self.saved_filters = combined;
                }
                if let Ok(issues) = self.storage.list_issues_for_site(site_id, None, 200) {
                    self.issue_site_ids = vec![site_id.clone(); issues.len()];
                    self.issues = issues.clone();
                    self.my_issues = issues.iter().take(20).cloned().collect();
                    self.my_issue_site_labels = vec![None; self.my_issues.len()];
                    if let Some(first) = issues.first() {
                        if let Ok(Some(detail)) =
                            self.storage.get_issue_detail_for_site(site_id, &first.key)
                        {
                            self.active_issue = Some(detail);
                        }
                    }
                }
                if let Ok(spaces) = self.storage.list_spaces_for_site(site_id) {
                    self.space_site_ids = vec![site_id.clone(); spaces.len()];
                    self.spaces = spaces;
                    if let Some(first_space) = self.spaces.first() {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(site_id, &first_space.key)
                        {
                            self.root_pages_by_space
                                .insert(first_space.key.clone(), roots);
                        }
                    }
                }
                if let Ok(pages) = self.storage.list_recent_pages_for_site(site_id, 30) {
                    self.my_pages = pages;
                    self.my_page_site_labels = vec![None; self.my_pages.len()];
                }
                if let Ok(counts) = self.storage.cache_counts_for_site(site_id) {
                    self.cache_counts = counts;
                }
            }
            None => {
                if let Ok(projs) = self.storage.list_projects_all_sites() {
                    self.projects = projs
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .map(|(_, p)| p)
                        .collect();
                }
                if let Ok(boards) = self.storage.list_boards_all_sites(None) {
                    let filtered: Vec<(String, BoardSummary)> = boards
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.board_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.boards = filtered.into_iter().map(|(_, b)| b).collect();
                    if let (Some(first_board), Some(first_sid)) =
                        (self.boards.first(), self.board_site_ids.first())
                    {
                        let bid = first_board.id;
                        if let Ok(sprints) = self.storage.list_sprints_for_site(first_sid, bid) {
                            self.sprints = sprints;
                        }
                        if let Ok(bi) = self
                            .storage
                            .list_board_issues_for_site(first_sid, bid, None, 150)
                        {
                            self.board_issues = bi;
                        }
                    }
                }
                if let Ok(mut filters) = self.storage.list_saved_filters() {
                    if !filters.is_empty() {
                        let mut combined = builtin_filters();
                        for f in filters.drain(..) {
                            if !combined.iter().any(|b| b.id == f.id) {
                                combined.push(f);
                            }
                        }
                        self.saved_filters = combined;
                    }
                }
                if let Ok(issues_all) = self.storage.list_issues_all_sites(None, 200) {
                    let filtered: Vec<(String, IssueSummary)> = issues_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    let has_multi = self.config.enabled_sites().len() > 1
                        || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                    self.issue_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.issues = filtered.iter().map(|(_, i)| i.clone()).collect();

                    let my_work_pairs = self
                        .storage
                        .list_my_work_all_sites(40)
                        .ok()
                        .map(|rows| {
                            rows.into_iter()
                                .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                                .collect::<Vec<_>>()
                        })
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| filtered.iter().take(20).cloned().collect());
                    self.my_issue_site_labels = my_work_pairs
                        .iter()
                        .map(|(sid, _)| {
                            (show_pills && has_multi).then(|| self.site_display_label(sid))
                        })
                        .collect();
                    self.my_issues = my_work_pairs.into_iter().map(|(_, i)| i).collect();

                    if let Some((first_sid, first)) = filtered.first() {
                        if let Ok(Some(detail)) = self
                            .storage
                            .get_issue_detail_for_site(first_sid, &first.key)
                        {
                            self.active_issue = Some(detail);
                        }
                    }
                }
                if let Ok(spaces_all) = self.storage.list_spaces_all_sites() {
                    let filtered: Vec<(String, SpaceSummary)> = spaces_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.space_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.spaces = filtered.into_iter().map(|(_, s)| s).collect();
                    if let (Some(first_space), Some(first_sid)) =
                        (self.spaces.first(), self.space_site_ids.first())
                    {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(first_sid, &first_space.key)
                        {
                            self.root_pages_by_space
                                .insert(first_space.key.clone(), roots);
                        }
                    }
                }
                if let Ok(pages_all) = self.storage.list_recent_pages_all_sites(30) {
                    let filtered: Vec<(String, PageSummary)> = pages_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    let has_multi = self.config.enabled_sites().len() > 1
                        || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                    self.my_page_site_labels = filtered
                        .iter()
                        .map(|(sid, _)| {
                            (show_pills && has_multi).then(|| self.site_display_label(sid))
                        })
                        .collect();
                    self.my_pages = filtered.into_iter().map(|(_, p)| p).collect();
                }
                if let Ok(counts) = self.storage.cache_counts() {
                    self.cache_counts = counts;
                }
            }
        }
        self.refresh_local_search("");
    }

    pub fn spawn_bootstrap(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if !self.auth.is_logged_in() || self.config.site_url.trim().is_empty() {
            ui.info("Offline cache mode (run `jira login` to connect)");
            return;
        }
        ui.tasks.begin("Initial sync");
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match client.get_myself().await {
                Ok(me) => {
                    client.set_cached_me(me.clone());
                    let _ = tx.send(UiMsg::MeLoaded(Ok(me)));
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::MeLoaded(Err(e.to_string())));
                }
            }
            let summary = sync::bootstrap_workspace(&client, &storage)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::BootstrapFinished(summary));
            if let Ok(filters) = client.list_favourite_filters().await {
                let _ = storage.upsert_saved_filters(&filters);
                let _ = tx.send(UiMsg::SavedFiltersLoaded(filters));
            }
        });
    }

    pub fn active_board(&self, ui: &UiState) -> Option<&BoardSummary> {
        self.boards.get(ui.board_ui.selected_board)
    }

    pub fn build_kanban_columns(&self, ui: &UiState) -> Vec<KanbanColumn> {
        build_kanban_columns_for_board(self.active_board(ui), &self.board_issues)
    }

    pub fn selected_board_issue(&self, ui: &UiState) -> Option<IssueSummary> {
        let cols = self.build_kanban_columns(ui);
        cols.get(ui.board_ui.selected_col)
            .and_then(|c| c.issues.get(ui.board_ui.selected_row))
            .cloned()
    }

    pub fn filtered_issues(&self, ui: &UiState) -> Vec<IssueSummary> {
        self.filtered_issues_with_sites(ui)
            .into_iter()
            .map(|(_, issue)| issue)
            .collect()
    }

    pub fn filtered_issues_with_sites(&self, ui: &UiState) -> Vec<(String, IssueSummary)> {
        let q = ui.issue_ui.local_filter.text.trim().to_ascii_lowercase();
        let fallback_site = self
            .site_scope
            .site_id()
            .unwrap_or(DEFAULT_SITE_ID)
            .to_string();
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                q.is_empty()
                    || it.key.to_ascii_lowercase().contains(&q)
                    || it.summary.to_ascii_lowercase().contains(&q)
                    || it.status.name.to_ascii_lowercase().contains(&q)
                    || it
                        .assignee
                        .as_ref()
                        .is_some_and(|a| a.display_name.to_ascii_lowercase().contains(&q))
            })
            .map(|(idx, it)| {
                let sid = self
                    .issue_site_ids
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| fallback_site.clone());
                (sid, it.clone())
            })
            .collect()
    }

    pub fn active_space(&self, ui: &UiState) -> Option<&SpaceSummary> {
        self.spaces.get(ui.confluence_ui.selected_space)
    }

    pub fn current_confluence_tree(&self, ui: &UiState) -> Vec<TreeRow> {
        let Some(space) = self.active_space(ui) else {
            return Vec::new();
        };
        let roots = self
            .root_pages_by_space
            .get(&space.key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        flatten_page_tree(roots, &self.page_children_map, &self.expanded_pages)
    }

    pub fn selected_issue_key(&self, ui: &UiState) -> Option<String> {
        match ui.active_tab {
            WorkspaceTab::Board => self
                .selected_board_issue(ui)
                .map(|i| i.key)
                .or_else(|| self.active_issue.as_ref().map(|d| d.summary.key.clone())),
            WorkspaceTab::Issues => {
                let list = self.filtered_issues(ui);
                list.get(ui.issue_ui.selected_issue)
                    .map(|i| i.key.clone())
                    .or_else(|| self.active_issue.as_ref().map(|d| d.summary.key.clone()))
            }
            WorkspaceTab::MyWork => self
                .my_issues
                .get(ui.my_work_ui.selected_issue)
                .map(|i| i.key.clone()),
            _ => self.active_issue.as_ref().map(|d| d.summary.key.clone()),
        }
    }

    pub fn current_attachments(&self, ui: &UiState) -> (String, Vec<AttachmentItem>) {
        if ui.active_tab == WorkspaceTab::Confluence
            || (ui.active_tab == WorkspaceTab::MyWork && ui.my_work_ui.active_column == 1)
        {
            if let Some(p) = &self.active_page {
                return (p.summary.title.clone(), p.attachments.clone());
            }
        }
        if let Some(issue) = &self.active_issue {
            return (issue.summary.key.clone(), issue.attachments.clone());
        }
        if let Some(page) = &self.active_page {
            return (page.summary.title.clone(), page.attachments.clone());
        }
        (String::new(), Vec::new())
    }

    pub fn current_cross_ref(&self, tab: WorkspaceTab) -> Option<CrossRefTarget> {
        match tab {
            WorkspaceTab::Confluence => self.active_page.as_ref().map(|p| CrossRefTarget::ConfluencePage {
                page_id: p.summary.id.clone(),
                title: Some(p.summary.title.clone()),
            }),
            _ => self.active_issue.as_ref().map(|i| CrossRefTarget::JiraIssue {
                key: i.summary.key.clone(),
                label: Some(i.summary.summary.clone()),
            }),
        }
    }

    pub fn collect_cross_links(&self, tab: WorkspaceTab) -> (String, Vec<CrossRefTarget>) {
        let mut out: Vec<CrossRefTarget> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        let push_unique = |out: &mut Vec<CrossRefTarget>, seen: &mut HashSet<String>, item: CrossRefTarget| {
            let key = match &item {
                CrossRefTarget::JiraIssue { key, .. } => format!("J:{}", key.to_ascii_uppercase()),
                CrossRefTarget::ConfluencePage { page_id, .. } => format!("C:{page_id}"),
            };
            if seen.insert(key) {
                out.push(item);
            }
        };

        if tab == WorkspaceTab::Confluence {
            let Some(page) = &self.active_page else {
                return ("Confluence".to_string(), out);
            };
            for k in &page.linked_issue_keys {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue {
                        key: k.clone(),
                        label: None,
                    },
                );
            }
            for r in adf::extract_cross_refs(&page.blocks, &page.body_markdown) {
                push_unique(&mut out, &mut seen, r);
            }
            for k in scan_issue_keys(&page.body_markdown) {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue { key: k, label: None },
                );
            }
            return (page.summary.title.clone(), out);
        }

        let Some(issue) = &self.active_issue else {
            return ("Jira".to_string(), out);
        };
        let self_key = issue.summary.key.to_ascii_uppercase();
        for lp in &issue.linked_pages {
            push_unique(
                &mut out,
                &mut seen,
                CrossRefTarget::ConfluencePage {
                    page_id: lp.page_id.clone(),
                    title: Some(lp.title.clone()),
                },
            );
        }
        for li in &issue.linked_issues {
            if li.key.to_ascii_uppercase() != self_key {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue {
                        key: li.key.clone(),
                        label: Some(li.summary.clone()),
                    },
                );
            }
        }
        for r in adf::extract_cross_refs(&issue.description_blocks, &issue.description_markdown) {
            if let CrossRefTarget::JiraIssue { ref key, .. } = r {
                if key.to_ascii_uppercase() == self_key {
                    continue;
                }
            }
            push_unique(&mut out, &mut seen, r);
        }
        for k in scan_issue_keys(&issue.description_markdown) {
            if k.to_ascii_uppercase() != self_key {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue { key: k, label: None },
                );
            }
        }
        for c in &issue.comments {
            for r in adf::extract_cross_refs(&c.blocks, &c.body_markdown) {
                if let CrossRefTarget::JiraIssue { ref key, .. } = r {
                    if key.to_ascii_uppercase() == self_key {
                        continue;
                    }
                }
                push_unique(&mut out, &mut seen, r);
            }
        }
        (issue.summary.key.clone(), out)
    }

    pub fn jump_to_cross_ref(&mut self, ui: &mut UiState, target: CrossRefTarget) {
        if let Some(current) = self.current_cross_ref(ui.active_tab) {
            if self.nav_history.last() != Some(&current) && current != target {
                self.nav_history.push(current);
            }
        }
        self.open_cross_ref_without_push(ui, target);
    }

    pub fn pop_nav_history(&mut self, ui: &mut UiState) -> bool {
        let Some(prev) = self.nav_history.pop() else {
            return false;
        };
        self.open_cross_ref_without_push(ui, prev);
        true
    }

    pub fn open_cross_ref_without_push(&mut self, ui: &mut UiState, target: CrossRefTarget) {
        match target {
            CrossRefTarget::JiraIssue { key, .. } => {
                ui.active_tab = WorkspaceTab::Issues;
                ui.focus = PaneFocus::Main;
                if let Some(pos) = self.issues.iter().position(|i| i.key == key) {
                    ui.issue_ui.selected_issue = pos;
                }
                self.request_issue_detail(ui, &key);
            }
            CrossRefTarget::ConfluencePage { page_id, .. } => {
                ui.active_tab = WorkspaceTab::Confluence;
                ui.focus = PaneFocus::Main;
                self.request_page_detail(ui, &page_id);
            }
        }
    }

    pub fn refresh_local_search(&mut self, query: &str) {
        if let Ok(scoped_hits) =
            self.storage
                .unified_search_scoped(query, self.site_scope.site_id(), 50)
        {
            let filtered: Vec<(String, UnifiedSearchHit)> = scoped_hits
                .into_iter()
                .filter(|(sid, _)| {
                    !self.site_scope.is_all() || self.site_is_enabled_in_config(sid)
                })
                .collect();
            let show_pills = self.site_scope.is_all()
                && (self.config.enabled_sites().len() > 1
                    || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID));
            self.search_hit_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
            self.search_hit_site_labels = filtered
                .iter()
                .map(|(s, _)| show_pills.then(|| self.site_display_label(s)))
                .collect();
            self.search_hits = filtered.into_iter().map(|(_, h)| h).collect();
        }
    }

    // ------------------------------------------------------------------
    // Async requests
    // ------------------------------------------------------------------

    pub fn request_board_load(&self, ui: &mut UiState, board_id: i64, sprint_id: Option<i64>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Board #{board_id}"));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let columns = client.get_board_columns(board_id).await.unwrap_or_default();
            let sprints = client.list_board_sprints(board_id).await.unwrap_or_default();
            if !sprints.is_empty() {
                let _ = storage.upsert_sprints(&sprints);
            }
            let effective_sprint = sprint_id.or_else(|| {
                sprints
                    .iter()
                    .find(|s| s.state.eq_ignore_ascii_case("active"))
                    .map(|s| s.id)
            });
            match client.list_board_issues(board_id, effective_sprint, 150).await {
                Ok(issues) => {
                    let _ = storage.upsert_issues(&issues);
                    let _ = tx.send(UiMsg::BoardDataLoaded {
                        board_id,
                        columns,
                        sprints,
                        issues,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::BoardLoadFailed(e.to_string()));
                }
            }
        });
    }

    pub fn request_jql_search(&self, ui: &mut UiState, jql: String, label: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("JQL: {label}"));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match client.search_issues_jql(&jql, 80, None, None).await {
                Ok(page) => {
                    let _ = storage.upsert_issues(&page.issues);
                    let _ = tx.send(UiMsg::IssuesLoaded {
                        issues: page.issues,
                        label,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssuesLoadFailed(e.to_string()));
                }
            }
        });
    }

    pub fn request_issue_detail(&mut self, ui: &mut UiState, key: &str) {
        if let Ok(Some(cached)) = self.storage.get_issue_detail(key) {
            self.active_issue = Some(cached);
        }
        ui.issue_ui.detail_scroll = 0;
        ui.board_ui.detail_scroll = 0;
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.get_issue(&key_owned).await {
                Ok(mut detail) => {
                    if let Ok(remote_links) = client.get_issue_remote_confluence_links(&key_owned).await {
                        for rl in remote_links {
                            if !detail.linked_pages.iter().any(|p| p.page_id == rl.page_id) {
                                detail.linked_pages.push(rl);
                            }
                        }
                    }
                    let _ = storage.upsert_issue_detail(&detail);
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        key: key_owned,
                        detail: Ok(detail),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        key: key_owned,
                        detail: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn request_open_transitions(&self, ui: &mut UiState, issue_key: &str) {
        ui.push_overlay(Overlay::Transition(TransitionOverlay::new(issue_key)));
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        let key = issue_key.to_string();
        tokio::spawn(async move {
            let res = client.list_transitions(&key).await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::TransitionsLoaded {
                issue_key: key,
                transitions: res,
            });
        });
    }

    pub fn request_transition_issue(
        &mut self,
        ui: &mut UiState,
        key: &str,
        tr: IssueTransition,
    ) {
        // Capture previous status for clean rollback on API error.
        let prev_status = self.current_issue_status(key);
        // 0ms optimistic local column update so Kanban card moves right away!
        self.apply_local_status_update(key, &tr.to_status);
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} → {}", tr.to_status.name));
            return;
        }
        ui.tasks.begin(format!("Transition {key}"));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.transition_issue(&key_owned, &tr.id, None).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::IssueTransitioned {
                        key: key_owned.clone(),
                        to_status: tr.to_status.clone(),
                        toast: format!("{key_owned} → {}", tr.to_status.name),
                    });
                    if let Ok(detail) = client.get_issue(&key_owned).await {
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueDetailLoaded {
                            key: key_owned,
                            detail: Ok(detail),
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueTransitionFailed {
                        key: key_owned,
                        previous_status: prev_status,
                        error: format!("Transition failed: {e}"),
                    });
                }
            }
        });
    }

    fn current_issue_status(&self, key: &str) -> Option<IssueStatus> {
        self.active_issue
            .as_ref()
            .filter(|d| d.summary.key == key)
            .map(|d| d.summary.status.clone())
            .or_else(|| self.board_issues.iter().find(|i| i.key == key).map(|i| i.status.clone()))
            .or_else(|| self.issues.iter().find(|i| i.key == key).map(|i| i.status.clone()))
            .or_else(|| self.my_issues.iter().find(|i| i.key == key).map(|i| i.status.clone()))
    }

    fn current_issue_assignee(&self, key: &str) -> Option<Option<UserSummary>> {
        self.active_issue
            .as_ref()
            .filter(|d| d.summary.key == key)
            .map(|d| d.summary.assignee.clone())
            .or_else(|| {
                self.board_issues
                    .iter()
                    .find(|i| i.key == key)
                    .map(|i| i.assignee.clone())
            })
            .or_else(|| self.issues.iter().find(|i| i.key == key).map(|i| i.assignee.clone()))
            .or_else(|| {
                self.my_issues
                    .iter()
                    .find(|i| i.key == key)
                    .map(|i| i.assignee.clone())
            })
    }

    fn apply_local_status_update(&mut self, key: &str, to_status: &IssueStatus) {
        for it in &mut self.board_issues {
            if it.key == key {
                it.status = to_status.clone();
            }
        }
        for it in &mut self.issues {
            if it.key == key {
                it.status = to_status.clone();
            }
        }
        for it in &mut self.my_issues {
            if it.key == key {
                it.status = to_status.clone();
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key {
                detail.summary.status = to_status.clone();
            }
        }
    }

    fn apply_local_assignee_update(&mut self, key: &str, assignee: Option<UserSummary>) {
        for it in &mut self.board_issues {
            if it.key == key {
                it.assignee = assignee.clone();
            }
        }
        for it in &mut self.issues {
            if it.key == key {
                it.assignee = assignee.clone();
            }
        }
        for it in &mut self.my_issues {
            if it.key == key {
                it.assignee = assignee.clone();
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key {
                detail.summary.assignee = assignee;
            }
        }
    }

    fn bump_local_comment_count(&mut self, key: &str, delta: isize) {
        let apply = |count: &mut usize| {
            if delta >= 0 {
                *count = count.saturating_add(delta as usize);
            } else {
                *count = count.saturating_sub((-delta) as usize);
            }
        };
        for it in &mut self.board_issues {
            if it.key == key {
                apply(&mut it.comment_count);
            }
        }
        for it in &mut self.issues {
            if it.key == key {
                apply(&mut it.comment_count);
            }
        }
        for it in &mut self.my_issues {
            if it.key == key {
                apply(&mut it.comment_count);
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key {
                apply(&mut detail.summary.comment_count);
            }
        }
    }

    pub fn request_assign_to_me(&mut self, ui: &mut UiState, key: &str) {
        let Some(me) = self.me.clone().or_else(|| self.client.cached_me()) else {
            ui.error("Current user profile has not been loaded yet");
            return;
        };
        let prev_assignee = self.current_issue_assignee(key);
        self.apply_local_assignee_update(key, Some(me.clone()));
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("Assigned {key} to you"));
            return;
        }
        let task_label = format!("Assign {key} to me");
        ui.tasks.begin(&task_label);
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.assign_issue(&key_owned, Some(&me.id)).await {
                Ok(()) => match client.get_issue(&key_owned).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            detail,
                            toast: format!("Assigned {key_owned} to you"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueAssignFailed {
                            key: key_owned,
                            previous_assignee: None,
                            task_label,
                            error: format!("Assigned issue, but failed to reload detail: {e}"),
                        });
                    }
                },
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to assign to me: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_open_assignee_picker(&self, ui: &mut UiState, key: &str, project_key: &str) {
        ui.push_overlay(Overlay::Assignee(AssigneeOverlay::new(key, project_key)));
        self.request_assignable_users(key, "");
    }

    pub fn request_assignable_users(&self, project_or_issue: &str, query: &str) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        let key = project_or_issue.to_string();
        let q = query.to_string();
        tokio::spawn(async move {
            let res = client
                .search_assignable_users(&key, &q)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AssignableUsersLoaded { query: q, users: res });
        });
    }

    pub fn request_assign_user(&mut self, ui: &mut UiState, key: &str, user: Option<UserSummary>) {
        let prev_assignee = self.current_issue_assignee(key);
        let label = user
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| "Unassigned".to_string());
        self.apply_local_assignee_update(key, user.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} assignee → {label}"));
            return;
        }
        let task_label = format!("Assign {key}");
        ui.tasks.begin(&task_label);
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            let id_ref = user.as_ref().map(|u| u.id.as_str());
            match client.assign_issue(&key_owned, id_ref).await {
                Ok(()) => match client.get_issue(&key_owned).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            detail,
                            toast: format!("{key_owned} assignee → {label}"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueAssignFailed {
                            key: key_owned,
                            previous_assignee: None,
                            task_label,
                            error: format!("Updated assignee, but failed to reload detail: {e}"),
                        });
                    }
                },
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to update assignee: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_submit_issue_form(&self, ui: &mut UiState, form: &IssueFormOverlay) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(key) = form.edit_key.clone() {
            ui.tasks.begin(format!("Update {key}"));
            let patch = IssueUpdate {
                summary: Some(form.summary.text.trim().to_string()),
                description_markdown: Some(form.description.text.clone()),
                priority: (!form.priority.text.trim().is_empty())
                    .then(|| form.priority.text.trim().to_string()),
                assignee_id: None,
                labels: Some(form.parsed_labels()),
            };
            tokio::spawn(async move {
                match client.update_issue(&key, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            detail,
                            toast: format!("Updated {key}"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(format!(
                            "Failed to update issue: {e}"
                        )));
                    }
                }
            });
        } else {
            ui.tasks.begin("Create issue");
            let req = OutgoingIssue {
                project_key: form.project.text.trim().to_ascii_uppercase(),
                issue_type: form.issue_type.text.trim().to_string(),
                summary: form.summary.text.trim().to_string(),
                description_markdown: form.description.text.clone(),
                priority: (!form.priority.text.trim().is_empty())
                    .then(|| form.priority.text.trim().to_string()),
                assignee_id: None,
                labels: form.parsed_labels(),
                parent_key: None,
            };
            tokio::spawn(async move {
                match client.create_issue(&req).await {
                    Ok(detail) => {
                        let k = detail.summary.key.clone();
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            detail,
                            toast: format!("Created {k}"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(format!(
                            "Failed to create issue: {e}"
                        )));
                    }
                }
            });
        }
    }

    pub fn request_submit_comment(&mut self, ui: &mut UiState, overlay: &CommentOverlay) {
        self.request_add_comment(ui, overlay.target.clone(), overlay.body.text.clone());
    }

    pub fn request_add_comment(
        &mut self,
        ui: &mut UiState,
        target: CommentTarget,
        body: String,
    ) {
        let optimistic_id = format!("optimistic:{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now().to_rfc3339();
        let author = self
            .me
            .clone()
            .or_else(|| self.client.cached_me())
            .unwrap_or_else(|| UserSummary {
                id: "me".to_string(),
                display_name: "You".to_string(),
                ..Default::default()
            });
        let blocks = adf::parse_markdown(&body);

        match &target {
            CommentTarget::Issue(key) => {
                if let Some(detail) = &mut self.active_issue {
                    if detail.summary.key == *key {
                        detail.comments.push(IssueComment {
                            id: optimistic_id.clone(),
                            issue_key: key.clone(),
                            author: author.clone(),
                            body_raw: body.clone(),
                            body_markdown: body.clone(),
                            blocks: blocks.clone(),
                            created_at: now.clone(),
                            updated_at: now.clone(),
                        });
                    }
                }
                self.bump_local_comment_count(key, 1);
            }
            CommentTarget::Page { page_id, .. } => {
                if let Some(page) = &mut self.active_page {
                    if page.summary.id == *page_id {
                        page.comments.push(ConfluenceComment {
                            id: optimistic_id.clone(),
                            page_id: page_id.clone(),
                            author_name: author.display_name.clone(),
                            body_raw: body.clone(),
                            body_markdown: body.clone(),
                            blocks: blocks.clone(),
                            created_at: now.clone(),
                        });
                    }
                }
            }
        }

        if tokio::runtime::Handle::try_current().is_err() {
            ui.success("Comment posted");
            return;
        }

        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        match target {
            CommentTarget::Issue(key) => {
                let task_label = format!("Comment on {key}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = client
                        .add_issue_comment(&key, &body)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::CommentAdded {
                        issue_key: Some(key),
                        page_id: None,
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        result: res,
                    });
                });
            }
            CommentTarget::Page { page_id, title } => {
                let task_label = format!("Comment on {title}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = client
                        .add_page_comment(&page_id, &body)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::CommentAdded {
                        issue_key: None,
                        page_id: Some(page_id),
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        result: res,
                    });
                });
            }
        }
    }

    pub fn request_space_root_pages(&mut self, ui: &mut UiState, space_key: &str) {
        if let Ok(roots) = self.storage.list_space_root_pages(space_key) {
            if !roots.is_empty() {
                self.root_pages_by_space
                    .insert(space_key.to_string(), roots);
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Space {space_key}"));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let sk = space_key.to_string();
        tokio::spawn(async move {
            match client.list_space_root_pages(&sk, 100).await {
                Ok(pages) => {
                    let _ = storage.upsert_pages(&pages);
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        space_key: sk,
                        pages: Ok(pages),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        space_key: sk,
                        pages: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn toggle_expand_page(&mut self, ui: &mut UiState, page: &PageSummary) {
        if self.expanded_pages.contains(&page.id) {
            self.expanded_pages.remove(&page.id);
            return;
        }
        self.expanded_pages.insert(page.id.clone());

        if self.page_children_map.contains_key(&page.id) {
            return;
        }
        if let Ok(cached) = self.storage.list_child_pages(&page.id) {
            if !cached.is_empty() {
                self.page_children_map.insert(page.id.clone(), cached);
            }
        }
        if !page.has_children || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Child pages: {}", page.title));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page.id.clone();
        tokio::spawn(async move {
            match client.list_child_pages(&pid).await {
                Ok(children) => {
                    let _ = storage.upsert_pages(&children);
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        parent_id: pid,
                        children: Ok(children),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        parent_id: pid,
                        children: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn request_page_detail(&mut self, ui: &mut UiState, page_id: &str) {
        if let Ok(Some(cached)) = self.storage.get_page_detail(page_id) {
            self.active_page = Some(cached);
        }
        ui.confluence_ui.reader_scroll = 0;
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page_id.to_string();
        tokio::spawn(async move {
            match client.get_page(&pid).await {
                Ok(detail) => {
                    let _ = storage.upsert_page_detail(&detail);
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        page_id: pid,
                        detail: Ok(detail),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        page_id: pid,
                        detail: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn request_submit_page_form(&self, ui: &mut UiState, form: &PageFormOverlay) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(pid) = form.edit_page_id.clone() {
            ui.tasks.begin("Update page");
            let patch = PageUpdate {
                title: form.title.text.trim().to_string(),
                body_markdown: form.body.text.clone(),
                prev_version: form.prev_version,
                version_message: Some("Edited via atlassian-cli TUI".to_string()),
            };
            tokio::spawn(async move {
                match client.update_page(&pid, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_page_detail(&detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            detail,
                            toast: "Confluence page saved".to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed(format!(
                            "Failed to update page: {e}"
                        )));
                    }
                }
            });
        } else {
            ui.tasks.begin("Create page");
            let req = OutgoingPage {
                space_id_or_key: form.space_key.clone(),
                parent_id: form.parent_id.clone(),
                title: form.title.text.trim().to_string(),
                body_markdown: form.body.text.clone(),
            };
            tokio::spawn(async move {
                match client.create_page(&req).await {
                    Ok(detail) => {
                        let _ = storage.upsert_page_detail(&detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            detail,
                            toast: "Created Confluence page".to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed(format!(
                            "Failed to create page: {e}"
                        )));
                    }
                }
            });
        }
    }

    pub fn request_my_work_refresh(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Loading My Work");
        let clients: Vec<(String, String, AtlassianClient)> = match &self.site_scope {
            SiteScope::Only(id) => {
                let c = self
                    .site_pool
                    .client_for(id)
                    .unwrap_or_else(|_| self.client.clone());
                vec![(id.clone(), self.site_display_label(id), c)]
            }
            SiteScope::All => {
                let enabled = self.site_pool.enabled_clients();
                if enabled.is_empty() {
                    vec![(
                        self.client.site_id().to_string(),
                        self.site_display_label(self.client.site_id()),
                        self.client.clone(),
                    )]
                } else {
                    enabled
                        .into_iter()
                        .map(|(s, c)| (s.id.clone(), s.display_name().to_string(), c))
                        .collect()
                }
            }
        };
        let show_pills = self.site_scope.is_all() && clients.len() > 1;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let mut all_issues: Vec<(Option<String>, IssueSummary)> = Vec::new();
            let mut all_pages: Vec<(Option<String>, PageSummary)> = Vec::new();
            for (site_id, site_label, client) in clients {
                let pill = show_pills.then_some(site_label);
                let issues = match client
                    .search_issues_jql(AtlassianClient::my_open_issues_jql(), 40, None, None)
                    .await
                {
                    Ok(p) => {
                        let _ = storage.upsert_issues_for_site(&site_id, &p.issues);
                        p.issues
                    }
                    Err(_) => storage
                        .list_issues_for_site(&site_id, None, 25)
                        .unwrap_or_default(),
                };
                for issue in issues {
                    all_issues.push((pill.clone(), issue));
                }
                let pages = match client
                    .search_confluence_cql("type = page ORDER BY lastmodified DESC", 25)
                    .await
                {
                    Ok(ps) => {
                        let _ = storage.upsert_pages_for_site(&site_id, &ps);
                        ps
                    }
                    Err(_) => storage
                        .list_recent_pages_for_site(&site_id, 25)
                        .unwrap_or_default(),
                };
                for page in pages {
                    all_pages.push((pill.clone(), page));
                }
            }
            all_issues.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
            all_pages.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
            let (issue_site_labels, issues): (Vec<_>, Vec<_>) = all_issues.into_iter().unzip();
            let (page_site_labels, pages): (Vec<_>, Vec<_>) = all_pages.into_iter().unzip();
            let _ = tx.send(UiMsg::MyWorkLoaded {
                issues,
                issue_site_labels,
                pages,
                page_site_labels,
            });
        });
    }

    pub fn request_remote_search(&self, ui: &mut UiState, query: &str) {
        let q = query.trim().to_string();
        if q.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Remote search: {q}"));
        let clients: Vec<(String, String, AtlassianClient)> = match &self.site_scope {
            SiteScope::Only(id) => {
                let c = self
                    .site_pool
                    .client_for(id)
                    .unwrap_or_else(|_| self.client.clone());
                vec![(id.clone(), self.site_display_label(id), c)]
            }
            SiteScope::All => {
                let enabled = self.site_pool.enabled_clients();
                if enabled.is_empty() {
                    vec![(
                        self.client.site_id().to_string(),
                        self.site_display_label(self.client.site_id()),
                        self.client.clone(),
                    )]
                } else {
                    enabled
                        .into_iter()
                        .map(|(s, c)| (s.id.clone(), s.display_name().to_string(), c))
                        .collect()
                }
            }
        };
        let show_pills = self.site_scope.is_all() && clients.len() > 1;
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let escaped = q.replace('"', "\\\"");
            let jql = format!("text ~ \"{escaped}\" ORDER BY updated DESC");
            let cql = format!("text ~ \"{escaped}\" AND type = page");
            let mut combined: Vec<(String, Option<String>, UnifiedSearchHit)> = Vec::new();
            let mut err_msg = None;
            for (site_id, site_label, client) in clients {
                let pill = show_pills.then_some(site_label);
                let (j_res, c_res) = tokio::join!(
                    client.search_issues_jql(&jql, 25, None, None),
                    client.search_confluence_cql(&cql, 25),
                );
                match j_res {
                    Ok(page) => {
                        for it in page.issues {
                            combined.push((
                                site_id.clone(),
                                pill.clone(),
                                UnifiedSearchHit {
                                    kind: crate::models::SearchHitKind::JiraIssue,
                                    id_or_key: it.key,
                                    container: it.project_key,
                                    title: it.summary,
                                    snippet: it.status.name.clone(),
                                    status_or_version: it.status.name,
                                    updated_at: it.updated_at,
                                    web_url: it.web_url,
                                },
                            ));
                        }
                    }
                    Err(e) => {
                        if err_msg.is_none() {
                            err_msg = Some(e.to_string());
                        }
                    }
                }
                match c_res {
                    Ok(pages) => {
                        for pg in pages {
                            combined.push((
                                site_id.clone(),
                                pill.clone(),
                                UnifiedSearchHit {
                                    kind: crate::models::SearchHitKind::ConfluencePage,
                                    id_or_key: pg.id,
                                    container: pg.space_key,
                                    title: pg.title,
                                    snippet: format!("v{}", pg.version_number),
                                    status_or_version: format!("v{}", pg.version_number),
                                    updated_at: pg.updated_at,
                                    web_url: pg.web_url,
                                },
                            ));
                        }
                    }
                    Err(e) => {
                        if err_msg.is_none() {
                            err_msg = Some(e.to_string());
                        }
                    }
                }
            }
            combined.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            let res = if combined.is_empty() {
                if let Some(err) = err_msg {
                    Err(err)
                } else {
                    Ok((Vec::new(), Vec::new(), Vec::new()))
                }
            } else {
                let mut hits = Vec::with_capacity(combined.len());
                let mut site_ids = Vec::with_capacity(combined.len());
                let mut labels = Vec::with_capacity(combined.len());
                for (sid, lbl, hit) in combined {
                    site_ids.push(sid);
                    labels.push(lbl);
                    hits.push(hit);
                }
                Ok((hits, site_ids, labels))
            };
            let _ = tx.send(UiMsg::RemoteSearchLoaded { query: q, hits: res });
        });
    }

    pub fn active_upload_target(&self, ui: &UiState) -> Option<UploadTarget> {
        if ui.active_tab == WorkspaceTab::Confluence {
            if let Some(page) = &self.active_page {
                return Some(UploadTarget::Page(page.summary.id.clone()));
            }
        }
        self.selected_issue_key(ui).map(UploadTarget::Issue)
    }

    pub fn request_upload_attachment(&self, ui: &mut UiState, overlay: &PathInputOverlay) {
        let path = PathBuf::from(overlay.path.text.trim());
        self.request_upload_file(ui, overlay.target.clone(), path);
    }

    pub fn request_upload_file(&self, ui: &mut UiState, target: UploadTarget, path: PathBuf) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Upload attachment");
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        match target {
            UploadTarget::Issue(key) => {
                tokio::spawn(async move {
                    let res = client
                        .upload_issue_attachment(&key, &path)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        issue_key: Some(key),
                        page_id: None,
                        result: res,
                    });
                });
            }
            UploadTarget::Page(id) => {
                tokio::spawn(async move {
                    let res = client
                        .upload_page_attachment(&id, &path)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        issue_key: None,
                        page_id: Some(id),
                        result: res,
                    });
                });
            }
        }
    }

    pub fn request_download_attachment(&self, ui: &mut UiState, att: AttachmentItem) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let dest = self.config.resolved_download_dir();
        ui.tasks.begin(format!("Download: {}", att.filename));
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = client
                .download_attachment(&att, &dest)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AttachmentDownloaded(res));
        });
    }

    pub fn request_load_preview_image(&self, ui: &UiState) {
        let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
            return;
        };
        if ov.protocol.is_pixel() {
            // Pixel protocols (Kitty / iTerm2 / Sixel) are decoded and synchronized by `tui.rs`.
            return;
        }
        let Some(att) = ov.current().cloned() else {
            return;
        };
        let (cols, rows) = ov.target_dimensions(ui.term_width, ui.term_height);
        let cache_key = img_preview::preview_cache_key(&att.id, cols, rows, ov.protocol);
        if self.image_cache.contains_key(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        let caps = ov.graphics_caps();
        let bg_rgb = match ui.theme.card_bg {
            ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
            _ => img_preview::DEFAULT_PREVIEW_BG_RGB,
        };
        tokio::spawn(async move {
            match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => {
                    let mime = (!att.mime_type.trim().is_empty()).then_some(att.mime_type.as_str());
                    let fname = (!att.filename.trim().is_empty()).then_some(att.filename.as_str());
                    if bitefoundry_html::sniff_content_kind(mime, fname, Some(&bytes))
                        == bitefoundry_html::ContentKind::Html
                    {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send(UiMsg::PreviewImageSniffedAsHtml {
                            title: att.filename.clone(),
                            source_url: att.content_url.clone(),
                            html,
                        });
                        return;
                    }
                    let res =
                        img_preview::decode_and_render_for_protocol(&bytes, cols, rows, bg_rgb, caps)
                            .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::ImageLoaded {
                        cache_key,
                        rendered: res,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ImageLoaded {
                        cache_key,
                        rendered: Err(e.to_string()),
                    });
                }
            }
        });
    }

    /// Downloads an `.html`/`.htm` attachment asynchronously and feeds it into `Overlay::HtmlPreview`.
    pub fn request_load_html_attachment(
        &self,
        ui: &mut UiState,
        title: String,
        att: AttachmentItem,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        let source_url = att.content_url.clone();
        ui.tasks.begin(format!("HTML: {}", att.filename));
        tokio::spawn(async move {
            let result = match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(err) => Err(err.to_string()),
            };
            let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                title,
                source_url,
                result,
            });
        });
    }

    /// Lazily starts `NativeHtmlServer` and serves the given HTML document for external browser viewing.
    pub fn request_serve_html_in_browser(
        &self,
        ui: &mut UiState,
        title: String,
        html: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Async runtime unavailable for native HTML server");
            return;
        }
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        ui.info("Serving HTML locally…");
        tokio::spawn(async move {
            let res = server
                .serve_html(&title, &html)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::HtmlServed(res));
        });
    }

    pub fn request_check_update(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Check for updates");
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = updater::check_or_apply_update(&repo, true, false)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::UpdateChecked(res));
        });
    }

    // ------------------------------------------------------------------
    // Event handlers
    // ------------------------------------------------------------------

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::SyncStarted => {
                self.syncing = true;
            }
            CoreEvent::SyncFinished { summary } => {
                self.syncing = false;
                self.sync_summary = summary.clone();
                if let Ok(counts) = self.storage.cache_counts() {
                    self.cache_counts = counts;
                }
                ui.tasks.finish("Sync", None);
                ui.success(summary);
            }
            CoreEvent::ProjectsUpdated(projects) => {
                self.projects = projects;
            }
            CoreEvent::BoardsUpdated(boards) => {
                self.boards = boards;
            }
            CoreEvent::BoardIssuesUpdated {
                board_id: _,
                sprint,
                issues,
            } => {
                self.board_issues = issues;
                if let Some(sp) = sprint {
                    if !self.sprints.iter().any(|s| s.id == sp.id) {
                        self.sprints.push(sp);
                    }
                }
            }
            CoreEvent::IssuesUpdated(issues) => {
                self.issues = issues;
            }
            CoreEvent::SpacesUpdated(spaces) => {
                self.spaces = spaces;
            }
            CoreEvent::SpacePagesUpdated { space_key, pages } => {
                self.root_pages_by_space.insert(space_key, pages);
            }
            CoreEvent::SyncError(err) => {
                self.syncing = false;
                ui.tasks.finish("Sync", Some(err.clone()));
                ui.error(format!("Sync error: {err}"));
                self.notify_desktop("Atlassian Sync Error", &err);
            }
        }
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::MeLoaded(Ok(me)) => {
                self.me = Some(me);
            }
            UiMsg::MeLoaded(Err(err)) => {
                ui.error(format!("Failed to verify user profile: {err}"));
            }
            UiMsg::BootstrapFinished(res) => match res {
                Ok(summary) => {
                    self.sync_summary = summary.clone();
                    ui.tasks.finish("Initial sync", None);
                    self.load_cached_initial();
                    ui.success(&summary);
                    self.notify_desktop("Atlassian Sync Complete", &summary);
                }
                Err(err) => {
                    ui.tasks.finish("Initial sync", Some(err.clone()));
                    ui.error(format!("Initial sync failed: {err}"));
                    self.notify_desktop("Atlassian Sync Failed", &err);
                }
            },
            UiMsg::BoardsLoaded(Ok(boards)) => {
                self.boards = boards;
            }
            UiMsg::BoardsLoaded(Err(e)) => ui.error(e),
            UiMsg::BoardDataLoaded {
                board_id,
                columns,
                sprints,
                issues,
            } => {
                ui.tasks.finish(&format!("Board #{board_id}"), None);
                if let Some(b) = self.boards.iter_mut().find(|b| b.id == board_id) {
                    if !columns.is_empty() {
                        b.columns = columns;
                    }
                }
                self.sprints = sprints;
                self.board_issues = issues;
                ui.board_ui.selected_col = 0;
                ui.board_ui.selected_row = 0;
            }
            UiMsg::BoardLoadFailed(err) => {
                ui.error(format!("Failed to load board: {err}"));
            }
            UiMsg::IssuesLoaded { issues, label } => {
                ui.tasks.finish(&format!("JQL: {label}"), None);
                self.issues = issues;
                ui.issue_ui.selected_issue = 0;
                if let Some(first) = self.issues.first().cloned() {
                    self.request_issue_detail(ui, &first.key);
                }
            }
            UiMsg::IssuesLoadFailed(err) => {
                ui.error(format!("JQL search failed: {err}"));
            }
            UiMsg::SavedFiltersLoaded(filters) => {
                let mut combined = builtin_filters();
                for f in filters {
                    if !combined.iter().any(|b| b.id == f.id) {
                        combined.push(f);
                    }
                }
                self.saved_filters = combined;
            }
            UiMsg::IssueDetailLoaded { key: _, detail } => match detail {
                Ok(d) => {
                    self.active_issue = Some(d);
                }
                Err(err) => ui.error(format!("Failed to load issue detail: {err}")),
            },
            UiMsg::IssueMutated { detail, toast } => {
                let key = detail.summary.key.clone();
                let summary = detail.summary.summary.clone();
                ui.tasks.finish(&format!("Assign {key} to me"), None);
                ui.tasks.finish(&format!("Assign {key}"), None);
                ui.tasks.finish(&format!("Update {key}"), None);
                ui.tasks.finish("Create issue", None);
                if let Some(it) = self.issues.iter_mut().find(|i| i.key == key) {
                    *it = detail.summary.clone();
                } else {
                    self.issues.insert(0, detail.summary.clone());
                }
                if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == key) {
                    *it = detail.summary.clone();
                }
                if let Some(it) = self.my_issues.iter_mut().find(|i| i.key == key) {
                    *it = detail.summary.clone();
                }
                self.active_issue = Some(detail);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &summary);
            }
            UiMsg::IssueTransitioned {
                key,
                to_status,
                toast,
            } => {
                ui.tasks.finish(&format!("Transition {key}"), None);
                self.apply_local_status_update(&key, &to_status);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &format!("Transitioned to {}", to_status.name));
            }
            UiMsg::IssueTransitionFailed {
                key,
                previous_status,
                error,
            } => {
                ui.tasks
                    .finish(&format!("Transition {key}"), Some(error.clone()));
                if let Some(prev) = previous_status {
                    self.apply_local_status_update(&key, &prev);
                }
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueAssignFailed {
                key,
                previous_assignee,
                task_label,
                error,
            } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                if let Some(prev) = previous_assignee {
                    self.apply_local_assignee_update(&key, prev);
                }
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueActionFailed(err) => {
                ui.error(&err);
                self.notify_desktop("Atlassian Jira Error", &err);
            }
            UiMsg::TransitionsLoaded {
                issue_key,
                transitions,
            } => {
                if let Some(Overlay::Transition(ov)) = ui.overlays.last_mut() {
                    if ov.issue_key == issue_key {
                        match transitions {
                            Ok(list) => ov.set_transitions(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::AssignableUsersLoaded { query, users } => {
                if let Some(Overlay::Assignee(ov)) = ui.overlays.last_mut() {
                    if ov.query.text == query {
                        match users {
                            Ok(list) => ov.set_users(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::CommentAdded {
                issue_key,
                page_id,
                optimistic_id,
                task_label,
                result,
            } => match result {
                Ok(()) => {
                    ui.tasks.finish(&task_label, None);
                    ui.success("Comment posted");
                    if let Some(k) = issue_key {
                        self.notify_desktop_issue(&k, "Comment posted");
                        self.request_issue_detail(ui, &k);
                    }
                    if let Some(pid) = page_id {
                        let space = self
                            .active_page
                            .as_ref()
                            .filter(|p| p.summary.id == pid)
                            .map(|p| p.summary.space_key.clone())
                            .unwrap_or_default();
                        let title = self
                            .active_page
                            .as_ref()
                            .filter(|p| p.summary.id == pid)
                            .map(|p| p.summary.title.clone())
                            .unwrap_or_else(|| format!("Page #{pid}"));
                        self.notify_desktop_page(&space, &format!("Comment posted on {title}"));
                        self.request_page_detail(ui, &pid);
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to post comment: {e}");
                    ui.tasks.finish(&task_label, Some(msg.clone()));
                    if let Some(ref opt_id) = optimistic_id {
                        if let Some(ref k) = issue_key {
                            if let Some(detail) = &mut self.active_issue {
                                if detail.summary.key == *k {
                                    detail.comments.retain(|c| c.id != *opt_id);
                                }
                            }
                            self.bump_local_comment_count(k, -1);
                        }
                        if let Some(ref pid) = page_id {
                            if let Some(page) = &mut self.active_page {
                                if page.summary.id == *pid {
                                    page.comments.retain(|c| c.id != *opt_id);
                                }
                            }
                        }
                    }
                    ui.error(&msg);
                    self.notify_desktop("Atlassian Comment Error", &msg);
                }
            },
            UiMsg::SpacesLoaded(Ok(spaces)) => self.spaces = spaces,
            UiMsg::SpacesLoaded(Err(e)) => ui.error(e),
            UiMsg::RootPagesLoaded { space_key, pages } => {
                ui.tasks.finish(&format!("Space {space_key}"), None);
                match pages {
                    Ok(list) => {
                        self.root_pages_by_space.insert(space_key, list);
                    }
                    Err(e) => ui.error(format!("Failed to load root pages: {e}")),
                }
            }
            UiMsg::ChildPagesLoaded {
                parent_id,
                children,
            } => match children {
                Ok(list) => {
                    self.page_children_map.insert(parent_id, list);
                }
                Err(e) => ui.error(format!("Failed to load child pages: {e}")),
            },
            UiMsg::PageDetailLoaded { page_id: _, detail } => match detail {
                Ok(d) => self.active_page = Some(d),
                Err(e) => ui.error(format!("Failed to load page: {e}")),
            },
            UiMsg::PageMutated { detail, toast } => {
                ui.tasks.finish("Update page", None);
                ui.tasks.finish("Create page", None);
                let sk = detail.summary.space_key.clone();
                let title = detail.summary.title.clone();
                if let Some(pid) = &detail.summary.parent_id {
                    self.page_children_map
                        .entry(pid.clone())
                        .or_default()
                        .retain(|p| p.id != detail.summary.id);
                    self.page_children_map
                        .entry(pid.clone())
                        .or_default()
                        .push(detail.summary.clone());
                    self.expanded_pages.insert(pid.clone());
                } else {
                    let roots = self.root_pages_by_space.entry(sk.clone()).or_default();
                    if let Some(existing) = roots.iter_mut().find(|p| p.id == detail.summary.id) {
                        *existing = detail.summary.clone();
                    } else {
                        roots.push(detail.summary.clone());
                    }
                }
                self.active_page = Some(detail);
                ui.success(&toast);
                self.notify_desktop_page(&sk, &title);
            }
            UiMsg::PageActionFailed(err) => {
                ui.error(&err);
                self.notify_desktop("Atlassian Confluence Error", &err);
            }
            UiMsg::MyWorkLoaded {
                issues,
                issue_site_labels,
                pages,
                page_site_labels,
            } => {
                ui.tasks.finish("Loading My Work", None);
                self.my_issues = issues;
                self.my_issue_site_labels = issue_site_labels;
                self.my_pages = pages;
                self.my_page_site_labels = page_site_labels;
            }
            UiMsg::RemoteSearchLoaded { query, hits } => {
                ui.tasks.finish(&format!("Remote search: {query}"), None);
                match hits {
                    Ok((list, site_ids, labels)) => {
                        self.search_hits = list;
                        self.search_hit_site_ids = site_ids;
                        self.search_hit_site_labels = labels;
                        ui.search_ui.is_remote = true;
                        ui.search_ui.selected_hit = 0;
                    }
                    Err(e) => ui.error(format!("Remote search failed: {e}")),
                }
            }
            UiMsg::CloudSitesDiscovered(res) => {
                ui.tasks.finish("Discover Cloud sites", None);
                match res {
                    Ok(profiles) => {
                        let count = profiles.len();
                        for profile in profiles {
                            let _ = self.storage.upsert_site(&SiteRecord {
                                site_id: profile.id.clone(),
                                name: profile.display_name().to_string(),
                                site_url: profile.site_url.clone(),
                                deployment: profile.deployment.as_str().to_string(),
                                enabled: profile.enabled,
                                last_synced_at: None,
                                last_error: None,
                            });
                            self.config.upsert_site(profile);
                        }
                        self.rebuild_site_pool();
                        let _ = self.save_config();
                        self.load_cached_initial();
                        ui.success(format!("Discovered {count} Cloud site(s)"));
                    }
                    Err(e) => {
                        ui.error(format!("Cloud site discovery failed: {e}"));
                    }
                }
            }
            UiMsg::MultiSiteSyncFinished(outcome) => {
                self.syncing = false;
                self.sync_summary = outcome.summary.clone();
                ui.tasks.finish("Sync all sites", None);
                self.load_cached_initial();
                ui.success(outcome.summary);
            }
            UiMsg::ImageLoaded {
                cache_key,
                rendered,
            } => match rendered {
                Ok(img) => {
                    self.image_cache.insert(cache_key, img);
                }
                Err(err) => {
                    if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() {
                        ov.error = Some(err);
                    }
                }
            },
            UiMsg::HtmlAttachmentLoaded {
                title,
                source_url,
                result,
            } => {
                let task_name = title
                    .rsplit(" — ")
                    .next()
                    .unwrap_or(&title);
                ui.tasks.finish(&format!("HTML: {task_name}"), None);
                if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                    if !source_url.is_empty() {
                        ov.source_url = source_url;
                    }
                    ov.set_loaded(result);
                } else if let Ok(html) = result {
                    let mut ov = HtmlPreviewOverlay::new(title, html);
                    ov.source_url = source_url;
                    ui.push_overlay(Overlay::HtmlPreview(ov));
                } else if let Err(err) = result {
                    ui.error(format!("Failed to load HTML attachment: {err}"));
                }
            }
            UiMsg::PreviewImageSniffedAsHtml {
                title,
                source_url,
                html,
            } => {
                if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                    ui.pop_overlay();
                }
                let mut ov = HtmlPreviewOverlay::new(title, html);
                ov.source_url = source_url;
                ui.push_overlay(Overlay::HtmlPreview(ov));
                ui.info("Detected HTML content — opened in interactive HTML Preview");
            }
            UiMsg::HtmlServed(res) => match res {
                Ok(url) => {
                    self.last_served_html_url = Some(url.clone());
                    if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                        ov.last_served_url = Some(url.clone());
                        if let Some(sess) = ov.session_mut() {
                            sess.last_status = Some(format!("Served at {url}"));
                        }
                    }
                    if !cfg!(test) {
                        if let Err(err) = open::that(&url) {
                            ui.error(format!("Served at {url} (browser open failed: {err})"));
                            return;
                        }
                    }
                    ui.success(format!("Opened HTML in browser: {url}"));
                }
                Err(err) => {
                    ui.error(format!("Failed to serve HTML locally: {err}"));
                }
            },
            UiMsg::AttachmentDownloaded(res) => match res {
                Ok(file) => {
                    ui.tasks.finish(&format!("Download: {}", file.filename), None);
                    ui.success(format!("Saved: {}", file.path.display()));
                    self.notify_desktop("Atlassian Download", &format!("Saved {}", file.filename));
                }
                Err(e) => {
                    ui.error(format!("Download failed: {e}"));
                    self.notify_desktop("Atlassian Download Failed", &e);
                }
            },
            UiMsg::AttachmentUploaded {
                issue_key,
                page_id,
                result,
            } => match result {
                Ok(_) => {
                    ui.tasks.finish("Upload attachment", None);
                    ui.success("Attachment uploaded");
                    self.notify_desktop("Atlassian Upload", "Attachment uploaded");
                    if let Some(k) = issue_key {
                        self.request_issue_detail(ui, &k);
                    }
                    if let Some(pid) = page_id {
                        self.request_page_detail(ui, &pid);
                    }
                }
                Err(e) => {
                    ui.tasks.finish("Upload attachment", Some(e.clone()));
                    ui.error(format!("Upload failed: {e}"));
                    self.notify_desktop("Atlassian Upload Failed", &e);
                }
            },
            UiMsg::UpdateChecked(res) => {
                ui.tasks.finish("Check for updates", None);
                match res {
                    Ok(info) => {
                        if info.update_available {
                            let msg = format!(
                                "New version {} available (current {})",
                                info.latest_version, info.current_version
                            );
                            ui.info(&msg);
                            self.notify_desktop("Atlassian Update Available", &msg);
                        } else {
                            ui.success(format!("Already up to date ({})", info.current_version));
                        }
                        ui.directory_ui.last_update = Some(info);
                    }
                    Err(e) => {
                        ui.error(format!("Update check failed: {e}"));
                        self.notify_desktop("Atlassian Update Check Failed", &e);
                    }
                }
            }
        }
    }
}

pub fn builtin_filters() -> Vec<SavedFilter> {
    vec![
        SavedFilter {
            id: "builtin:my-open".to_string(),
            name: "My Open Issues".to_string(),
            jql: AtlassianClient::my_open_issues_jql().to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:reported".to_string(),
            name: "Reported by Me".to_string(),
            jql: "reporter = currentUser() ORDER BY updated DESC".to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:recent".to_string(),
            name: "Recently Updated".to_string(),
            jql: "ORDER BY updated DESC".to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:all-open".to_string(),
            name: "All Open Issues".to_string(),
            jql: "statusCategory != Done ORDER BY updated DESC".to_string(),
            builtin: true,
        },
    ]
}

/// Groups issues into Kanban columns derived from `board.columns` or default 3-category columns.
pub fn build_kanban_columns_for_board(
    board: Option<&BoardSummary>,
    issues: &[IssueSummary],
) -> Vec<KanbanColumn> {
    let mut columns: Vec<KanbanColumn> = match board {
        Some(b) if !b.columns.is_empty() => b
            .columns
            .iter()
            .map(|c| KanbanColumn {
                title: c.name.clone(),
                category: c.category_hint,
                issues: Vec::new(),
            })
            .collect(),
        _ => vec![
            KanbanColumn {
                title: "To Do".to_string(),
                category: StatusCategory::Todo,
                issues: Vec::new(),
            },
            KanbanColumn {
                title: "In Progress".to_string(),
                category: StatusCategory::InProgress,
                issues: Vec::new(),
            },
            KanbanColumn {
                title: "Done".to_string(),
                category: StatusCategory::Done,
                issues: Vec::new(),
            },
        ],
    };

    let board_cols = board.map(|b| b.columns.as_slice()).unwrap_or(&[]);

    for issue in issues {
        let mut target_idx: Option<usize> = None;
        if !board_cols.is_empty() {
            target_idx = board_cols.iter().position(|col| {
                (!issue.status.id.is_empty() && col.status_ids.iter().any(|id| id == &issue.status.id))
                    || col
                        .status_names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&issue.status.name))
                    || col.name.eq_ignore_ascii_case(&issue.status.name)
            });
        }
        if target_idx.is_none() {
            target_idx = columns
                .iter()
                .position(|c| c.category == issue.status.category);
        }
        let idx = target_idx.unwrap_or(0);
        if let Some(col) = columns.get_mut(idx) {
            col.issues.push(issue.clone());
        }
    }

    columns
}

/// Flattens the hierarchical Confluence page tree respecting `expanded` page IDs.
pub fn flatten_page_tree(
    roots: &[PageSummary],
    children_map: &HashMap<String, Vec<PageSummary>>,
    expanded: &HashSet<String>,
) -> Vec<TreeRow> {
    fn walk(
        nodes: &[PageSummary],
        depth: usize,
        children_map: &HashMap<String, Vec<PageSummary>>,
        expanded: &HashSet<String>,
        visited: &mut HashSet<String>,
        out: &mut Vec<TreeRow>,
    ) {
        for page in nodes {
            if !visited.insert(page.id.clone()) {
                continue;
            }
            let children = children_map.get(&page.id);
            let has_children =
                page.has_children || children.is_some_and(|c| !c.is_empty());
            let is_expanded = expanded.contains(&page.id);
            out.push(TreeRow {
                page: page.clone(),
                depth,
                expanded: is_expanded,
                has_children,
            });
            if is_expanded {
                if let Some(kids) = children {
                    walk(kids, depth + 1, children_map, expanded, visited, out);
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk(roots, 0, children_map, expanded, &mut visited, &mut out);
    out
}

fn scan_issue_keys(text: &str) -> Vec<String> {
    let Ok(re) = Regex::new(r"\b([A-Z][A-Z0-9]{1,9}-\d+)\b") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str().to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::IssueStatus;

    fn sample_issue(key: &str, status_name: &str, cat: StatusCategory) -> IssueSummary {
        IssueSummary {
            key: key.to_string(),
            summary: format!("Summary for {key}"),
            status: IssueStatus {
                id: status_name.to_lowercase(),
                name: status_name.to_string(),
                category: cat,
            },
            ..Default::default()
        }
    }

    #[test]
    fn persona_defaults_to_expected_workspace_tab() {
        let cfg = AppConfig::default();
        let jira_ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        let conf_ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);
        assert_eq!(jira_ui.active_tab, WorkspaceTab::Board);
        assert_eq!(jira_ui.active_tab.index(), 0);
        assert_eq!(conf_ui.active_tab, WorkspaceTab::Confluence);
        assert_eq!(conf_ui.active_tab.index(), 3);
    }

    #[test]
    fn kanban_groups_issues_into_board_columns_and_default_categories() {
        let board = BoardSummary {
            id: 1,
            name: "Sprint Board".into(),
            board_type: "scrum".into(),
            project_key: Some("ATL".into()),
            project_name: None,
            columns: vec![
                BoardColumn {
                    name: "Backlog".into(),
                    status_ids: vec![],
                    status_names: vec!["To Do".into()],
                    category_hint: StatusCategory::Todo,
                },
                BoardColumn {
                    name: "In Review".into(),
                    status_ids: vec![],
                    status_names: vec!["In Review".into()],
                    category_hint: StatusCategory::InProgress,
                },
                BoardColumn {
                    name: "Shipped".into(),
                    status_ids: vec![],
                    status_names: vec!["Done".into()],
                    category_hint: StatusCategory::Done,
                },
            ],
        };
        let issues = vec![
            sample_issue("ATL-1", "To Do", StatusCategory::Todo),
            sample_issue("ATL-2", "In Review", StatusCategory::InProgress),
            sample_issue("ATL-3", "Done", StatusCategory::Done),
        ];

        let cols = build_kanban_columns_for_board(Some(&board), &issues);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].issues[0].key, "ATL-1");
        assert_eq!(cols[1].issues[0].key, "ATL-2");
        assert_eq!(cols[2].issues[0].key, "ATL-3");
    }

    #[test]
    fn confluence_tree_flattens_and_expands_children() {
        let root = PageSummary {
            id: "100".into(),
            title: "Architecture".into(),
            has_children: true,
            ..Default::default()
        };
        let child = PageSummary {
            id: "101".into(),
            parent_id: Some("100".into()),
            title: "TUI Design".into(),
            has_children: false,
            ..Default::default()
        };
        let mut children_map = HashMap::new();
        children_map.insert("100".to_string(), vec![child]);

        let mut expanded = HashSet::new();
        let collapsed = flatten_page_tree(&[root.clone()], &children_map, &expanded);
        assert_eq!(collapsed.len(), 1);
        assert!(!collapsed[0].expanded);
        assert!(collapsed[0].has_children);

        expanded.insert("100".to_string());
        let open = flatten_page_tree(&[root], &children_map, &expanded);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].depth, 0);
        assert!(open[0].expanded);
        assert_eq!(open[1].depth, 1);
        assert_eq!(open[1].page.title, "TUI Design");
    }

    #[tokio::test]
    async fn cross_link_history_stack_pushes_and_pops() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        app.active_issue = Some(IssueDetail {
            summary: sample_issue("ATL-10", "In Progress", StatusCategory::InProgress),
            ..Default::default()
        });

        app.jump_to_cross_ref(
            &mut ui,
            CrossRefTarget::ConfluencePage {
                page_id: "555".into(),
                title: Some("Design Doc".into()),
            },
        );
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);
        assert_eq!(app.nav_history.len(), 1);
        assert!(matches!(
            app.nav_history[0],
            CrossRefTarget::JiraIssue { ref key, .. } if key == "ATL-10"
        ));

        assert!(app.pop_nav_history(&mut ui));
        assert_eq!(ui.active_tab, WorkspaceTab::Issues);
        assert!(app.nav_history.is_empty());
        assert!(!app.pop_nav_history(&mut ui));
    }

    #[test]
    fn ui_state_initializes_all_preferences_from_app_config() {
        let cfg = AppConfig {
            theme: crate::config::ThemePreset::Light,
            density: DensityMode::Compact,
            inline_images: false,
            show_nav_rail: false,
            notify: false,
            image_protocol: ConfigImageProtocol::Sixel,
            ..Default::default()
        };
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        assert_eq!(ui.density, DensityMode::Compact);
        assert!(!ui.inline_images);
        assert!(!ui.show_nav_rail);
        assert!(!ui.notify);
        assert_eq!(
            ui.graphics_caps.protocol,
            img_preview::TerminalImageProtocol::Sixel
        );
        assert_eq!(ui.theme.bg_sidebar, Theme::daylight_light().bg_sidebar);
    }

    #[tokio::test]
    async fn optimistic_transition_assign_and_comment_rollback_on_failure() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        let initial_status = IssueStatus {
            id: "1".into(),
            name: "To Do".into(),
            category: StatusCategory::Todo,
        };
        let issue = IssueSummary {
            key: "ATL-99".into(),
            summary: "Optimistic rollback test".into(),
            status: initial_status.clone(),
            ..Default::default()
        };
        app.board_issues.push(issue.clone());
        app.issues.push(issue.clone());
        app.active_issue = Some(IssueDetail {
            summary: issue,
            ..Default::default()
        });
        app.me = Some(UserSummary {
            id: "u1".into(),
            display_name: "Alice".into(),
            ..Default::default()
        });

        // 1. Optimistic transition -> rollback on failure
        app.request_transition_issue(
            &mut ui,
            "ATL-99",
            IssueTransition {
                id: "31".into(),
                name: "Done".into(),
                to_status: IssueStatus {
                    id: "3".into(),
                    name: "Done".into(),
                    category: StatusCategory::Done,
                },
            },
        );
        assert_eq!(
            app.active_issue.as_ref().unwrap().summary.status.name,
            "Done"
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueTransitionFailed {
                key: "ATL-99".into(),
                previous_status: Some(initial_status),
                error: "HTTP 500".into(),
            },
        );
        assert_eq!(
            app.active_issue.as_ref().unwrap().summary.status.name,
            "To Do"
        );

        // 2. Optimistic assign to me -> rollback on failure
        app.request_assign_to_me(&mut ui, "ATL-99");
        assert_eq!(
            app.active_issue
                .as_ref()
                .unwrap()
                .summary
                .assignee
                .as_ref()
                .map(|u| u.display_name.as_str()),
            Some("Alice")
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueAssignFailed {
                key: "ATL-99".into(),
                previous_assignee: Some(None),
                task_label: "Assign ATL-99 to me".into(),
                error: "HTTP 403".into(),
            },
        );
        assert!(app.active_issue.as_ref().unwrap().summary.assignee.is_none());

        // 3. Optimistic comment -> rollback on failure
        app.request_add_comment(
            &mut ui,
            CommentTarget::Issue("ATL-99".into()),
            "Testing optimistic comment".into(),
        );
        assert_eq!(app.active_issue.as_ref().unwrap().comments.len(), 1);
        assert_eq!(app.active_issue.as_ref().unwrap().summary.comment_count, 1);
        let opt_id = app.active_issue.as_ref().unwrap().comments[0].id.clone();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::CommentAdded {
                issue_key: Some("ATL-99".into()),
                page_id: None,
                optimistic_id: Some(opt_id),
                task_label: "Comment on ATL-99".into(),
                result: Err("network error".into()),
            },
        );
        assert!(app.active_issue.as_ref().unwrap().comments.is_empty());
        assert_eq!(app.active_issue.as_ref().unwrap().summary.comment_count, 0);
    }
}
