//! Application & UI state, Kanban grouping, Confluence page tree, cross-app history, and async tasks.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use regex::Regex;
use tokio::sync::mpsc;

use crate::{
    adf,
    atlassian::{
        client::is_not_found_error, collect::SYNC_BOARD_ISSUE_CAP as BOARD_ISSUE_VIEW_CAP,
        AtlassianClient, SiteClientPool,
    },
    auth::{
        fetch_accessible_resources, run_oauth_loopback_login, AuthManager, OAuthLoopbackOptions,
        StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{
        AppConfig, DensityMode, DeploymentKind, DraftStore,
        ImageProtocolPreference as ConfigImageProtocol, Persona, SiteProfile,
    },
    img_preview::{self, DecodedImageCache, RenderedTerminalImage, TerminalGraphicsCaps},
    models::{
        AttachmentItem, BoardColumn, BoardSummary, ConfluenceComment, CoreEvent, CrossRefTarget,
        DownloadedFile, IssueComment, IssueDetail, IssueStatus, IssueSummary, IssueTransition,
        OpenItemTarget, OutgoingIssue, OutgoingPage, PageDetail, PageSummary, PageUpdate,
        ProjectSummary, SavedFilter, SiteScope, SpaceSummary, SprintSummary, StatusCategory,
        UnifiedSearchHit, UpdateCheckResult, UserSummary,
    },
    storage::{CacheCounts, SiteRecord, Storage, DEFAULT_SITE_ID},
    sync::{self, MultiSiteSyncOutcome, SiteSyncTarget, SyncController},
    ui::{
        directory_view::ConnectedSiteRow,
        filter::space_match_score,
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
    pub board_scroll: usize,
    pub sprint_scroll: usize,
    /// `0` = boards list, `1` = sprints list inside left nav.
    pub nav_section: usize,
    pub selected_col: usize,
    pub selected_row: usize,
    pub col_scroll: usize,
    pub drawer_open: bool,
    pub detail_scroll: u16,
    pub detail_max_scroll: u16,
    /// Set once the user picks a board, so reloads keep their selection.
    pub user_moved_board: bool,
}

#[derive(Debug, Clone)]
pub struct IssueUiState {
    pub jql: TextBuffer,
    pub local_filter: TextBuffer,
    pub local_filter_active: bool,
    pub selected_filter: usize,
    pub selected_issue: usize,
    pub filter_scroll: usize,
    pub issue_scroll: usize,
    /// `0` = saved filters, `1` = issue list inside left nav.
    pub nav_section: usize,
    pub detail_scroll: u16,
    pub detail_max_scroll: u16,
}

impl Default for IssueUiState {
    fn default() -> Self {
        Self {
            jql: TextBuffer::from_text("ORDER BY updated DESC"),
            local_filter: TextBuffer::new(),
            local_filter_active: false,
            selected_filter: 0,
            selected_issue: 0,
            filter_scroll: 0,
            issue_scroll: 0,
            nav_section: 1,
            detail_scroll: 0,
            detail_max_scroll: 0,
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
    /// Index into `AppState::spaces` (not into the filtered rows).
    pub selected_space: usize,
    /// First SPACES pane row on screen; follows `selected_space` when drawn.
    pub space_scroll: usize,
    /// Incremental SPACES filter (`/` in the SPACES section; leading `~` = personal).
    pub space_filter: TextBuffer,
    pub space_filter_active: bool,
    /// Set once the user picks a space, so reloads keep their selection.
    pub user_moved_space: bool,
    pub selected_tree_row: usize,
    /// `0` = Space selector, `1` = Page tree inside left nav.
    pub nav_section: usize,
    pub toc_open: bool,
    pub selected_toc: usize,
    pub toc_scroll: usize,
    pub reader_scroll: u16,
    pub reader_max_scroll: u16,
}

/// One row of the SPACES pane, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpacePaneRow {
    /// Collapsible site group header (multi-site All Sites mode) and its listed spaces.
    SiteHeader {
        site_id: String,
        spaces: usize,
        collapsed: bool,
    },
    /// Index into `AppState::spaces`.
    Space(usize),
}

/// `UiState::collapsed_site_groups` key of a site's group in the SPACES pane.
pub fn space_group_key(site_id: &str) -> String {
    format!("wiki:{site_id}")
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
    pub selected_site: usize,
    pub site_scroll: usize,
    pub last_update: Option<UpdateCheckResult>,
}

#[derive(Debug)]
pub enum UiMsg {
    MeLoaded(Result<UserSummary, String>),
    BootstrapFinished(Result<String, String>),
    BoardsLoaded(Result<Vec<BoardSummary>, String>),
    BoardDataLoaded {
        seq: u64,
        site_id: String,
        board_id: i64,
        requested_sprint_id: Option<i64>,
        columns: Vec<BoardColumn>,
        sprints: Vec<SprintSummary>,
        issues: Vec<IssueSummary>,
    },
    BoardLoadFailed {
        seq: u64,
        site_id: String,
        board_id: i64,
        error: String,
    },
    IssuesLoaded {
        seq: u64,
        site_id: String,
        issues: Vec<IssueSummary>,
        label: String,
    },
    IssuesLoadFailed {
        seq: u64,
        label: String,
        error: String,
    },
    SavedFiltersLoaded(Vec<SavedFilter>),
    IssueDetailLoaded {
        seq: Option<u64>,
        site_id: String,
        key: String,
        detail: Result<IssueDetail, String>,
        not_found: bool,
    },
    IssueMutated {
        site_id: String,
        detail: IssueDetail,
        toast: String,
        clear_draft_key: Option<String>,
    },
    IssueTransitioned {
        site_id: String,
        key: String,
        to_status: IssueStatus,
        toast: String,
    },
    IssueTransitionFailed {
        site_id: String,
        key: String,
        previous_status: Option<IssueStatus>,
        error: String,
    },
    IssueAssignFailed {
        site_id: String,
        key: String,
        previous_assignee: Option<Option<UserSummary>>,
        task_label: String,
        error: String,
    },
    IssueActionFailed {
        task_label: String,
        error: String,
    },
    TransitionsLoaded {
        seq: u64,
        site_id: String,
        issue_key: String,
        transitions: Result<Vec<IssueTransition>, String>,
    },
    AssignableUsersLoaded {
        seq: u64,
        site_id: String,
        query: String,
        users: Result<Vec<UserSummary>, String>,
    },
    CommentAdded {
        site_id: String,
        issue_key: Option<String>,
        page_id: Option<String>,
        optimistic_id: Option<String>,
        task_label: String,
        clear_draft_key: Option<String>,
        result: Result<(), String>,
    },
    SpacesLoaded(Result<Vec<SpaceSummary>, String>),
    RootPagesLoaded {
        site_id: String,
        space_key: String,
        pages: Result<Vec<PageSummary>, String>,
    },
    ChildPagesLoaded {
        site_id: String,
        parent_id: String,
        task_label: String,
        children: Result<Vec<PageSummary>, String>,
    },
    PageDetailLoaded {
        seq: Option<u64>,
        site_id: String,
        page_id: String,
        detail: Result<PageDetail, String>,
        not_found: bool,
    },
    PageMutated {
        site_id: String,
        detail: PageDetail,
        toast: String,
        clear_draft_key: Option<String>,
    },
    PageActionFailed {
        task_label: String,
        error: String,
    },
    MyWorkLoaded {
        seq: u64,
        issues: Vec<IssueSummary>,
        issue_site_ids: Vec<String>,
        issue_site_labels: Vec<Option<String>>,
        pages: Vec<PageSummary>,
        page_site_ids: Vec<String>,
        page_site_labels: Vec<Option<String>>,
    },
    AttachmentsRefreshed {
        site_id: String,
        issue_key: Option<String>,
        page_id: Option<String>,
        attachments: Result<Vec<AttachmentItem>, String>,
    },
    CommentsRefreshed {
        site_id: String,
        issue_key: Option<String>,
        page_id: Option<String>,
        issue_comments: Option<Result<Vec<IssueComment>, String>>,
        issue_comment_total: Option<usize>,
        page_comments: Option<Result<Vec<ConfluenceComment>, String>>,
    },
    RemoteSearchLoaded {
        seq: u64,
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
        task_label: String,
        result: Result<String, String>,
    },
    PreviewImageSniffedAsHtml {
        title: String,
        source_url: String,
        html: String,
    },
    HtmlServed(Result<String, String>),
    AttachmentDownloaded {
        task_label: String,
        result: Result<DownloadedFile, String>,
    },
    AttachmentUploaded {
        site_id: String,
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
    pub pending_pixel_graphics: Vec<crate::ui::overlays::image_preview::PendingPixelGraphic>,
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
    /// Scroll offset for the Help overlay on short/narrow terminals.
    pub help_scroll: u16,
    /// Max scroll computed during the last Help overlay draw.
    pub help_max_scroll: u16,
    /// Last left-click timestamp & target for double-click detection.
    pub last_click: Option<(Instant, crate::ui::hitmap::HitTarget)>,
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
            pending_pixel_graphics: Vec::new(),
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
            help_scroll: 0,
            help_max_scroll: 0,
            last_click: None,
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
            || (self.active_tab == WorkspaceTab::Confluence
                && self.confluence_ui.space_filter_active)
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

const FOCUS_CHECK_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
struct InFlightIssueDetail {
    site_id: String,
    key: String,
    target_updated_at: Option<String>,
    seq: u64,
}

#[derive(Debug, Clone)]
struct InFlightPageDetail {
    site_id: String,
    page_id: String,
    target_version: Option<i32>,
    seq: u64,
}

/// One Back (`Backspace`) step: the item that was open and the site it was open on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavEntry {
    pub site_id: Option<String>,
    pub target: CrossRefTarget,
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
    /// Site of each row of `projects` (parallel vector, same length).
    pub project_site_ids: Vec<String>,
    pub boards: Vec<BoardSummary>,
    pub board_site_ids: Vec<String>,
    pub board_issue_site_id: Option<String>,
    pub sprints: Vec<SprintSummary>,
    pub board_issues: Vec<IssueSummary>,

    pub saved_filters: Vec<SavedFilter>,
    pub issues: Vec<IssueSummary>,
    pub issue_site_ids: Vec<String>,
    pub active_issue: Option<IssueDetail>,

    pub spaces: Vec<SpaceSummary>,
    pub space_site_ids: Vec<String>,
    /// Keyed by `(site, space key)`: two sites can share a space key.
    pub root_pages_by_space: HashMap<SiteKey, Vec<PageSummary>>,
    /// Keyed by `(site, parent page id)`: page ids are only unique per site.
    pub page_children_map: HashMap<SiteKey, Vec<PageSummary>>,
    pub expanded_pages: HashSet<SiteKey>,
    pub active_page: Option<PageDetail>,

    pub my_issues: Vec<IssueSummary>,
    pub my_issue_site_ids: Vec<String>,
    pub my_issue_site_labels: Vec<Option<String>>,
    pub my_pages: Vec<PageSummary>,
    pub my_page_site_ids: Vec<String>,
    pub my_page_site_labels: Vec<Option<String>>,

    pub search_hits: Vec<UnifiedSearchHit>,
    pub search_hit_site_ids: Vec<String>,
    pub search_hit_site_labels: Vec<Option<String>>,
    pub cache_counts: CacheCounts,

    /// Cross-App navigation stack (`Jira <-> Confluence`).
    pub nav_history: Vec<NavEntry>,

    /// Decoded image cache (`"{att_id}:{cols}x{rows}[:{protocol}]"` -> `RenderedTerminalImage`).
    pub image_cache: DecodedImageCache,
    pub pixel_image_cache: HashMap<String, RenderedTerminalImage>,

    /// Lazy localhost Axum server for viewing HTML documents in the default browser (`0 MB` idle).
    pub html_server: bitefoundry_html::NativeHtmlServer,
    /// Last localhost URL returned by `html_server.serve_html`.
    pub last_served_html_url: Option<String>,

    /// Persistent unsent drafts (`drafts.json`) for comments, issues, and Confluence pages.
    pub drafts: DraftStore,

    /// Optional file path overrides for isolated persistence in tests.
    pub config_path_override: Option<PathBuf>,
    pub drafts_path_override: Option<PathBuf>,

    /// Monotonic sequence counters guarding against out-of-order async responses.
    pub issue_detail_req_seq: u64,
    pub page_detail_req_seq: u64,
    pub board_data_req_seq: u64,
    pub issues_req_seq: u64,
    pub my_work_req_seq: u64,
    pub remote_search_req_seq: u64,
    pub transitions_req_seq: AtomicU64,
    pub assignable_users_req_seq: AtomicU64,
    /// True when `self.issues` shows a user-selected custom JQL, saved filter, or project filter
    /// so periodic 45s background `IssuesUpdated` events never overwrite it.
    pub issues_custom_query: bool,
    pub active_jql_label: Option<String>,
    pub active_jql_site_id: Option<String>,
    /// Sprint ID currently loaded into `self.board_issues` (guards against active-sprint overwrites).
    active_board_sprint_id: Option<i64>,
    /// Timestamp of the last live My Work fetch this session.
    last_my_work_refresh_at: Option<Instant>,
    /// Site IDs for currently opened issue / page details.
    pub active_issue_site_id: Option<String>,
    pub active_page_site_id: Option<String>,
    /// Canonical `(site_id, key/page_id)` target currently open or loading on screen.
    pub active_issue_target: Option<(String, String)>,
    pub active_page_target: Option<(String, String)>,
    /// Actual `detail_updated_at` / `detail_version` of the currently loaded detail body
    /// (prevents reconciled summary columns in cached detail from masking a changed revision).
    active_issue_detail_updated_at: Option<String>,
    active_page_detail_version: Option<i32>,
    /// In-flight remote detail request trackers for cross-path deduplication.
    in_flight_issue_detail: Option<InFlightIssueDetail>,
    in_flight_page_detail: Option<InFlightPageDetail>,
    /// Last open-item target sent to the background sync engine.
    last_watched_open_item: Option<OpenItemTarget>,
    /// Last focus-gain check timestamp for short OS-burst debounce.
    last_focus_check_at: Option<Instant>,
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
            project_site_ids: Vec::new(),
            boards: Vec::new(),
            board_site_ids: Vec::new(),
            board_issue_site_id: None,
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
            my_issue_site_ids: Vec::new(),
            my_issue_site_labels: Vec::new(),
            my_pages: Vec::new(),
            my_page_site_ids: Vec::new(),
            my_page_site_labels: Vec::new(),
            search_hits: Vec::new(),
            search_hit_site_ids: Vec::new(),
            search_hit_site_labels: Vec::new(),
            cache_counts: CacheCounts::default(),
            nav_history: Vec::new(),
            image_cache: DecodedImageCache::new(),
            pixel_image_cache: HashMap::new(),
            html_server: bitefoundry_html::NativeHtmlServer::new(),
            last_served_html_url: None,
            drafts,
            config_path_override: None,
            drafts_path_override: None,
            issue_detail_req_seq: 0,
            page_detail_req_seq: 0,
            board_data_req_seq: 0,
            issues_req_seq: 0,
            my_work_req_seq: 0,
            remote_search_req_seq: 0,
            transitions_req_seq: AtomicU64::new(0),
            assignable_users_req_seq: AtomicU64::new(0),
            issues_custom_query: false,
            active_jql_label: None,
            active_jql_site_id: None,
            active_board_sprint_id: None,
            last_my_work_refresh_at: None,
            active_issue_site_id: None,
            active_page_site_id: None,
            active_issue_target: None,
            active_page_target: None,
            active_issue_detail_updated_at: None,
            active_page_detail_version: None,
            in_flight_issue_detail: None,
            in_flight_page_detail: None,
            last_watched_open_item: None,
            last_focus_check_at: None,
        }
    }

    /// Resolves the canonical `(site_id, AtlassianClient)` for the current `site_scope`
    /// (falling back to `self.client`'s bound site).
    pub fn effective_site_id_and_client(&self) -> (String, AtlassianClient) {
        if let Some(sid) = self.site_scope.site_id() {
            if sid.eq_ignore_ascii_case(self.client.site_id()) {
                return (self.client.site_id().to_string(), self.client.clone());
            }
            if let Ok(c) = self.site_pool.client_for(sid) {
                return (c.site_id().to_string(), c);
            }
        }
        (self.client.site_id().to_string(), self.client.clone())
    }

    pub(crate) fn site_id_and_client_for(
        &self,
        explicit_site: Option<&str>,
    ) -> (String, AtlassianClient) {
        if let Some(sid) = explicit_site.map(str::trim).filter(|s| !s.is_empty()) {
            if sid.eq_ignore_ascii_case(self.client.site_id()) {
                return (self.client.site_id().to_string(), self.client.clone());
            }
            if let Ok(c) = self.site_pool.client_for(sid) {
                return (c.site_id().to_string(), c);
            }
        }
        self.effective_site_id_and_client()
    }

    /// Site id carried by a list event; an empty one (legacy daemon) means the effective site.
    fn event_site_or_effective(&self, site_id: String) -> String {
        if site_id.trim().is_empty() {
            self.effective_site_id_and_client().0
        } else {
            site_id
        }
    }

    fn reload_projects_all_sites(&mut self) {
        if let Ok(projs) = self.storage.list_projects_all_sites() {
            let filtered: Vec<(String, ProjectSummary)> = projs
                .into_iter()
                .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                .collect();
            self.project_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
            self.projects = filtered.into_iter().map(|(_, p)| p).collect();
        }
    }

    /// Site of the Directory project row `row`, falling back to the effective site.
    pub fn project_row_site(&self, row: usize) -> String {
        self.project_site_ids
            .get(row)
            .cloned()
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Site of the space row `row` (index into `spaces`), falling back to the effective site.
    pub fn space_row_site(&self, row: usize) -> String {
        self.space_site_ids
            .get(row)
            .cloned()
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Removes `key` of `site_id` from My Work, keeping the parallel site vectors aligned.
    pub(crate) fn remove_my_issue(&mut self, site_id: &str, key: &str) {
        let mut idx = 0usize;
        while idx < self.my_issues.len() {
            let matches_site = self
                .my_issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.my_issues[idx].key == key {
                self.my_issues.remove(idx);
                if idx < self.my_issue_site_ids.len() {
                    self.my_issue_site_ids.remove(idx);
                }
                if idx < self.my_issue_site_labels.len() {
                    self.my_issue_site_labels.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
    }

    /// Removes page `page_id` of `site_id` from My Work, keeping the parallel site vectors aligned.
    pub(crate) fn remove_my_page(&mut self, site_id: &str, page_id: &str) {
        let mut idx = 0usize;
        while idx < self.my_pages.len() {
            let matches_site = self
                .my_page_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.my_pages[idx].id == page_id {
                self.my_pages.remove(idx);
                if idx < self.my_page_site_ids.len() {
                    self.my_page_site_ids.remove(idx);
                }
                if idx < self.my_page_site_labels.len() {
                    self.my_page_site_labels.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
    }

    /// Persistent draft key for `base` (an overlay's `draft_key()`). With more than one site
    /// configured the site is prefixed, so two sites' `ENG-1` keep separate drafts; a
    /// single-site config keeps the unprefixed key so existing drafts still restore.
    pub fn scoped_draft_key(&self, site_id: Option<&str>, base: String) -> String {
        match site_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(site) if self.config.sites.len() > 1 => {
                format!("site:{}:{base}", site.to_ascii_lowercase())
            }
            _ => base,
        }
    }

    fn clear_draft_and_save(&mut self, key: &str) {
        if self.drafts.remove(key).is_some() {
            let _ = self.save_drafts();
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
        let prev_cell_px = ui.graphics_caps.cell_px;
        let prev_cell_px_exact = ui.graphics_caps.cell_px_exact;
        ui.graphics_caps = img_preview::detect_graphics_caps(config_protocol_to_img_pref(
            self.config.image_protocol,
        ));
        if prev_cell_px_exact && !ui.graphics_caps.cell_px_exact {
            ui.graphics_caps.cell_px = prev_cell_px;
            ui.graphics_caps.cell_px_exact = true;
        }
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

    /// Rebuilds the shared `SiteClientPool` from `self.config`, updates `self.client`
    /// when a specific site scope is active, and pushes updated polling targets to `sync_ctrl`.
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
        let targets = sync::sync_targets(&self.site_pool);
        let _ = self.sync_ctrl.update_targets(targets);
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

    fn reset_active_view_for_site_switch(&mut self) {
        self.issue_detail_req_seq = self.issue_detail_req_seq.wrapping_add(1);
        self.page_detail_req_seq = self.page_detail_req_seq.wrapping_add(1);
        self.board_data_req_seq = self.board_data_req_seq.wrapping_add(1);
        self.issues_req_seq = self.issues_req_seq.wrapping_add(1);
        self.my_work_req_seq = self.my_work_req_seq.wrapping_add(1);
        self.remote_search_req_seq = self.remote_search_req_seq.wrapping_add(1);
        self.transitions_req_seq.fetch_add(1, Ordering::AcqRel);
        self.assignable_users_req_seq.fetch_add(1, Ordering::AcqRel);
        self.issues_custom_query = false;
        self.active_jql_label = None;
        self.active_jql_site_id = None;
        self.active_board_sprint_id = None;
        self.last_my_work_refresh_at = None;
        self.in_flight_issue_detail = None;
        self.in_flight_page_detail = None;
        self.active_issue = None;
        self.active_issue_site_id = None;
        self.active_issue_target = None;
        self.active_issue_detail_updated_at = None;
        self.active_page = None;
        self.active_page_site_id = None;
        self.active_page_target = None;
        self.active_page_detail_version = None;
        self.root_pages_by_space.clear();
        self.page_children_map.clear();
        self.expanded_pages.clear();
        self.board_issue_site_id = None;
    }

    fn reset_ui_for_site_switch(&self, ui: &mut UiState) {
        ui.search_ui.is_remote = false;
        ui.search_ui.selected_hit = 0;
        ui.board_ui.selected_sprint = 0;
        ui.board_ui.selected_col = 0;
        ui.board_ui.selected_row = 0;
        ui.board_ui.detail_scroll = 0;
        ui.board_ui.user_moved_board = false;
        ui.issue_ui.selected_filter = 0;
        ui.issue_ui.selected_issue = 0;
        ui.issue_ui.detail_scroll = 0;
        ui.issue_ui.local_filter.clear();
        ui.issue_ui.local_filter_active = false;
        ui.my_work_ui.selected_issue = 0;
        ui.my_work_ui.selected_page = 0;
        ui.confluence_ui.selected_tree_row = 0;
        ui.confluence_ui.selected_toc = 0;
        ui.confluence_ui.reader_scroll = 0;
        ui.confluence_ui.user_moved_space = false;
        ui.confluence_ui.space_filter.clear();
        ui.confluence_ui.space_filter_active = false;
        ui.directory_ui.selected_project = 0;
        ui.directory_ui.selected_space = 0;
    }

    /// Applies a `SiteSwitcherCommand` emitted by `SiteSwitcherOverlay`, persisting
    /// `active_site` / `enabled` to `config.toml` and refreshing federated views.
    pub fn apply_site_switcher_command(&mut self, cmd: SiteSwitcherCommand, ui: &mut UiState) {
        match cmd {
            SiteSwitcherCommand::SelectAllSites => {
                self.site_scope = SiteScope::All;
                self.config.active_site = None;
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
                ui.success("Scope: All Sites (Federated)");
            }
            SiteSwitcherCommand::SelectSite(site_id) => {
                let label = self.site_display_label(&site_id);
                self.site_scope = SiteScope::Only(site_id.clone());
                self.config.active_site = Some(site_id);
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
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
                let disabled_active_scope = now_enabled == Some(false)
                    && self
                        .site_scope
                        .site_id()
                        .is_some_and(|active_id| active_id.eq_ignore_ascii_case(&site_id));
                if disabled_active_scope {
                    self.site_scope = SiteScope::All;
                    self.config.active_site = None;
                }
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
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
            ui.info("Run `atlassian login --oauth` or `atlassian sites discover` to discover Cloud sites");
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
            "No OAuth session found. Run `atlassian login --oauth --client-id <ID>` to discover Cloud sites.",
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

    /// Returns all known per-site signed-in account IDs (lowercase `site_id -> user.id`)
    /// from SQLite persisted identities, `site_pool` cached profiles, and primary `self.me`.
    pub fn known_me_ids_by_site(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Ok(stored) = self.storage.list_identities() {
            for (sid, user) in stored {
                if !user.id.trim().is_empty() {
                    out.insert(sid.trim().to_ascii_lowercase(), user.id);
                }
            }
        }
        for (site, client) in self.site_pool.all_clients() {
            if let Some(me) = client.cached_me() {
                if !me.id.trim().is_empty() {
                    out.insert(site.id.trim().to_ascii_lowercase(), me.id);
                }
            }
        }
        if let Some(me) = self.me.clone().or_else(|| self.client.cached_me()) {
            if !me.id.trim().is_empty() {
                out.insert(
                    self.client.site_id().trim().to_ascii_lowercase(),
                    me.id,
                );
            }
        }
        out
    }

    /// Re-reads `cache_counts` scoped to the active site (or across all sites in federated mode).
    pub fn refresh_scoped_cache_counts(&mut self) {
        let counts = match self.site_scope.site_id() {
            Some(sid) => self.storage.cache_counts_for_site(sid),
            None => self.storage.cache_counts(),
        };
        if let Ok(c) = counts {
            self.cache_counts = c;
        }
    }

    /// Resets the Issues tab custom JQL / Saved Filter protection flag back to default view mode.
    pub fn reset_issues_custom_query(&mut self) {
        self.issues_custom_query = false;
        self.active_jql_label = None;
        self.active_jql_site_id = None;
    }

    /// Re-reads `my_issues` from SQLite respecting the active `site_scope`, per-site `me_id`,
    /// enabled sites, and excluding `Done` issues.
    fn reload_my_issues_from_storage(&mut self) {
        let show_pills = self.site_scope.is_all();
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                let (_, site_client) = self.site_id_and_client_for(Some(site_id));
                let me_id = self.known_me_for_site(site_id, &site_client).map(|m| m.id);
                self.my_issues = self
                    .storage
                    .list_my_work_for_site(site_id, me_id.as_deref(), 40)
                    .unwrap_or_default();
                self.my_issue_site_ids = vec![site_id.clone(); self.my_issues.len()];
                self.my_issue_site_labels = vec![None; self.my_issues.len()];
            }
            None => {
                let me_ids = self.known_me_ids_by_site();
                let my_work_pairs: Vec<(String, IssueSummary)> = self
                    .storage
                    .list_my_work_all_sites_for_users(&me_ids, 40)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                    .collect();
                let has_multi = self.config.enabled_sites().len() > 1
                    || my_work_pairs.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                self.my_issue_site_ids = my_work_pairs.iter().map(|(sid, _)| sid.clone()).collect();
                self.my_issue_site_labels = my_work_pairs
                    .iter()
                    .map(|(sid, _)| {
                        (show_pills && has_multi).then(|| self.site_display_label(sid))
                    })
                    .collect();
                self.my_issues = my_work_pairs.into_iter().map(|(_, i)| i).collect();
            }
        }
    }

    /// Re-reads `my_pages` from SQLite respecting the active `site_scope` and enabled sites.
    fn reload_my_pages_from_storage(&mut self) {
        let show_pills = self.site_scope.is_all();
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                if let Ok(pages) = self.storage.list_recent_pages_for_site(site_id, 30) {
                    self.my_pages = pages;
                    self.my_page_site_ids = vec![site_id.clone(); self.my_pages.len()];
                    self.my_page_site_labels = vec![None; self.my_pages.len()];
                }
            }
            None => {
                if let Ok(pages_all) = self.storage.list_recent_pages_all_sites(30) {
                    let filtered: Vec<(String, PageSummary)> = pages_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    let has_multi = self.config.enabled_sites().len() > 1
                        || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                    self.my_page_site_ids = filtered.iter().map(|(sid, _)| sid.clone()).collect();
                    self.my_page_site_labels = filtered
                        .iter()
                        .map(|(sid, _)| {
                            (show_pills && has_multi).then(|| self.site_display_label(sid))
                        })
                        .collect();
                    self.my_pages = filtered.into_iter().map(|(_, p)| p).collect();
                }
            }
        }
    }

    /// Clamps every list/tree/column cursor index in `UiState` to current collection lengths.
    pub fn clamp_selections(&self, ui: &mut UiState) {
        let clamp = |idx: &mut usize, len: usize| {
            if len == 0 {
                *idx = 0;
            } else if *idx >= len {
                *idx = len - 1;
            }
        };
        clamp(&mut ui.board_ui.selected_board, self.boards.len());
        clamp(&mut ui.board_ui.selected_sprint, self.sprints.len());
        let cols = self.build_kanban_columns(ui);
        clamp(&mut ui.board_ui.selected_col, cols.len());
        let col_len = cols
            .get(ui.board_ui.selected_col)
            .map(|c| c.issues.len())
            .unwrap_or(0);
        clamp(&mut ui.board_ui.selected_row, col_len);

        clamp(&mut ui.issue_ui.selected_filter, self.saved_filters.len());
        let filtered_len = self.filtered_issues(ui).len();
        clamp(&mut ui.issue_ui.selected_issue, filtered_len);

        clamp(&mut ui.my_work_ui.selected_issue, self.my_issues.len());
        clamp(&mut ui.my_work_ui.selected_page, self.my_pages.len());

        clamp(&mut ui.confluence_ui.selected_space, self.spaces.len());
        let tree_len = self.current_confluence_tree(ui).len();
        clamp(&mut ui.confluence_ui.selected_tree_row, tree_len);
        let toc_len = self
            .active_page
            .as_ref()
            .map(|p| p.toc.iter().filter(|t| t.level <= 4).count())
            .unwrap_or(0);
        clamp(&mut ui.confluence_ui.selected_toc, toc_len);

        clamp(&mut ui.search_ui.selected_hit, self.search_hits.len());

        clamp(&mut ui.directory_ui.selected_project, self.projects.len());
        clamp(&mut ui.directory_ui.selected_space, self.spaces.len());
        let site_count = self.config.effective_sites().len();
        clamp(&mut ui.directory_ui.selected_site, site_count);
    }

    /// Re-sorts `projects` and `spaces` so pinned items float to the top (matching SQLite order),
    /// keeping parallel `project_site_ids` and `space_site_ids` strictly aligned.
    pub fn sort_directory_pinned(&mut self) {
        let multi = self.is_multi_site_all_mode();

        let mut proj_pairs: Vec<(String, ProjectSummary)> = self
            .projects
            .drain(..)
            .enumerate()
            .map(|(i, p)| {
                let sid = self
                    .project_site_ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_SITE_ID.to_string());
                (sid, p)
            })
            .collect();
        proj_pairs.sort_by(|(sa, a), (sb, b)| {
            let site_ord = if multi {
                sa.cmp(sb)
            } else {
                std::cmp::Ordering::Equal
            };
            site_ord
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        self.project_site_ids = proj_pairs.iter().map(|(s, _)| s.clone()).collect();
        self.projects = proj_pairs.into_iter().map(|(_, p)| p).collect();

        let mut space_pairs: Vec<(String, SpaceSummary)> = self
            .spaces
            .drain(..)
            .enumerate()
            .map(|(i, s)| {
                let sid = self
                    .space_site_ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_SITE_ID.to_string());
                (sid, s)
            })
            .collect();
        space_pairs.sort_by(|(sa, a), (sb, b)| {
            let site_ord = if multi {
                sa.cmp(sb)
            } else {
                std::cmp::Ordering::Equal
            };
            site_ord
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        self.space_site_ids = space_pairs.iter().map(|(s, _)| s.clone()).collect();
        self.spaces = space_pairs.into_iter().map(|(_, s)| s).collect();
    }

    /// Reloads SQLite cache while preserving user-selected board/sprint, space, custom JQL
    /// results, and active Search results (never wiping search with an empty query).
    pub fn reload_cached_preserving_ui(&mut self, ui: &mut UiState) {
        let prev_board: Option<(String, i64)> = if ui.board_ui.user_moved_board {
            self.active_board_with_site(ui).map(|(sid, b)| (sid, b.id))
        } else {
            None
        };
        let prev_sprint_id = self
            .active_board_sprint_id
            .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
        let prev_space: Option<(String, String)> = if ui.confluence_ui.user_moved_space {
            self.active_space(ui).map(|sp| (self.tree_site(ui), sp.key.clone()))
        } else {
            None
        };
        let preserved_custom_issues = if self.issues_custom_query {
            Some((self.issues.clone(), self.issue_site_ids.clone()))
        } else {
            None
        };

        self.load_cached_initial();

        if let Some((saved_issues, saved_site_ids)) = preserved_custom_issues {
            self.issues = saved_issues;
            self.issue_site_ids = saved_site_ids;
        }

        if let Some((prev_sid, prev_bid)) = prev_board {
            if let Some(new_idx) = self
                .boards
                .iter()
                .enumerate()
                .position(|(idx, b)| {
                    b.id == prev_bid
                        && self
                            .board_site_ids
                            .get(idx)
                            .is_some_and(|s| s.eq_ignore_ascii_case(&prev_sid))
                })
            {
                ui.board_ui.selected_board = new_idx;
                let board_sid = self
                    .board_site_ids
                    .get(new_idx)
                    .cloned()
                    .unwrap_or(prev_sid);
                self.board_issue_site_id = Some(board_sid.clone());
                if let Ok(sprints) = self.storage.list_sprints_for_site(&board_sid, prev_bid) {
                    self.sprints = sprints;
                }
                let matched_sprint = prev_sprint_id.and_then(|sid| {
                    self.sprints
                        .iter()
                        .position(|s| s.id == sid)
                        .map(|idx| (idx, sid))
                });
                let effective_sprint = if let Some((sp_idx, sid)) = matched_sprint {
                    ui.board_ui.selected_sprint = sp_idx;
                    Some(sid)
                } else {
                    self.sprints
                        .iter()
                        .position(|s| s.state.eq_ignore_ascii_case("active"))
                        .map(|idx| {
                            ui.board_ui.selected_sprint = idx;
                            self.sprints[idx].id
                        })
                };
                self.active_board_sprint_id = effective_sprint;
                if let Ok(bi) = self.storage.list_board_issues_for_site(
                    &board_sid,
                    prev_bid,
                    effective_sprint,
                    BOARD_ISSUE_VIEW_CAP,
                ) {
                    self.board_issues = bi;
                }
            }
        }

        if let Some((prev_sid, prev_key)) = prev_space {
            if let Some(new_idx) = self
                .spaces
                .iter()
                .enumerate()
                .position(|(idx, sp)| {
                    sp.key.eq_ignore_ascii_case(&prev_key)
                        && self
                            .space_site_ids
                            .get(idx)
                            .is_some_and(|s| s.eq_ignore_ascii_case(&prev_sid))
                })
            {
                ui.confluence_ui.selected_space = new_idx;
                let space_sid = self
                    .space_site_ids
                    .get(new_idx)
                    .cloned()
                    .unwrap_or(prev_sid);
                let space_key = self.spaces[new_idx].key.clone();
                if let Ok(roots) = self
                    .storage
                    .list_space_root_pages_for_site(&space_sid, &space_key)
                {
                    self.root_pages_by_space
                        .insert(SiteKey::new(&space_sid, &space_key), roots);
                }
            }
        }

        self.apply_default_selection(ui);

        if !ui.search_ui.is_remote && !ui.search_ui.query.text.trim().is_empty() {
            let q = ui.search_ui.query.text.clone();
            self.refresh_local_search(&q);
        }

        self.clamp_selections(ui);
    }

    pub fn load_cached_initial(&mut self) {
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                if let Ok(projs) = self.storage.list_projects_for_site(site_id) {
                    self.project_site_ids = vec![site_id.clone(); projs.len()];
                    self.projects = projs;
                }
                if let Ok(boards) = self.storage.list_boards_for_site(site_id, None) {
                    self.board_site_ids = vec![site_id.clone(); boards.len()];
                    self.boards = boards;
                    if let Some(default_board) = self.boards.get(self.resolve_default_board_index())
                    {
                        let bid = default_board.id;
                        self.board_issue_site_id = Some(site_id.clone());
                        if let Ok(sprints) = self.storage.list_sprints_for_site(site_id, bid) {
                            self.sprints = sprints;
                        }
                        self.active_board_sprint_id = self
                            .sprints
                            .iter()
                            .find(|s| s.state.eq_ignore_ascii_case("active"))
                            .map(|s| s.id);
                        if let Ok(bi) =
                            self.storage
                                .list_board_issues_for_site(site_id, bid, None, BOARD_ISSUE_VIEW_CAP)
                        {
                            self.board_issues = bi;
                        }
                    } else {
                        self.board_issue_site_id = None;
                        self.sprints.clear();
                        self.board_issues.clear();
                        self.active_board_sprint_id = None;
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
                } else {
                    self.saved_filters = builtin_filters();
                }
                if let Ok(issues) = self.storage.list_issues_for_site(site_id, None, 200) {
                    self.issue_site_ids = vec![site_id.clone(); issues.len()];
                    self.issues = issues.clone();
                    self.reload_my_issues_from_storage();
                    let preserved_key = self
                        .active_issue
                        .as_ref()
                        .filter(|_| {
                            self.active_issue_site_id
                                .as_deref()
                                .map(|s| s.eq_ignore_ascii_case(site_id))
                                .unwrap_or(true)
                        })
                        .map(|d| d.summary.key.clone())
                        .filter(|k| issues.iter().any(|i| &i.key == k));
                    if let Some(key) = preserved_key {
                        if let Ok(Some(detail)) = self.storage.get_issue_detail_for_site(site_id, &key) {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(site_id, &key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(site_id.clone());
                            self.active_issue_target = Some((site_id.clone(), key));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    } else if let Some(first) = issues.first() {
                        if let Ok(Some(detail)) =
                            self.storage.get_issue_detail_for_site(site_id, &first.key)
                        {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(site_id, &first.key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(site_id.clone());
                            self.active_issue_target = Some((site_id.clone(), first.key.clone()));
                            self.active_issue_detail_updated_at = detail_ts;
                        } else if self
                            .active_issue_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                        {
                            self.active_issue = None;
                            self.active_issue_site_id = None;
                            self.active_issue_target = None;
                            self.active_issue_detail_updated_at = None;
                        }
                    } else if self
                        .active_issue_site_id
                        .as_deref()
                        .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                    {
                        self.active_issue = None;
                        self.active_issue_site_id = None;
                        self.active_issue_target = None;
                        self.active_issue_detail_updated_at = None;
                    }
                }
                if let Some(active_page_id) = self.active_page.as_ref().map(|p| p.summary.id.clone()) {
                    if let Ok(Some(detail)) =
                        self.storage.get_page_detail_for_site(site_id, &active_page_id)
                    {
                        let detail_ver = self
                            .storage
                            .get_page_detail_version_for_site(site_id, &active_page_id)
                            .ok()
                            .flatten();
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id.clone(), active_page_id));
                        self.active_page_detail_version = detail_ver;
                    } else if self
                        .active_page_site_id
                        .as_deref()
                        .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                    {
                        self.active_page = None;
                        self.active_page_site_id = None;
                        self.active_page_target = None;
                        self.active_page_detail_version = None;
                    }
                }
                if let Ok(spaces) = self.storage.list_spaces_for_site(site_id) {
                    self.space_site_ids = vec![site_id.clone(); spaces.len()];
                    self.spaces = spaces;
                    if let Some(default_space) = self.spaces.get(self.resolve_default_space_index())
                    {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(site_id, &default_space.key)
                        {
                            self.root_pages_by_space
                                .insert(SiteKey::new(site_id, &default_space.key), roots);
                        }
                    }
                }
                self.reload_my_pages_from_storage();
                if let Ok(counts) = self.storage.cache_counts_for_site(site_id) {
                    self.cache_counts = counts;
                }
            }
            None => {
                self.reload_projects_all_sites();
                if let Ok(boards) = self.storage.list_boards_all_sites(None) {
                    let filtered: Vec<(String, BoardSummary)> = boards
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.board_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.boards = filtered.into_iter().map(|(_, b)| b).collect();
                    let default_idx = self.resolve_default_board_index();
                    if let (Some(default_board), Some(board_sid)) = (
                        self.boards.get(default_idx),
                        self.board_site_ids.get(default_idx),
                    ) {
                        let bid = default_board.id;
                        self.board_issue_site_id = Some(board_sid.clone());
                        if let Ok(sprints) = self.storage.list_sprints_for_site(board_sid, bid) {
                            self.sprints = sprints;
                        }
                        self.active_board_sprint_id = self
                            .sprints
                            .iter()
                            .find(|s| s.state.eq_ignore_ascii_case("active"))
                            .map(|s| s.id);
                        if let Ok(bi) = self.storage.list_board_issues_for_site(
                            board_sid,
                            bid,
                            None,
                            BOARD_ISSUE_VIEW_CAP,
                        ) {
                            self.board_issues = bi;
                        }
                    } else {
                        self.board_issue_site_id = None;
                        self.sprints.clear();
                        self.board_issues.clear();
                        self.active_board_sprint_id = None;
                    }
                }
                {
                    let mut combined = builtin_filters();
                    let filter_site_id = self.client.site_id().to_string();
                    if let Ok(filters) = self.storage.list_saved_filters_for_site(&filter_site_id) {
                        for f in filters {
                            if !combined.iter().any(|b| b.id == f.id) {
                                combined.push(f);
                            }
                        }
                    }
                    if !filter_site_id.eq_ignore_ascii_case(DEFAULT_SITE_ID) {
                        if let Ok(filters) = self.storage.list_saved_filters() {
                            for f in filters {
                                if !combined.iter().any(|b| b.id == f.id) {
                                    combined.push(f);
                                }
                            }
                        }
                    }
                    self.saved_filters = combined;
                }
                if let Ok(issues_all) = self.storage.list_issues_all_sites(None, 200) {
                    let filtered: Vec<(String, IssueSummary)> = issues_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.issue_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.issues = filtered.iter().map(|(_, i)| i.clone()).collect();

                    self.reload_my_issues_from_storage();

                    let preserved = self.active_issue.as_ref().and_then(|d| {
                        let existing_sid = self.active_issue_site_id.as_deref();
                        filtered
                            .iter()
                            .find(|(sid, i)| {
                                i.key == d.summary.key
                                    && existing_sid
                                        .map(|e| e.eq_ignore_ascii_case(sid))
                                        .unwrap_or(true)
                            })
                            .map(|(sid, i)| (sid.clone(), i.key.clone()))
                    });
                    if let Some((sid, key)) = preserved {
                        if let Ok(Some(detail)) = self.storage.get_issue_detail_for_site(&sid, &key) {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(&sid, &key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(sid.clone());
                            self.active_issue_target = Some((sid, key));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    } else if let Some((first_sid, first)) = filtered.first() {
                        if let Ok(Some(detail)) = self
                            .storage
                            .get_issue_detail_for_site(first_sid, &first.key)
                        {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(first_sid, &first.key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(first_sid.clone());
                            self.active_issue_target = Some((first_sid.clone(), first.key.clone()));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    }
                }
                if let Some(active_page_id) = self.active_page.as_ref().map(|p| p.summary.id.clone()) {
                    let target_sid = self
                        .active_page_site_id
                        .clone()
                        .unwrap_or_else(|| self.client.site_id().to_string());
                    if let Ok(Some(detail)) =
                        self.storage.get_page_detail_for_site(&target_sid, &active_page_id)
                    {
                        let detail_ver = self
                            .storage
                            .get_page_detail_version_for_site(&target_sid, &active_page_id)
                            .ok()
                            .flatten();
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(target_sid.clone());
                        self.active_page_target = Some((target_sid, active_page_id));
                        self.active_page_detail_version = detail_ver;
                    }
                }
                if let Ok(spaces_all) = self.storage.list_spaces_all_sites() {
                    let filtered: Vec<(String, SpaceSummary)> = spaces_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.space_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.spaces = filtered.into_iter().map(|(_, s)| s).collect();
                    let default_idx = self.resolve_default_space_index();
                    if let (Some(default_space), Some(space_sid)) = (
                        self.spaces.get(default_idx),
                        self.space_site_ids.get(default_idx),
                    ) {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(space_sid, &default_space.key)
                        {
                            self.root_pages_by_space
                                .insert(SiteKey::new(space_sid, &default_space.key), roots);
                        }
                    }
                }
                self.reload_my_pages_from_storage();
                if let Ok(counts) = self.storage.cache_counts() {
                    self.cache_counts = counts;
                }
            }
        }
    }

    pub fn spawn_bootstrap(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let mut targets: Vec<(String, String, AtlassianClient)> = self
            .site_pool
            .enabled_clients()
            .into_iter()
            .filter(|(site, client)| {
                !site.site_url.trim().is_empty() && client.auth().is_logged_in()
            })
            .map(|(site, client)| {
                (
                    site.id.clone(),
                    site.display_name().to_string(),
                    client,
                )
            })
            .collect();
        if targets.is_empty()
            && self.auth.is_logged_in()
            && !self.config.site_url.trim().is_empty()
        {
            targets.push((
                self.client.site_id().to_string(),
                self.site_display_label(self.client.site_id()),
                self.client.clone(),
            ));
        }
        if targets.is_empty() {
            ui.info("Offline cache mode (run `atlassian login` to connect)");
            return;
        }

        ui.tasks.begin("Initial sync");
        let primary_site_id = self.client.site_id().to_string();
        let active_scope = self.site_scope.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let mut primary_me_sent = false;
            for (idx, (site_id, _site_name, client)) in targets.iter().enumerate() {
                let is_primary =
                    site_id.eq_ignore_ascii_case(&primary_site_id) || (targets.len() == 1 && idx == 0);
                match client.get_myself().await {
                    Ok(me) => {
                        client.set_cached_me(me.clone());
                        let _ = storage.save_identity_for_site(site_id, &me);
                        if is_primary && !primary_me_sent {
                            primary_me_sent = true;
                            let _ = tx.send(UiMsg::MeLoaded(Ok(me)));
                        }
                    }
                    Err(e) => {
                        if is_primary && !primary_me_sent {
                            primary_me_sent = true;
                            let _ = tx.send(UiMsg::MeLoaded(Err(e.to_string())));
                        }
                    }
                }
            }

            let summary = if targets.len() == 1 {
                let (site_id, _, client) = &targets[0];
                sync::bootstrap_workspace_for_site(site_id, client, &storage)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                let sync_targets: Vec<SiteSyncTarget> = targets
                    .iter()
                    .map(|(site_id, site_name, client)| SiteSyncTarget {
                        site_id: site_id.clone(),
                        site_name: site_name.clone(),
                        client: client.clone(),
                    })
                    .collect();
                let mut breakers = sync::CircuitBreakerMap::new();
                let outcome = sync::run_multi_site_sync(
                    &sync_targets,
                    &storage,
                    &mut breakers,
                    sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                    None,
                )
                .await;
                if outcome.succeeded_sites > 0 {
                    Ok(outcome.summary)
                } else {
                    let first_err = outcome
                        .per_site
                        .iter()
                        .find_map(|r| r.error.clone())
                        .unwrap_or(outcome.summary);
                    Err(first_err)
                }
            };
            let _ = tx.send(UiMsg::BootstrapFinished(summary));

            let mut visible_filters = Vec::new();
            for (site_id, _, client) in &targets {
                if let Ok(filters) = client.list_favourite_filters().await {
                    let _ = storage.upsert_saved_filters_for_site(site_id, &filters);
                    if active_scope.matches(site_id) {
                        for f in filters {
                            if !visible_filters.iter().any(|existing: &SavedFilter| existing.id == f.id) {
                                visible_filters.push(f);
                            }
                        }
                    }
                }
            }
            if !visible_filters.is_empty() {
                let _ = tx.send(UiMsg::SavedFiltersLoaded(visible_filters));
            }
        });
    }

    pub fn active_board(&self, ui: &UiState) -> Option<&BoardSummary> {
        self.boards.get(ui.board_ui.selected_board)
    }

    pub fn active_board_with_site(&self, ui: &UiState) -> Option<(String, BoardSummary)> {
        let board = self.active_board(ui)?.clone();
        let site_id = self
            .active_board_site_id(ui)
            .map(str::to_string)
            .unwrap_or_else(|| self.effective_site_id_and_client().0);
        Some((site_id, board))
    }

    pub fn active_board_site_id(&self, ui: &UiState) -> Option<&str> {
        self.board_site_ids
            .get(ui.board_ui.selected_board)
            .map(String::as_str)
            .or(self.board_issue_site_id.as_deref())
    }

    pub fn active_space_site_id(&self, ui: &UiState) -> Option<&str> {
        self.space_site_ids
            .get(ui.confluence_ui.selected_space)
            .map(String::as_str)
    }

    /// Identifies the single currently open Jira issue or Confluence page on screen,
    /// including its canonical `site_id`, shared by selection, focus return, and periodic checks.
    pub fn current_open_target(&self, ui: &UiState) -> Option<OpenItemTarget> {
        if let Some(top) = ui.overlays.last() {
            match top {
                Overlay::Comment(ov) => match &ov.target {
                    CommentTarget::Issue(key) => {
                        let sid = self
                            .active_issue_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid,
                            key: key.clone(),
                        });
                    }
                    CommentTarget::Page { page_id, .. } => {
                        let sid = self
                            .active_page_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid,
                            page_id: page_id.clone(),
                        });
                    }
                },
                Overlay::IssueForm(ov) => {
                    if let Some(ref key) = ov.edit_key {
                        let sid = self
                            .active_issue_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid,
                            key: key.clone(),
                        });
                    }
                }
                Overlay::PageForm(ov) => {
                    if let Some(ref page_id) = ov.edit_page_id {
                        let sid = self
                            .active_page_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid,
                            page_id: page_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        match ui.active_tab {
            WorkspaceTab::Confluence => {
                if let Some((ref sid, ref pid)) = self.active_page_target {
                    return Some(OpenItemTarget::ConfluencePage {
                        site_id: sid.clone(),
                        page_id: pid.clone(),
                    });
                }
                self.active_page.as_ref().map(|p| {
                    let sid = self
                        .active_page_site_id
                        .clone()
                        .or_else(|| self.active_space_site_id(ui).map(str::to_string))
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    OpenItemTarget::ConfluencePage {
                        site_id: sid,
                        page_id: p.summary.id.clone(),
                    }
                })
            }
            WorkspaceTab::Issues => {
                if let Some((ref sid, ref key)) = self.active_issue_target {
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid.clone(),
                        key: key.clone(),
                    });
                }
                if let Some(ref d) = self.active_issue {
                    let sid = self
                        .active_issue_site_id
                        .clone()
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid,
                        key: d.summary.key.clone(),
                    });
                }
                None
            }
            WorkspaceTab::Board => {
                if !ui.board_ui.drawer_open
                    && self.active_issue.is_none()
                    && self.active_issue_target.is_none()
                {
                    return None;
                }
                if let Some((ref sid, ref key)) = self.active_issue_target {
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid.clone(),
                        key: key.clone(),
                    });
                }
                self.active_issue.as_ref().map(|d| {
                    let sid = self
                        .active_issue_site_id
                        .clone()
                        .or_else(|| self.active_board_site_id(ui).map(str::to_string))
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    OpenItemTarget::JiraIssue {
                        site_id: sid,
                        key: d.summary.key.clone(),
                    }
                })
            }
            _ => {
                if matches!(ui.overlays.last(), Some(Overlay::Attachments(_))) {
                    if let Some((ref sid, ref pid)) = self.active_page_target {
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid.clone(),
                            page_id: pid.clone(),
                        });
                    }
                    if let Some((ref sid, ref key)) = self.active_issue_target {
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid.clone(),
                            key: key.clone(),
                        });
                    }
                }
                None
            }
        }
    }

    /// Synchronizes the currently open object with the background sync engine's 5s watcher.
    pub fn sync_open_item_watch(&mut self, ui: &UiState) {
        let target = self.current_open_target(ui);
        if target != self.last_watched_open_item {
            self.last_watched_open_item = target.clone();
            let _ = self
                .sync_ctrl
                .request(sync::SyncCommand::WatchOpenItem { target });
        }
    }

    /// Triggered on terminal `FocusGained`: immediately checks the open screen object's revision
    /// (and refreshes comments/attachments if their dedicated panel is currently visible).
    pub fn handle_focus_gained(&mut self, ui: &mut UiState) {
        self.sync_open_item_watch(ui);
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let now = Instant::now();
        if let Some(prev) = self.last_focus_check_at {
            if now.saturating_duration_since(prev) < FOCUS_CHECK_DEBOUNCE
                && self.last_watched_open_item.as_ref() == Some(&target)
            {
                return;
            }
        }
        self.last_focus_check_at = Some(now);
        let _ = self
            .sync_ctrl
            .request(sync::SyncCommand::CheckOpenItemNow { target });

        match ui.overlays.last() {
            Some(Overlay::Attachments(_)) => self.request_attachments_refresh(ui),
            Some(Overlay::Comment(_)) => self.request_comments_refresh(ui),
            _ => {}
        }
    }

    fn current_open_issue_identity(&self) -> Option<(String, String)> {
        if let Some((ref sid, ref key)) = self.active_issue_target {
            return Some((sid.clone(), key.clone()));
        }
        self.active_issue.as_ref().map(|d| {
            let sid = self
                .active_issue_site_id
                .clone()
                .unwrap_or_else(|| self.effective_site_id_and_client().0);
            (sid, d.summary.key.clone())
        })
    }

    fn current_open_page_identity(&self) -> Option<(String, String)> {
        if let Some((ref sid, ref pid)) = self.active_page_target {
            return Some((sid.clone(), pid.clone()));
        }
        self.active_page.as_ref().map(|p| {
            let sid = self
                .active_page_site_id
                .clone()
                .unwrap_or_else(|| self.effective_site_id_and_client().0);
            (sid, p.summary.id.clone())
        })
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
        let site = self.tree_site(ui);
        let roots = self
            .root_pages_by_space
            .get(&SiteKey::new(&site, &space.key))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        flatten_page_tree(roots, &site, &self.page_children_map, &self.expanded_pages)
    }

    /// Site the SPACES tree currently shows: the selected space's site, else
    /// the effective site of the current scope.
    pub fn tree_site(&self, ui: &UiState) -> String {
        self.active_space_site_id(ui)
            .map(str::to_string)
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Index of `config.default_space_key` in `spaces` (case-insensitive; the entry on
    /// `config.resolved_site_profile()` wins when several sites have the key), else `0`.
    pub fn resolve_default_space_index(&self) -> usize {
        let Some(key) = self
            .config
            .default_space_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return 0;
        };
        self.default_index_on_site(self.spaces.len(), &self.space_site_ids, |i| {
            self.spaces[i].key.eq_ignore_ascii_case(key)
        })
        .unwrap_or(0)
    }

    /// Index of `config.default_board_id`, else of the first board of
    /// `config.default_project` (the order of the sync's default board), else `0`.
    pub fn resolve_default_board_index(&self) -> usize {
        let by_id = self.config.default_board_id.and_then(|bid| {
            self.default_index_on_site(self.boards.len(), &self.board_site_ids, |i| {
                self.boards[i].id == bid
            })
        });
        let by_project = || {
            let project = self
                .config
                .default_project
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())?;
            self.default_index_on_site(self.boards.len(), &self.board_site_ids, |i| {
                self.boards[i]
                    .project_key
                    .as_deref()
                    .is_some_and(|k| k.eq_ignore_ascii_case(project))
            })
        };
        by_id.or_else(by_project).unwrap_or(0)
    }

    /// First index in `0..len` matching `pred`, preferring one whose site in `site_ids`
    /// is `config.resolved_site_profile()` (keys and ids repeat across sites).
    fn default_index_on_site(
        &self,
        len: usize,
        site_ids: &[String],
        pred: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let preferred = self.config.resolved_site_profile().map(|s| s.id);
        let mut first = None;
        for i in (0..len).filter(|&i| pred(i)) {
            let on_preferred = preferred
                .as_deref()
                .is_some_and(|p| site_ids.get(i).is_some_and(|s| s.eq_ignore_ascii_case(p)));
            if on_preferred {
                return Some(i);
            }
            first.get_or_insert(i);
        }
        first
    }

    /// Points the board and SPACES selections at the configured defaults unless the
    /// user already picked one, so a background reload never yanks the cursor.
    pub fn apply_default_selection(&self, ui: &mut UiState) {
        if !ui.board_ui.user_moved_board {
            ui.board_ui.selected_board = self.resolve_default_board_index();
        }
        if !ui.confluence_ui.user_moved_space {
            ui.confluence_ui.selected_space = self.resolve_default_space_index();
        }
    }

    /// Indices into `spaces` matching the SPACES filter, best match first, then
    /// cached order (pinned first); every space in cached order without a filter.
    fn filtered_space_indices(&self, ui: &UiState) -> Vec<usize> {
        let query = ui.confluence_ui.space_filter.text.trim();
        if query.is_empty() {
            return (0..self.spaces.len()).collect();
        }
        let mut scored: Vec<(u32, usize)> = self
            .spaces
            .iter()
            .enumerate()
            .filter_map(|(i, sp)| space_match_score(query, sp).map(|score| (score, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// SPACES pane rows in display order: the filtered spaces, grouped under
    /// collapsible site headers in multi-site All Sites mode.
    pub fn space_pane_rows(&self, ui: &UiState) -> Vec<SpacePaneRow> {
        let matches = self.filtered_space_indices(ui);
        if !self.is_multi_site_all_mode() {
            return matches.into_iter().map(SpacePaneRow::Space).collect();
        }
        let mut site_order: Vec<String> = Vec::new();
        let enabled = self.config.enabled_sites().into_iter().map(|s| s.id);
        for sid in enabled.chain(self.space_site_ids.iter().cloned()) {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(&sid)) {
                site_order.push(sid);
            }
        }
        let mut rows = Vec::new();
        for site_id in site_order {
            let group: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|&i| {
                    self.space_site_ids
                        .get(i)
                        .is_some_and(|s| s.eq_ignore_ascii_case(&site_id))
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            let collapsed = ui
                .collapsed_site_groups
                .contains(&space_group_key(&site_id));
            rows.push(SpacePaneRow::SiteHeader {
                site_id,
                spaces: group.len(),
                collapsed,
            });
            if !collapsed {
                rows.extend(group.into_iter().map(SpacePaneRow::Space));
            }
        }
        rows
    }

    /// Indices into `spaces` the SPACES pane lists, in display order; navigation
    /// steps over these while `selected_space` stays an index into `spaces`.
    pub fn visible_space_indices(&self, ui: &UiState) -> Vec<usize> {
        self.space_pane_rows(ui)
            .into_iter()
            .filter_map(|row| match row {
                SpacePaneRow::Space(i) => Some(i),
                SpacePaneRow::SiteHeader { .. } => None,
            })
            .collect()
    }

    /// Selects the first listed space when the SPACES filter hides the selected one.
    pub fn keep_space_selection_visible(&self, ui: &mut UiState) {
        let visible = self.visible_space_indices(ui);
        if !visible.contains(&ui.confluence_ui.selected_space) {
            if let Some(&first) = visible.first() {
                ui.confluence_ui.selected_space = first;
            }
        }
    }

    /// Whether personal space `space_key` (`~<account id or username>`) belongs to the
    /// signed-in user on `site_id`; `None` without a cached identity for that site.
    pub fn personal_space_owner_is_me(&self, site_id: &str, space_key: &str) -> Option<bool> {
        let owner = space_key.trim().strip_prefix('~')?.trim();
        let (site, client) = self.site_id_and_client_for(Some(site_id));
        let me = self.known_me_for_site(&site, &client)?;
        let me_id = me.id.trim();
        Some(!me_id.is_empty() && owner.eq_ignore_ascii_case(me_id))
    }

    pub fn selected_issue_key(&self, ui: &UiState) -> Option<String> {
        self.selected_issue_ref(ui).map(|(_, key)| key)
    }

    /// `(site_id, key)` of the issue a write should target: the selected row
    /// together with the site that row was loaded from — never the site of
    /// whichever issue happens to be open in the detail pane (ATL-11).
    pub fn selected_issue_ref(&self, ui: &UiState) -> Option<(String, String)> {
        let fallback_site = || self.effective_site_id_and_client().0;
        let opened = || {
            self.active_issue.as_ref().map(|d| {
                (
                    self.active_issue_site_id
                        .clone()
                        .unwrap_or_else(fallback_site),
                    d.summary.key.clone(),
                )
            })
        };
        match ui.active_tab {
            WorkspaceTab::Board => self
                .selected_board_issue(ui)
                .map(|i| {
                    (
                        self.board_issue_site_id
                            .clone()
                            .unwrap_or_else(fallback_site),
                        i.key,
                    )
                })
                .or_else(opened),
            WorkspaceTab::Issues => self
                .filtered_issues_with_sites(ui)
                .into_iter()
                .nth(ui.issue_ui.selected_issue)
                .map(|(sid, i)| (sid, i.key))
                .or_else(opened),
            WorkspaceTab::MyWork => {
                let idx = ui.my_work_ui.selected_issue;
                self.my_issues.get(idx).map(|i| {
                    (
                        self.my_issue_site_ids
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(fallback_site),
                        i.key.clone(),
                    )
                })
            }
            _ => opened(),
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

    /// Site of the issue or page `current_cross_ref(tab)` names.
    pub fn current_item_site(&self, tab: WorkspaceTab) -> Option<String> {
        match tab {
            WorkspaceTab::Confluence => self.active_page_site_id.clone(),
            _ => self.active_issue_site_id.clone(),
        }
    }

    /// Site of the item `current_attachments(ui)` lists the attachments of.
    pub fn current_attachments_site(&self, ui: &UiState) -> Option<String> {
        if (ui.active_tab == WorkspaceTab::Confluence
            || (ui.active_tab == WorkspaceTab::MyWork && ui.my_work_ui.active_column == 1))
            && self.active_page.is_some()
        {
            return self.active_page_site_id.clone();
        }
        if self.active_issue.is_some() {
            return self.active_issue_site_id.clone();
        }
        self.active_page_site_id.clone()
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
        self.jump_to_cross_ref_for_site(ui, None, target);
    }

    pub fn jump_to_cross_ref_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: CrossRefTarget,
    ) {
        if let Some(current) = self.current_cross_ref(ui.active_tab) {
            if self.nav_history.last().map(|e| &e.target) != Some(&current) && current != target {
                self.nav_history.push(NavEntry {
                    site_id: self.current_item_site(ui.active_tab),
                    target: current,
                });
            }
        }
        self.open_cross_ref_without_push_for_site(ui, site_id, target);
    }

    pub fn pop_nav_history(&mut self, ui: &mut UiState) -> bool {
        let Some(prev) = self.nav_history.pop() else {
            return false;
        };
        self.open_cross_ref_without_push_for_site(ui, prev.site_id.as_deref(), prev.target);
        true
    }

    pub fn open_cross_ref_without_push(&mut self, ui: &mut UiState, target: CrossRefTarget) {
        self.open_cross_ref_without_push_for_site(ui, None, target);
    }

    pub fn open_cross_ref_without_push_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: CrossRefTarget,
    ) {
        match target {
            CrossRefTarget::JiraIssue { key, .. } => {
                ui.active_tab = WorkspaceTab::Issues;
                ui.focus = PaneFocus::Main;
                let find_in_filtered = |app: &Self, ui: &UiState| {
                    let filtered = app.filtered_issues_with_sites(ui);
                    if let Some(sid) = site_id {
                        filtered
                            .iter()
                            .position(|(s, i)| i.key == key && s.eq_ignore_ascii_case(sid))
                            .or_else(|| filtered.iter().position(|(_, i)| i.key == key))
                    } else {
                        filtered.iter().position(|(_, i)| i.key == key)
                    }
                };
                if let Some(pos) = find_in_filtered(self, ui) {
                    ui.issue_ui.selected_issue = pos;
                } else if self.issues.iter().any(|i| i.key == key)
                    && !ui.issue_ui.local_filter.text.trim().is_empty()
                {
                    ui.issue_ui.local_filter.clear();
                    ui.issue_ui.local_filter_active = false;
                    if let Some(pos) = find_in_filtered(self, ui) {
                        ui.issue_ui.selected_issue = pos;
                    }
                }
                self.request_issue_detail_for_site(ui, site_id, &key);
            }
            CrossRefTarget::ConfluencePage { page_id, .. } => {
                ui.active_tab = WorkspaceTab::Confluence;
                ui.focus = PaneFocus::Main;
                self.request_page_detail_for_site(ui, site_id, &page_id);
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

    /// Board id and site of the BOARDS row `row`. Board ids are only unique per site, so the
    /// site comes from the row itself, never from a lookup by id.
    pub fn board_row_target(&self, row: usize) -> Option<(i64, Option<&str>)> {
        let board = self.boards.get(row)?;
        Some((board.id, self.board_site_ids.get(row).map(String::as_str)))
    }

    /// Loads the board shown in BOARDS row `row` (and `sprint_id`, when given) from that row's site.
    pub fn request_board_load(&mut self, ui: &mut UiState, row: usize, sprint_id: Option<i64>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some((board_id, board_site)) = self.board_row_target(row) else {
            return;
        };
        let (site_id, client) = self.site_id_and_client_for(board_site);
        self.board_data_req_seq = self.board_data_req_seq.wrapping_add(1);
        let seq = self.board_data_req_seq;
        ui.tasks.begin(format!("Board #{board_id}"));
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let columns = client.get_board_columns(board_id).await.unwrap_or_default();
            let sprints = client.list_board_sprints(board_id).await.unwrap_or_default();
            if !sprints.is_empty() {
                let _ = storage.upsert_sprints_for_site(&site_id, &sprints);
            }
            let effective_sprint = sprint_id.or_else(|| {
                sprints
                    .iter()
                    .find(|s| s.state.eq_ignore_ascii_case("active"))
                    .map(|s| s.id)
            });
            match sync::load_board_issues_for_site(
                &client,
                &storage,
                &site_id,
                board_id,
                effective_sprint,
            )
            .await
            {
                Ok(issues) => {
                    let _ = tx.send(UiMsg::BoardDataLoaded {
                        seq,
                        site_id,
                        board_id,
                        requested_sprint_id: sprint_id,
                        columns,
                        sprints,
                        issues,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::BoardLoadFailed {
                        seq,
                        site_id,
                        board_id,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_jql_search(&mut self, ui: &mut UiState, jql: String, label: String) {
        self.request_jql_search_for_site(ui, None, jql, label);
    }

    pub fn request_jql_search_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        jql: String,
        label: String,
    ) {
        self.issues_custom_query = true;
        self.active_jql_label = Some(label.clone());
        self.active_jql_site_id = explicit_site.map(str::to_string);
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.issues_req_seq = self.issues_req_seq.wrapping_add(1);
        let seq = self.issues_req_seq;
        ui.tasks.begin(format!("JQL: {label}"));
        let (site_id, client) = self.site_id_and_client_for(explicit_site);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match client.search_all_issues_jql(&jql, 200).await {
                Ok(collected) => {
                    let _ = storage.upsert_issues_for_site(&site_id, &collected.items);
                    let _ = tx.send(UiMsg::IssuesLoaded {
                        seq,
                        site_id,
                        issues: collected.items,
                        label,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssuesLoadFailed {
                        seq,
                        label,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_issue_detail(&mut self, ui: &mut UiState, key: &str) {
        self.request_issue_detail_for_site(ui, None, key);
    }

    pub fn request_issue_detail_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
    ) {
        self.request_issue_detail_internal(ui, explicit_site, key, None, true);
    }

    pub fn request_issue_detail_for_site_if_needed(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
        expected_updated_at: Option<&str>,
    ) -> bool {
        self.request_issue_detail_internal(ui, explicit_site, key, expected_updated_at, false)
    }

    fn request_issue_detail_internal(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
        expected_updated_at: Option<&str>,
        force_open: bool,
    ) -> bool {
        let resolved_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_issue_target
                    .as_ref()
                    .filter(|(_, k)| k == key)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                self.active_issue
                    .as_ref()
                    .filter(|d| d.summary.key == key)
                    .and_then(|_| self.active_issue_site_id.clone())
            })
            .or_else(|| {
                self.issues
                    .iter()
                    .position(|i| i.key == key)
                    .and_then(|idx| self.issue_site_ids.get(idx))
                    .cloned()
            });
        let (site_id, client) = self.site_id_and_client_for(resolved_site.as_deref());

        if !force_open {
            if let Some(ref inflight) = self.in_flight_issue_detail {
                if inflight.site_id.eq_ignore_ascii_case(&site_id) && inflight.key == key {
                    let already_covers = match (
                        expected_updated_at.map(str::trim).filter(|s| !s.is_empty()),
                        inflight.target_updated_at.as_deref(),
                    ) {
                        (None, _) => true,
                        (Some(inc), Some(cur)) => {
                            crate::storage::jira::compare_updated_at(inc, cur)
                                != std::cmp::Ordering::Greater
                        }
                        (Some(_), None) => false,
                    };
                    if already_covers {
                        return false;
                    }
                }
            }
        }

        let same_open_item = self
            .active_issue
            .as_ref()
            .is_some_and(|d| d.summary.key == key)
            && self
                .active_issue_site_id
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(&site_id))
                .unwrap_or_else(|| {
                    self.effective_site_id_and_client()
                        .0
                        .eq_ignore_ascii_case(&site_id)
                });

        if !same_open_item {
            if let Ok(Some(cached)) = self.storage.get_issue_detail_for_site(&site_id, key) {
                let detail_ts = self
                    .storage
                    .get_issue_detail_updated_at_for_site(&site_id, key)
                    .ok()
                    .flatten();
                self.active_issue = Some(cached);
                self.active_issue_detail_updated_at = detail_ts;
            } else {
                self.active_issue = None;
                self.active_issue_detail_updated_at = None;
            }
            ui.issue_ui.detail_scroll = 0;
            ui.board_ui.detail_scroll = 0;
        }
        self.active_issue_site_id = Some(site_id.clone());
        self.active_issue_target = Some((site_id.clone(), key.to_string()));
        self.issue_detail_req_seq = self.issue_detail_req_seq.wrapping_add(1);
        let seq = self.issue_detail_req_seq;
        let target_updated = expected_updated_at
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.storage
                    .get_issue_row_updated_at_for_site(&site_id, key)
                    .ok()
                    .flatten()
            });
        self.in_flight_issue_detail = Some(InFlightIssueDetail {
            site_id: site_id.clone(),
            key: key.to_string(),
            target_updated_at: target_updated,
            seq,
        });
        if tokio::runtime::Handle::try_current().is_err() {
            return true;
        }
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.get_issue_with_meta(&key_owned).await {
                Ok((mut detail, meta)) => {
                    let _ = storage.upsert_issue_detail_for_site_merging(
                        &site_id,
                        &mut detail,
                        !meta.remote_links_ok,
                    );
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        key: key_owned,
                        detail: Ok(detail),
                        not_found: false,
                    });
                }
                Err(e) => {
                    let not_found = is_not_found_error(&e);
                    if not_found {
                        let _ = storage.delete_issue_for_site(&site_id, &key_owned);
                    }
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        key: key_owned,
                        detail: Err(e.to_string()),
                        not_found,
                    });
                }
            }
        });
        true
    }

    pub fn request_open_transitions(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        issue_key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let mut overlay = TransitionOverlay::new(issue_key);
        overlay.site_id = site.clone();
        ui.push_overlay(Overlay::Transition(overlay));
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let seq = self
            .transitions_req_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let (resolved_site, client) = self.site_id_and_client_for(site.as_deref());
        let tx = self.ui_tx.clone();
        let key = issue_key.to_string();
        tokio::spawn(async move {
            let res = client.list_transitions(&key).await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::TransitionsLoaded {
                seq,
                site_id: resolved_site,
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
        self.request_transition_issue_for_site(ui, None, key, tr);
    }

    pub fn request_transition_issue_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        tr: IssueTransition,
    ) {
        let site = self.issue_write_site(site_id);
        // Capture previous status for clean rollback on API error.
        let prev_status = self.current_issue_status(site.as_deref(), key);
        // 0ms optimistic local column update so Kanban card moves right away!
        self.apply_local_status_update(site.as_deref(), key, &tr.to_status);
        self.clamp_selections(ui);
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} → {}", tr.to_status.name));
            return;
        }
        ui.tasks.begin(format!("Transition {key}"));
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.transition_issue(&key_owned, &tr.id, None).await {
                Ok(_) => {
                    let _ = storage.update_issue_status_for_site(
                        &site_id,
                        &key_owned,
                        &tr.to_status,
                    );
                    let _ = tx.send(UiMsg::IssueTransitioned {
                        site_id: site_id.clone(),
                        key: key_owned.clone(),
                        to_status: tr.to_status.clone(),
                        toast: format!("{key_owned} → {}", tr.to_status.name),
                    });
                    if let Ok((mut detail, meta)) = client.get_issue_with_meta(&key_owned).await {
                        let _ = storage.upsert_issue_detail_for_site_merging(
                            &site_id,
                            &mut detail,
                            !meta.remote_links_ok,
                        );
                        let _ = tx.send(UiMsg::IssueDetailLoaded {
                            seq: None,
                            site_id,
                            key: key_owned,
                            detail: Ok(detail),
                            not_found: false,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueTransitionFailed {
                        site_id,
                        key: key_owned,
                        previous_status: prev_status,
                        error: format!("Transition failed: {e}"),
                    });
                }
            }
        });
    }

    /// Site a write for an issue goes to: the explicit site carried by the
    /// overlay/row, else the site of the opened issue.
    fn issue_write_site(&self, explicit: Option<&str>) -> Option<String> {
        explicit
            .map(str::to_string)
            .or_else(|| self.active_issue_site_id.clone())
    }

    /// Finds the locally listed issue `key` on `site_id` (`None` = any site).
    fn find_local_issue(&self, site_id: Option<&str>, key: &str) -> Option<&IssueSummary> {
        if let Some(d) = self.active_issue.as_ref() {
            if d.summary.key == key && site_matches(self.active_issue_site_id.as_deref(), site_id) {
                return Some(&d.summary);
            }
        }
        if site_matches(self.board_issue_site_id.as_deref(), site_id) {
            if let Some(it) = self.board_issues.iter().find(|i| i.key == key) {
                return Some(it);
            }
        }
        let in_list = |list: &'_ [IssueSummary], sites: &'_ [String]| {
            list.iter().enumerate().find_map(|(idx, it)| {
                (it.key == key && site_matches(sites.get(idx).map(String::as_str), site_id))
                    .then_some(idx)
            })
        };
        if let Some(idx) = in_list(&self.issues, &self.issue_site_ids) {
            return self.issues.get(idx);
        }
        in_list(&self.my_issues, &self.my_issue_site_ids).and_then(|idx| self.my_issues.get(idx))
    }

    /// Applies `f` to every local copy of issue `key` that belongs to
    /// `site_id` (`None` = any site): board, list, My Work and the opened issue.
    fn for_each_local_issue_mut(
        &mut self,
        site_id: Option<&str>,
        key: &str,
        mut f: impl FnMut(&mut IssueSummary),
    ) {
        if site_matches(self.board_issue_site_id.as_deref(), site_id) {
            for it in self.board_issues.iter_mut().filter(|i| i.key == key) {
                f(it);
            }
        }
        for (idx, it) in self.issues.iter_mut().enumerate() {
            if it.key == key
                && site_matches(self.issue_site_ids.get(idx).map(String::as_str), site_id)
            {
                f(it);
            }
        }
        for (idx, it) in self.my_issues.iter_mut().enumerate() {
            if it.key == key
                && site_matches(self.my_issue_site_ids.get(idx).map(String::as_str), site_id)
            {
                f(it);
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key
                && site_matches(self.active_issue_site_id.as_deref(), site_id)
            {
                f(&mut detail.summary);
            }
        }
    }

    fn current_issue_status(&self, site_id: Option<&str>, key: &str) -> Option<IssueStatus> {
        self.find_local_issue(site_id, key).map(|i| i.status.clone())
    }

    fn current_issue_assignee(
        &self,
        site_id: Option<&str>,
        key: &str,
    ) -> Option<Option<UserSummary>> {
        self.find_local_issue(site_id, key).map(|i| i.assignee.clone())
    }

    fn apply_local_status_update(&mut self, site_id: Option<&str>, key: &str, to_status: &IssueStatus) {
        self.for_each_local_issue_mut(site_id, key, |it| it.status = to_status.clone());
    }

    fn apply_local_assignee_update(
        &mut self,
        site_id: Option<&str>,
        key: &str,
        assignee: Option<UserSummary>,
    ) {
        self.for_each_local_issue_mut(site_id, key, |it| it.assignee = assignee.clone());
    }

    fn bump_local_comment_count(&mut self, site_id: Option<&str>, key: &str, delta: isize) {
        self.for_each_local_issue_mut(site_id, key, |it| {
            if delta >= 0 {
                it.comment_count = it.comment_count.saturating_add(delta as usize);
            } else {
                it.comment_count = it.comment_count.saturating_sub((-delta) as usize);
            }
        });
    }

    pub fn request_assign_to_me(&mut self, ui: &mut UiState, key: &str) {
        self.request_assign_to_me_for_site(ui, None, key);
    }

    /// Identity of the signed-in user on `site_id`: the target client's own
    /// cached profile, the primary profile when the target *is* the primary site,
    /// or the persisted identity stored in SQLite for `site_id`. Account ids
    /// differ per site (ATL-25), so another site's "me" is never sent.
    fn known_me_for_site(&self, site_id: &str, client: &AtlassianClient) -> Option<UserSummary> {
        client
            .cached_me()
            .or_else(|| {
                site_id
                    .eq_ignore_ascii_case(self.client.site_id())
                    .then(|| self.me.clone().or_else(|| self.client.cached_me()))
                    .flatten()
            })
            .or_else(|| self.storage.identity_for_site(site_id).ok().flatten())
    }

    pub fn request_assign_to_me_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let known_me = self.known_me_for_site(&site_id, &client);
        let has_runtime = tokio::runtime::Handle::try_current().is_ok();
        if known_me.is_none() && !has_runtime {
            ui.error("Current user profile has not been loaded yet");
            return;
        }
        let prev_assignee = self.current_issue_assignee(Some(&site_id), key);
        if let Some(me) = &known_me {
            self.apply_local_assignee_update(Some(&site_id), key, Some(me.clone()));
        }
        if !has_runtime {
            ui.success(format!("Assigned {key} to you"));
            return;
        }
        let task_label = format!("Assign {key} to me");
        ui.tasks.begin(&task_label);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            let me = match known_me {
                Some(me) => me,
                None => match client.get_myself().await {
                    Ok(me) => {
                        client.set_cached_me(me.clone());
                        let _ = storage.save_identity_for_site(&site_id, &me);
                        me
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueAssignFailed {
                            site_id,
                            key: key_owned,
                            previous_assignee: None,
                            task_label,
                            error: format!("Could not resolve your account on this site: {e}"),
                        });
                        return;
                    }
                },
            };
            match client.assign_issue(&key_owned, Some(&me.id)).await {
                Ok(()) => {
                    let _ = storage.update_issue_assignee_for_site(&site_id, &key_owned, Some(&me));
                    match client.get_issue_with_meta(&key_owned).await {
                        Ok((mut detail, meta)) => {
                            let _ = storage.upsert_issue_detail_for_site_merging(
                                &site_id,
                                &mut detail,
                                !meta.remote_links_ok,
                            );
                            let _ = tx.send(UiMsg::IssueMutated {
                                site_id,
                                detail,
                                toast: format!("Assigned {key_owned} to you"),
                                clear_draft_key: None,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(UiMsg::IssueAssignFailed {
                                site_id,
                                key: key_owned,
                                previous_assignee: None,
                                task_label,
                                error: format!("Assigned issue, but failed to reload detail: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        site_id,
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to assign to me: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_open_assignee_picker(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        project_key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let mut overlay = AssigneeOverlay::new(key, project_key);
        overlay.site_id = site.clone();
        let lookup_key = overlay.project_or_issue_key.clone();
        ui.push_overlay(Overlay::Assignee(overlay));
        self.request_assignable_users(site.as_deref(), &lookup_key, "");
    }

    pub fn request_assignable_users(
        &self,
        site_id: Option<&str>,
        project_or_issue: &str,
        query: &str,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let seq = self
            .assignable_users_req_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let site = self.issue_write_site(site_id);
        let (resolved_site, client) = self.site_id_and_client_for(site.as_deref());
        let tx = self.ui_tx.clone();
        let key = project_or_issue.to_string();
        let q = query.to_string();
        tokio::spawn(async move {
            let res = client
                .search_assignable_users(&key, &q)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AssignableUsersLoaded {
                seq,
                site_id: resolved_site,
                query: q,
                users: res,
            });
        });
    }

    pub fn request_assign_user(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        user: Option<UserSummary>,
    ) {
        let site = self.issue_write_site(site_id);
        let prev_assignee = self.current_issue_assignee(site.as_deref(), key);
        let label = user
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| "Unassigned".to_string());
        self.apply_local_assignee_update(site.as_deref(), key, user.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} assignee → {label}"));
            return;
        }
        let task_label = format!("Assign {key}");
        ui.tasks.begin(&task_label);
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            let id_ref = user.as_ref().map(|u| u.id.as_str());
            match client.assign_issue(&key_owned, id_ref).await {
                Ok(()) => {
                    let _ = storage.update_issue_assignee_for_site(
                        &site_id,
                        &key_owned,
                        user.as_ref(),
                    );
                    match client.get_issue_with_meta(&key_owned).await {
                        Ok((mut detail, meta)) => {
                            let _ = storage.upsert_issue_detail_for_site_merging(
                                &site_id,
                                &mut detail,
                                !meta.remote_links_ok,
                            );
                            let _ = tx.send(UiMsg::IssueMutated {
                                site_id,
                                detail,
                                toast: format!("{key_owned} assignee → {label}"),
                                clear_draft_key: None,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(UiMsg::IssueAssignFailed {
                                site_id,
                                key: key_owned,
                                previous_assignee: None,
                                task_label,
                                error: format!("Updated assignee, but failed to reload detail: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        site_id,
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to update assignee: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_submit_issue_form(
        &mut self,
        ui: &mut UiState,
        form: &IssueFormOverlay,
        draft_key: Option<String>,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            return;
        }
        let site = self.issue_write_site(form.site_id.as_deref());
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(key) = form.edit_key.clone() {
            let patch = form.build_edit_patch();
            if patch.summary.is_none()
                && patch.description_markdown.is_none()
                && patch.priority.is_none()
                && patch.assignee_id.is_none()
                && patch.labels.is_none()
            {
                if let Some(dk) = draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                ui.info(format!("{key}: no fields changed"));
                return;
            }
            ui.tasks.begin(format!("Update {key}"));
            tokio::spawn(async move {
                match client.update_issue(&key, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            site_id,
                            detail,
                            toast: format!("Updated {key}"),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed {
                            task_label: format!("Update {key}"),
                            error: format!("Failed to update issue (draft preserved): {e}"),
                        });
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
                        let _ = storage.upsert_issue_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            site_id,
                            detail,
                            toast: format!("Created {k}"),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed {
                            task_label: "Create issue".to_string(),
                            error: format!("Failed to create issue (draft preserved): {e}"),
                        });
                    }
                }
            });
        }
    }

    pub fn request_submit_comment(
        &mut self,
        ui: &mut UiState,
        overlay: &CommentOverlay,
        draft_key: Option<String>,
    ) {
        self.request_add_comment_with_draft(
            ui,
            overlay.site_id.clone(),
            overlay.target.clone(),
            overlay.body.text.clone(),
            draft_key,
        );
    }

    pub fn request_add_comment(
        &mut self,
        ui: &mut UiState,
        target: CommentTarget,
        body: String,
    ) {
        self.request_add_comment_with_draft(ui, None, target, body, None);
    }

    fn request_add_comment_with_draft(
        &mut self,
        ui: &mut UiState,
        site: Option<String>,
        target: CommentTarget,
        body: String,
        draft_key: Option<String>,
    ) {
        let site = match &target {
            CommentTarget::Issue(_) => self.issue_write_site(site.as_deref()),
            CommentTarget::Page { .. } => site.or_else(|| self.active_page_site_id.clone()),
        };
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let optimistic_id = format!("optimistic:{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now().to_rfc3339();
        let author = self
            .known_me_for_site(&site_id, &client)
            .unwrap_or_else(|| UserSummary {
                id: "me".to_string(),
                display_name: "You".to_string(),
                ..Default::default()
            });
        let blocks = adf::parse_markdown(&body);

        match &target {
            CommentTarget::Issue(key) => {
                let opened_matches =
                    site_matches(self.active_issue_site_id.as_deref(), Some(&site_id));
                if let Some(detail) = self.active_issue.as_mut().filter(|_| opened_matches) {
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
                self.bump_local_comment_count(Some(&site_id), key, 1);
            }
            CommentTarget::Page { page_id, .. } => {
                let opened_matches =
                    site_matches(self.active_page_site_id.as_deref(), Some(&site_id));
                if let Some(page) = self.active_page.as_mut().filter(|_| opened_matches) {
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
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            ui.success("Comment posted");
            return;
        }

        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            CommentTarget::Issue(key) => {
                let task_label = format!("Comment on {key}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = match client.add_issue_comment(&key, &body).await {
                        Ok(comment) => {
                            let _ = storage.append_issue_comment_for_site(&site_id, &key, &comment);
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentAdded {
                        site_id,
                        issue_key: Some(key),
                        page_id: None,
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        clear_draft_key: draft_key,
                        result: res,
                    });
                });
            }
            CommentTarget::Page { page_id, title } => {
                let task_label = format!("Comment on {title}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = match client.add_page_comment(&page_id, &body).await {
                        Ok(comment) => {
                            let _ =
                                storage.append_page_comment_for_site(&site_id, &page_id, &comment);
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentAdded {
                        site_id,
                        issue_key: None,
                        page_id: Some(page_id),
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        clear_draft_key: draft_key,
                        result: res,
                    });
                });
            }
        }
    }

    pub fn request_space_root_pages(&mut self, ui: &mut UiState, space_key: &str) {
        self.request_space_root_pages_for_site(ui, None, space_key);
    }

    pub fn request_space_root_pages_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        space_key: &str,
    ) {
        let space_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_space(ui)
                    .filter(|s| s.key == space_key)
                    .and_then(|_| self.active_space_site_id(ui).map(str::to_string))
            })
            .or_else(|| {
                self.spaces
                    .iter()
                    .position(|s| s.key == space_key)
                    .and_then(|idx| self.space_site_ids.get(idx))
                    .cloned()
            });
        let (site_id, client) = self.site_id_and_client_for(space_site.as_deref());
        if let Ok(roots) = self
            .storage
            .list_space_root_pages_for_site(&site_id, space_key)
        {
            if !roots.is_empty() {
                self.root_pages_by_space
                    .insert(SiteKey::new(&site_id, space_key), roots);
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Space {space_key}"));
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let sk = space_key.to_string();
        tokio::spawn(async move {
            match sync::load_space_root_pages_for_site(&client, &storage, &site_id, &sk).await {
                Ok(pages) => {
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        site_id,
                        space_key: sk,
                        pages: Ok(pages),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        site_id,
                        space_key: sk,
                        pages: Err(e.to_string()),
                    });
                }
            }
        });
    }

    /// Fetches direct child pages for `(site_id, page_id)`. When `force` is `true`, bypasses
    /// the in-memory `page_children_map` cache so manual refresh / re-expand sees live changes.
    pub fn request_page_children_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        page_id: &str,
        title: &str,
        force: bool,
    ) {
        let (site_id, client) = self.site_id_and_client_for(Some(site_id));
        let node = SiteKey::new(&site_id, page_id);
        if !force && self.page_children_map.contains_key(&node) {
            return;
        }
        if !self.page_children_map.contains_key(&node) {
            if let Ok(cached) = self.storage.list_child_pages_for_site(&site_id, page_id) {
                if !cached.is_empty() {
                    self.page_children_map.insert(node, cached);
                }
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = if title.trim().is_empty() {
            format!("Child pages: #{page_id}")
        } else {
            format!("Child pages: {title}")
        };
        ui.tasks.begin(&task_label);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page_id.to_string();
        tokio::spawn(async move {
            match sync::load_child_pages_for_site(&client, &storage, &site_id, &pid).await {
                Ok(children) => {
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        site_id,
                        parent_id: pid,
                        task_label,
                        children: Ok(children),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        site_id,
                        parent_id: pid,
                        task_label,
                        children: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn toggle_expand_page(&mut self, ui: &mut UiState, page: &PageSummary) {
        // Same site as `current_confluence_tree`, so the toggled row is the one drawn.
        let tree_site = self.tree_site(ui);
        let (site_id, _) = self.site_id_and_client_for(Some(&tree_site));
        let node = SiteKey::new(&site_id, &page.id);
        if self.expanded_pages.remove(&node) {
            return;
        }
        self.expanded_pages.insert(node);
        // Re-expanding keeps cached children visible immediately while refreshing live children.
        self.request_page_children_for_site(ui, &site_id, &page.id, &page.title, true);
    }

    /// Toggles expand/collapse on a Confluence tree row (e.g. chevron click) without forcibly
    /// switching `active_page`.
    pub fn toggle_page_expand_only(&mut self, ui: &mut UiState, row_idx: usize) {
        let tree = self.current_confluence_tree(ui);
        if let Some(row) = tree.get(row_idx) {
            let page = row.page.clone();
            ui.confluence_ui.selected_tree_row = row_idx;
            self.toggle_expand_page(ui, &page);
            self.clamp_selections(ui);
        }
    }

    /// Context-aware manual refresh for the currently active workspace tab (called alongside
    /// workspace sync on `r` / `Action::SyncAll`).
    pub fn refresh_active_context(&mut self, ui: &mut UiState) {
        match ui.active_tab {
            WorkspaceTab::Board => {
                if self.active_board_with_site(ui).is_some() {
                    let sprint_id = self
                        .active_board_sprint_id
                        .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
                    self.request_board_load(ui, ui.board_ui.selected_board, sprint_id);
                }
                if ui.board_ui.drawer_open {
                    if let Some((sid, key)) = self.current_open_issue_identity() {
                        self.request_issue_detail_for_site(ui, Some(&sid), &key);
                    }
                }
            }
            WorkspaceTab::Issues => {
                let jql = ui.issue_ui.jql.text.trim().to_string();
                let non_default_jql = !jql.is_empty() && jql != "ORDER BY updated DESC";
                if (self.issues_custom_query || non_default_jql) && !jql.is_empty() {
                    let label = self
                        .active_jql_label
                        .clone()
                        .unwrap_or_else(|| "Custom JQL".to_string());
                    let site = self.active_jql_site_id.clone();
                    self.request_jql_search_for_site(ui, site.as_deref(), jql, label);
                }
                if let Some((sid, key)) = self.current_open_issue_identity() {
                    self.request_issue_detail_for_site(ui, Some(&sid), &key);
                }
            }
            WorkspaceTab::MyWork => {
                self.request_my_work_refresh(ui);
            }
            WorkspaceTab::Confluence => {
                if let Some(sp) = self.active_space(ui).cloned() {
                    let space_site = self.tree_site(ui);
                    self.request_space_root_pages_for_site(ui, Some(&space_site), &sp.key);
                    let expanded_nodes: Vec<(String, String)> = self
                        .current_confluence_tree(ui)
                        .into_iter()
                        .filter(|row| row.expanded)
                        .map(|row| (row.page.id, row.page.title))
                        .collect();
                    for (pid, title) in expanded_nodes {
                        self.request_page_children_for_site(ui, &space_site, &pid, &title, true);
                    }
                }
                if let Some((sid, pid)) = self.current_open_page_identity() {
                    self.request_page_detail_for_site(ui, Some(&sid), &pid);
                }
            }
            WorkspaceTab::Search => {
                let q = ui.search_ui.query.text.clone();
                if ui.search_ui.is_remote {
                    self.request_remote_search(ui, &q);
                } else {
                    self.refresh_local_search(&q);
                    self.clamp_selections(ui);
                }
            }
            WorkspaceTab::Directory => {
                self.refresh_scoped_cache_counts();
            }
        }
    }

    pub fn request_page_detail(&mut self, ui: &mut UiState, page_id: &str) {
        self.request_page_detail_for_site(ui, None, page_id);
    }

    pub fn request_page_detail_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
    ) {
        self.request_page_detail_internal(ui, explicit_site, page_id, None, true);
    }

    pub fn request_page_detail_for_site_if_needed(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
        expected_version: Option<i32>,
    ) -> bool {
        self.request_page_detail_internal(ui, explicit_site, page_id, expected_version, false)
    }

    fn request_page_detail_internal(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
        expected_version: Option<i32>,
        force_open: bool,
    ) -> bool {
        let resolved_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_page_target
                    .as_ref()
                    .filter(|(_, p)| p == page_id)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                self.active_page
                    .as_ref()
                    .filter(|p| p.summary.id == page_id)
                    .and_then(|_| self.active_page_site_id.clone())
            })
            .or_else(|| self.active_space_site_id(ui).map(str::to_string));
        let (site_id, client) = self.site_id_and_client_for(resolved_site.as_deref());

        if !force_open {
            if let Some(ref inflight) = self.in_flight_page_detail {
                if inflight.site_id.eq_ignore_ascii_case(&site_id) && inflight.page_id == page_id {
                    let already_covers = match (expected_version, inflight.target_version) {
                        (None, _) => true,
                        (Some(inc), Some(cur)) => cur >= inc,
                        (Some(_), None) => false,
                    };
                    if already_covers {
                        return false;
                    }
                }
            }
        }

        let same_open_page = self
            .active_page
            .as_ref()
            .is_some_and(|p| p.summary.id == page_id)
            && self
                .active_page_site_id
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(&site_id))
                .unwrap_or_else(|| {
                    self.effective_site_id_and_client()
                        .0
                        .eq_ignore_ascii_case(&site_id)
                });

        if !same_open_page {
            if let Ok(Some(cached)) = self.storage.get_page_detail_for_site(&site_id, page_id) {
                let detail_ver = self
                    .storage
                    .get_page_detail_version_for_site(&site_id, page_id)
                    .ok()
                    .flatten();
                self.active_page = Some(cached);
                self.active_page_detail_version = detail_ver;
            } else {
                self.active_page = None;
                self.active_page_detail_version = None;
            }
            ui.confluence_ui.reader_scroll = 0;
        }
        self.active_page_site_id = Some(site_id.clone());
        self.active_page_target = Some((site_id.clone(), page_id.to_string()));
        self.page_detail_req_seq = self.page_detail_req_seq.wrapping_add(1);
        let seq = self.page_detail_req_seq;
        let target_ver = expected_version.or_else(|| {
            self.storage
                .get_page_row_version_for_site(&site_id, page_id)
                .ok()
                .flatten()
        });
        self.in_flight_page_detail = Some(InFlightPageDetail {
            site_id: site_id.clone(),
            page_id: page_id.to_string(),
            target_version: target_ver,
            seq,
        });
        if tokio::runtime::Handle::try_current().is_err() {
            return true;
        }
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page_id.to_string();
        tokio::spawn(async move {
            match client
                .get_page_with_meta_in_cache(&pid, &site_id, &storage)
                .await
            {
                Ok((mut detail, meta)) => {
                    let _ =
                        storage.upsert_page_detail_for_site_with_meta(&site_id, &mut detail, meta);
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        page_id: pid,
                        detail: Ok(detail),
                        not_found: false,
                    });
                }
                Err(e) => {
                    let not_found = is_not_found_error(&e);
                    if not_found {
                        let _ = storage.delete_page_for_site(&site_id, &pid);
                    }
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        page_id: pid,
                        detail: Err(e.to_string()),
                        not_found,
                    });
                }
            }
        });
        true
    }

    /// Targeted attachment refresh for the currently open page or issue (used when opening
    /// `AttachmentsOverlay`, pressing `r` inside it, or returning focus while it is visible).
    pub fn request_attachments_refresh(&self, ui: &UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            OpenItemTarget::ConfluencePage { site_id, page_id } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_page_attachments(&page_id).await {
                        Ok(atts) => {
                            let _ = storage.update_page_attachments_for_site(&sid, &page_id, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentsRefreshed {
                        site_id: sid,
                        issue_key: None,
                        page_id: Some(page_id),
                        attachments: res,
                    });
                });
            }
            OpenItemTarget::JiraIssue { site_id, key } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_issue_attachments(&key).await {
                        Ok(atts) => {
                            let _ = storage.update_issue_attachments_for_site(&sid, &key, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentsRefreshed {
                        site_id: sid,
                        issue_key: Some(key),
                        page_id: None,
                        attachments: res,
                    });
                });
            }
        }
    }

    /// Targeted comments refresh for the currently open page or issue (used when opening
    /// `CommentOverlay` or returning focus while it is visible, without touching unsent text).
    pub fn request_comments_refresh(&self, ui: &UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            OpenItemTarget::ConfluencePage { site_id, page_id } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_page_comments(&page_id).await {
                        Ok(comments) => {
                            let _ = storage.update_page_comments_for_site(&sid, &page_id, &comments);
                            Ok(comments)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentsRefreshed {
                        site_id: sid,
                        issue_key: None,
                        page_id: Some(page_id),
                        issue_comments: None,
                        issue_comment_total: None,
                        page_comments: Some(res),
                    });
                });
            }
            OpenItemTarget::JiraIssue { site_id, key } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let (res, total) = match client.list_issue_comments_collected(&key).await {
                        Ok((collected, total)) => {
                            // The server total keeps comment_count right past the comment cap.
                            let _ = storage.update_issue_comments_for_site_with_total(
                                &sid,
                                &key,
                                &collected.items,
                                total,
                            );
                            (Ok(collected.items), total)
                        }
                        Err(e) => (Err(e.to_string()), None),
                    };
                    let _ = tx.send(UiMsg::CommentsRefreshed {
                        site_id: sid,
                        issue_key: Some(key),
                        page_id: None,
                        issue_comments: Some(res),
                        issue_comment_total: total,
                        page_comments: None,
                    });
                });
            }
        }
    }

    pub fn request_submit_page_form(
        &mut self,
        ui: &mut UiState,
        form: &PageFormOverlay,
        draft_key: Option<String>,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            return;
        }
        let site = form
            .site_id
            .clone()
            .or_else(|| self.active_page_site_id.clone());
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(pid) = form.edit_page_id.clone() {
            let body_unchanged = form.body.text == form.initial_body;
            if form.title.text.trim() == form.initial_title.trim() && body_unchanged {
                if let Some(key) = draft_key.as_deref() {
                    self.clear_draft_and_save(key);
                }
                return;
            }
            ui.tasks.begin("Update page");
            let patch = PageUpdate {
                title: form.title.text.trim().to_string(),
                body_markdown: form.body.text.clone(),
                body_raw_passthrough: body_unchanged
                    .then(|| form.initial_body_raw.clone())
                    .flatten(),
                body_representation: body_unchanged
                    .then(|| form.initial_body_representation.clone())
                    .flatten(),
                prev_version: form.prev_version,
                version_message: Some("Edited via atlassian-cli TUI".to_string()),
            };
            tokio::spawn(async move {
                match client.update_page(&pid, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_page_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            site_id,
                            detail,
                            toast: "Confluence page saved".to_string(),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed {
                            task_label: "Update page".to_string(),
                            error: format!("Failed to update page (draft preserved): {e}"),
                        });
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
                        let _ = storage.upsert_page_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            site_id,
                            detail,
                            toast: "Created Confluence page".to_string(),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed {
                            task_label: "Create page".to_string(),
                            error: format!("Failed to create page (draft preserved): {e}"),
                        });
                    }
                }
            });
        }
    }

    /// Refreshes My Work when switching to the My Work tab if never live-fetched this session
    /// or older than 30 seconds (not only when lists are empty).
    pub fn maybe_refresh_my_work_on_tab_switch(&mut self, ui: &mut UiState) {
        const MY_WORK_STALE_SECS: u64 = 30;
        let is_stale = self
            .last_my_work_refresh_at
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(MY_WORK_STALE_SECS));
        if is_stale || (self.my_issues.is_empty() && self.my_pages.is_empty()) {
            self.request_my_work_refresh(ui);
        }
    }

    pub fn request_my_work_refresh(&mut self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.my_work_req_seq = self.my_work_req_seq.wrapping_add(1);
        let seq = self.my_work_req_seq;
        self.last_my_work_refresh_at = Some(Instant::now());
        ui.tasks.begin("Loading My Work");
        let clients: Vec<(String, String, Option<String>, AtlassianClient)> = match &self.site_scope {
            SiteScope::Only(id) => {
                let c = self
                    .site_pool
                    .client_for(id)
                    .unwrap_or_else(|_| self.client.clone());
                let me_id = self.known_me_for_site(id, &c).map(|m| m.id);
                vec![(id.clone(), self.site_display_label(id), me_id, c)]
            }
            SiteScope::All => {
                let enabled = self.site_pool.enabled_clients();
                if enabled.is_empty() {
                    let sid = self.client.site_id().to_string();
                    let me_id = self.known_me_for_site(&sid, &self.client).map(|m| m.id);
                    vec![(
                        sid.clone(),
                        self.site_display_label(&sid),
                        me_id,
                        self.client.clone(),
                    )]
                } else {
                    enabled
                        .into_iter()
                        .map(|(s, c)| {
                            let me_id = self.known_me_for_site(&s.id, &c).map(|m| m.id);
                            (s.id.clone(), s.display_name().to_string(), me_id, c)
                        })
                        .collect()
                }
            }
        };
        let show_pills = self.site_scope.is_all() && clients.len() > 1;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let mut all_issues: Vec<(String, Option<String>, IssueSummary)> = Vec::new();
            let mut all_pages: Vec<(String, Option<String>, PageSummary)> = Vec::new();
            for (site_id, site_label, me_id, client) in clients {
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
                        .list_my_work_for_site(&site_id, me_id.as_deref(), 40)
                        .unwrap_or_default(),
                };
                for issue in issues {
                    all_issues.push((site_id.clone(), pill.clone(), issue));
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
                    all_pages.push((site_id.clone(), pill.clone(), page));
                }
            }
            all_issues.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            all_pages.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            let mut issue_site_ids = Vec::with_capacity(all_issues.len());
            let mut issue_site_labels = Vec::with_capacity(all_issues.len());
            let mut issues = Vec::with_capacity(all_issues.len());
            for (sid, lbl, item) in all_issues {
                issue_site_ids.push(sid);
                issue_site_labels.push(lbl);
                issues.push(item);
            }
            let mut page_site_ids = Vec::with_capacity(all_pages.len());
            let mut page_site_labels = Vec::with_capacity(all_pages.len());
            let mut pages = Vec::with_capacity(all_pages.len());
            for (sid, lbl, item) in all_pages {
                page_site_ids.push(sid);
                page_site_labels.push(lbl);
                pages.push(item);
            }
            let _ = tx.send(UiMsg::MyWorkLoaded {
                seq,
                issues,
                issue_site_ids,
                issue_site_labels,
                pages,
                page_site_ids,
                page_site_labels,
            });
        });
    }

    pub fn request_remote_search(&mut self, ui: &mut UiState, query: &str) {
        let q = query.trim().to_string();
        if q.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.remote_search_req_seq = self.remote_search_req_seq.wrapping_add(1);
        let seq = self.remote_search_req_seq;
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
            let _ = tx.send(UiMsg::RemoteSearchLoaded {
                seq,
                query: q,
                hits: res,
            });
        });
    }

    pub fn active_upload_target(&self, ui: &UiState) -> Option<UploadTarget> {
        self.active_upload_ref(ui).map(|(_, target)| target)
    }

    /// Upload target plus the site it lives on (the selected issue row's site,
    /// or the opened page's site on the Confluence tab).
    pub fn active_upload_ref(&self, ui: &UiState) -> Option<(Option<String>, UploadTarget)> {
        if ui.active_tab == WorkspaceTab::Confluence {
            if let Some(page) = &self.active_page {
                return Some((
                    self.active_page_site_id.clone(),
                    UploadTarget::Page(page.summary.id.clone()),
                ));
            }
        }
        self.selected_issue_ref(ui)
            .map(|(site, key)| (Some(site), UploadTarget::Issue(key)))
    }

    pub fn request_upload_attachment(&self, ui: &mut UiState, overlay: &PathInputOverlay) {
        let path = PathBuf::from(overlay.path.text.trim());
        self.request_upload_file(ui, overlay.site_id.as_deref(), overlay.target.clone(), path);
    }

    pub fn request_upload_file(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: UploadTarget,
        path: PathBuf,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Upload attachment");
        let explicit_site = site_id.or(match &target {
            UploadTarget::Issue(_) => self.active_issue_site_id.as_deref(),
            UploadTarget::Page(_) => self.active_page_site_id.as_deref(),
        });
        let (site_id, client) = self.site_id_and_client_for(explicit_site);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            UploadTarget::Issue(key) => {
                tokio::spawn(async move {
                    let res = match client.upload_issue_attachment(&key, &path).await {
                        Ok(atts) => {
                            let _ =
                                storage.append_issue_attachments_for_site(&site_id, &key, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        site_id,
                        issue_key: Some(key),
                        page_id: None,
                        result: res,
                    });
                });
            }
            UploadTarget::Page(id) => {
                tokio::spawn(async move {
                    let res = match client.upload_page_attachment(&id, &path).await {
                        Ok(atts) => {
                            let _ =
                                storage.append_page_attachments_for_site(&site_id, &id, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        site_id,
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
        let task_label = format!("Download: {}", att.filename);
        ui.tasks.begin(&task_label);
        let explicit_site = self.current_attachments_site(ui);
        let (_, client) = self.site_id_and_client_for(explicit_site.as_deref());
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = client
                .download_attachment(&att, &dest)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AttachmentDownloaded {
                task_label,
                result: res,
            });
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
        let fit = ov.fit_box(ui.term_width, ui.term_height);
        let cache_key = ov.preview_image_cache_key(&att, ui.term_width, ui.term_height);
        if self.image_cache.contains_key(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let (_, client) = self.site_id_and_client_for(ov.site_id.as_deref());
        let tx = self.ui_tx.clone();
        let caps = ov.graphics_caps();
        let bg_rgb = img_preview::color_to_rgb(
            ui.theme.bg_elevated,
            img_preview::DEFAULT_PREVIEW_BG_RGB,
        );
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
                    let res = img_preview::decode_and_render_halfblocks_with_cell_px(
                        &bytes,
                        fit,
                        bg_rgb,
                        caps.cell_px,
                    )
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
        site_id: Option<&str>,
        title: String,
        att: AttachmentItem,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (_, client) = self.site_id_and_client_for(site_id);
        let tx = self.ui_tx.clone();
        let source_url = att.content_url.clone();
        let task_label = format!("HTML: {}", att.filename);
        ui.tasks.begin(&task_label);
        tokio::spawn(async move {
            let result = match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(err) => Err(err.to_string()),
            };
            let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                title,
                source_url,
                task_label,
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

    fn maybe_refresh_open_items_after_sync(&mut self, ui: &mut UiState) {
        if let Some((sid, key)) = self.current_open_issue_identity() {
            let stale = self
                .storage
                .is_issue_detail_stale_for_site(&sid, &key)
                .unwrap_or(false);
            if stale {
                let row_updated = self
                    .storage
                    .get_issue_row_updated_at_for_site(&sid, &key)
                    .ok()
                    .flatten();
                self.request_issue_detail_for_site_if_needed(
                    ui,
                    Some(&sid),
                    &key,
                    row_updated.as_deref(),
                );
            }
        }
        if let Some((sid, pid)) = self.current_open_page_identity() {
            let stale = self
                .storage
                .is_page_detail_stale_for_site(&sid, &pid)
                .unwrap_or(false);
            if stale {
                let row_ver = self
                    .storage
                    .get_page_row_version_for_site(&sid, &pid)
                    .ok()
                    .flatten();
                self.request_page_detail_for_site_if_needed(ui, Some(&sid), &pid, row_ver);
            }
        }
    }

    fn apply_issue_summary_to_lists(&mut self, site_id: &str, summary: &IssueSummary) {
        if self
            .board_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or(true)
        {
            if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == summary.key) {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
        for (idx, it) in self.issues.iter_mut().enumerate() {
            let row_matches_site = self
                .issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if row_matches_site && it.key == summary.key {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
        for (idx, it) in self.my_issues.iter_mut().enumerate() {
            let row_matches_site = self
                .my_issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if row_matches_site && it.key == summary.key {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
    }

    fn issue_revision_needs_detail(&self, site_id: &str, incoming: &IssueSummary) -> bool {
        let Some(cur) = self.active_issue.as_ref() else {
            return true;
        };
        let site_matches = self
            .active_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or_else(|| {
                self.effective_site_id_and_client()
                    .0
                    .eq_ignore_ascii_case(site_id)
            });
        if cur.summary.key != incoming.key || !site_matches {
            return true;
        }
        if self
            .storage
            .is_issue_detail_stale_for_site(site_id, &incoming.key)
            .unwrap_or(false)
        {
            return true;
        }
        match self.active_issue_detail_updated_at.as_deref() {
            None => return true,
            Some(detail_ts) => {
                if crate::storage::jira::compare_updated_at(&incoming.updated_at, detail_ts)
                    == std::cmp::Ordering::Greater
                {
                    return true;
                }
            }
        }
        incoming.summary != cur.summary.summary
            || incoming.status != cur.summary.status
            || incoming.priority != cur.summary.priority
            || incoming.assignee != cur.summary.assignee
            || incoming.labels != cur.summary.labels
    }

    fn page_revision_needs_detail(&self, site_id: &str, incoming: &PageSummary) -> bool {
        let Some(cur) = self.active_page.as_ref() else {
            return true;
        };
        let site_matches = self
            .active_page_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or_else(|| {
                self.effective_site_id_and_client()
                    .0
                    .eq_ignore_ascii_case(site_id)
            });
        if cur.summary.id != incoming.id || !site_matches {
            return true;
        }
        if self
            .storage
            .is_page_detail_stale_for_site(site_id, &incoming.id)
            .unwrap_or(false)
        {
            return true;
        }
        match self.active_page_detail_version {
            None => return true,
            Some(detail_ver) if incoming.version_number > detail_ver => return true,
            _ => {}
        }
        incoming.title != cur.summary.title
            || crate::storage::jira::compare_updated_at(&incoming.updated_at, &cur.summary.updated_at)
                == std::cmp::Ordering::Greater
    }

    fn purge_deleted_issue(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        key: &str,
        notify_toast: bool,
    ) {
        let _ = self.storage.delete_issue_for_site(site_id, key);
        let mut idx = 0usize;
        while idx < self.issues.len() {
            let matches_site = self
                .issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.issues[idx].key == key {
                self.issues.remove(idx);
                if idx < self.issue_site_ids.len() {
                    self.issue_site_ids.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
        if self
            .board_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or(true)
        {
            self.board_issues.retain(|i| i.key != key);
        }
        self.remove_my_issue(site_id, key);
        let mut hit_idx = 0usize;
        while hit_idx < self.search_hits.len() {
            let matches_site = self
                .search_hit_site_ids
                .get(hit_idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site
                && self.search_hits[hit_idx].kind == crate::models::SearchHitKind::JiraIssue
                && self.search_hits[hit_idx].id_or_key == key
            {
                self.search_hits.remove(hit_idx);
                if hit_idx < self.search_hit_site_ids.len() {
                    self.search_hit_site_ids.remove(hit_idx);
                }
                if hit_idx < self.search_hit_site_labels.len() {
                    self.search_hit_site_labels.remove(hit_idx);
                }
            } else {
                hit_idx += 1;
            }
        }
        let cleared_active = self.active_issue.as_ref().is_some_and(|cur| {
            cur.summary.key == key
                && self
                    .active_issue_site_id
                    .as_deref()
                    .map(|s| s.eq_ignore_ascii_case(site_id))
                    .unwrap_or(true)
        }) || self.active_issue_target.as_ref().is_some_and(|(s, k)| {
            k == key && s.eq_ignore_ascii_case(site_id)
        });
        if cleared_active {
            self.active_issue = None;
            self.active_issue_site_id = None;
            self.active_issue_target = None;
            self.active_issue_detail_updated_at = None;
            self.in_flight_issue_detail = None;
            if notify_toast {
                ui.info(format!("{key} was deleted remotely and removed from cache"));
            }
        }
        self.refresh_scoped_cache_counts();
        self.clamp_selections(ui);
        self.sync_open_item_watch(ui);
    }

    fn purge_deleted_page(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        page_id: &str,
        notify_toast: bool,
    ) {
        let _ = self.storage.delete_page_for_site(site_id, page_id);
        for (k, roots) in self.root_pages_by_space.iter_mut() {
            if k.is_site(site_id) {
                roots.retain(|p| p.id != page_id);
            }
        }
        for (k, children) in self.page_children_map.iter_mut() {
            if k.is_site(site_id) {
                children.retain(|p| p.id != page_id);
            }
        }
        self.page_children_map
            .remove(&SiteKey::new(site_id, page_id));
        self.expanded_pages.remove(&SiteKey::new(site_id, page_id));
        self.remove_my_page(site_id, page_id);
        let mut hit_idx = 0usize;
        while hit_idx < self.search_hits.len() {
            let matches_site = self
                .search_hit_site_ids
                .get(hit_idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site
                && self.search_hits[hit_idx].kind == crate::models::SearchHitKind::ConfluencePage
                && self.search_hits[hit_idx].id_or_key == page_id
            {
                self.search_hits.remove(hit_idx);
                if hit_idx < self.search_hit_site_ids.len() {
                    self.search_hit_site_ids.remove(hit_idx);
                }
                if hit_idx < self.search_hit_site_labels.len() {
                    self.search_hit_site_labels.remove(hit_idx);
                }
            } else {
                hit_idx += 1;
            }
        }
        let cleared_active = self.active_page.as_ref().is_some_and(|cur| {
            cur.summary.id == page_id
                && self
                    .active_page_site_id
                    .as_deref()
                    .map(|s| s.eq_ignore_ascii_case(site_id))
                    .unwrap_or(true)
        }) || self.active_page_target.as_ref().is_some_and(|(s, p)| {
            p == page_id && s.eq_ignore_ascii_case(site_id)
        });
        if cleared_active {
            self.active_page = None;
            self.active_page_site_id = None;
            self.active_page_target = None;
            self.active_page_detail_version = None;
            self.in_flight_page_detail = None;
            if notify_toast {
                ui.info(format!(
                    "Page #{page_id} was deleted remotely and removed from cache"
                ));
            }
        }
        self.refresh_scoped_cache_counts();
        self.clamp_selections(ui);
        self.sync_open_item_watch(ui);
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::SyncStarted => {
                self.syncing = true;
            }
            CoreEvent::SyncFinished { summary } => {
                self.syncing = false;
                self.sync_summary = summary.clone();
                self.refresh_scoped_cache_counts();
                self.reload_my_issues_from_storage();
                self.reload_my_pages_from_storage();
                self.maybe_refresh_open_items_after_sync(ui);
                self.clamp_selections(ui);
                ui.tasks.finish("Sync", None);
                ui.success(summary);
            }
            CoreEvent::ProjectsUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.project_site_ids = vec![sid; items.len()];
                    self.projects = items;
                } else {
                    self.reload_projects_all_sites();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::BoardsUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.board_site_ids = vec![sid; items.len()];
                    self.boards = items;
                } else if let Ok(all) = self.storage.list_boards_all_sites(None) {
                    let filtered: Vec<(String, BoardSummary)> = all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.board_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.boards = filtered.into_iter().map(|(_, b)| b).collect();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_issue_identity().and_then(|(open_sid, open_key)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    issues.iter().find(|i| i.key == open_key).and_then(|inc| {
                        self.issue_revision_needs_detail(&effective_sid, inc)
                            .then(|| (open_sid, open_key, inc.updated_at.clone()))
                    })
                });
                let matches_active_board = self.active_board(ui).is_none_or(|b| b.id == board_id)
                    && self
                        .active_board_site_id(ui)
                        .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                        .unwrap_or_else(|| self.site_scope.matches(&effective_sid));
                let selected_sprint_id = self
                    .active_board_sprint_id
                    .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
                let incoming_sprint_id = sprint.as_ref().map(|s| s.id);
                let sprint_matches = match (selected_sprint_id, incoming_sprint_id) {
                    (Some(sel), Some(inc)) => sel == inc,
                    (Some(sel), None) => self
                        .sprints
                        .iter()
                        .find(|s| s.id == sel)
                        .is_none_or(|s| s.state.eq_ignore_ascii_case("active")),
                    (None, _) => true,
                };
                if matches_active_board {
                    if let Some(sp) = sprint {
                        if !self.sprints.iter().any(|s| s.id == sp.id) {
                            self.sprints.push(sp);
                        }
                    }
                    if sprint_matches {
                        self.board_issues = issues;
                        self.board_issue_site_id = Some(effective_sid);
                        if incoming_sprint_id.is_some() {
                            self.active_board_sprint_id = incoming_sprint_id;
                        }
                    }
                    self.clamp_selections(ui);
                }
                if let Some((sid, key, updated_at)) = active_refresh {
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&sid),
                        &key,
                        Some(&updated_at),
                    );
                }
            }
            CoreEvent::IssuesUpdated { site_id, issues } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_issue_identity().and_then(|(open_sid, open_key)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    issues.iter().find(|i| i.key == open_key).and_then(|inc| {
                        self.issue_revision_needs_detail(&effective_sid, inc)
                            .then(|| (open_sid, open_key, inc.updated_at.clone()))
                    })
                });
                if self.site_scope.matches(&effective_sid) {
                    if !self.issues_custom_query {
                        if self.is_multi_site_all_mode() {
                            if let Ok(all) = self.storage.list_issues_all_sites(None, 200) {
                                let filtered: Vec<(String, IssueSummary)> = all
                                    .into_iter()
                                    .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                                    .collect();
                                self.issue_site_ids =
                                    filtered.iter().map(|(s, _)| s.clone()).collect();
                                self.issues = filtered.into_iter().map(|(_, i)| i).collect();
                            }
                        } else {
                            self.issue_site_ids = vec![effective_sid.clone(); issues.len()];
                            self.issues = issues;
                        }
                    } else {
                        // Preserve custom JQL / Saved Filter rows, updating any matching issues in place.
                        for inc in &issues {
                            for (idx, it) in self.issues.iter_mut().enumerate() {
                                let row_matches_site = self
                                    .issue_site_ids
                                    .get(idx)
                                    .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                                    .unwrap_or(true);
                                if row_matches_site && it.key == inc.key {
                                    let prev_comments = it.comment_count;
                                    let prev_atts = it.attachment_count;
                                    *it = inc.clone();
                                    if it.comment_count == 0 && prev_comments > 0 {
                                        it.comment_count = prev_comments;
                                    }
                                    if it.attachment_count == 0 && prev_atts > 0 {
                                        it.attachment_count = prev_atts;
                                    }
                                }
                            }
                        }
                    }
                    self.reload_my_issues_from_storage();
                    self.clamp_selections(ui);
                }
                if let Some((sid, key, updated_at)) = active_refresh {
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&sid),
                        &key,
                        Some(&updated_at),
                    );
                }
            }
            CoreEvent::SpacesUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.space_site_ids = vec![sid; items.len()];
                    self.spaces = items;
                } else if let Ok(all) = self.storage.list_spaces_all_sites() {
                    let filtered: Vec<(String, SpaceSummary)> = all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.space_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.spaces = filtered.into_iter().map(|(_, s)| s).collect();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_page_identity().and_then(|(open_sid, open_pid)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    pages.iter().find(|p| p.id == open_pid).and_then(|inc| {
                        self.page_revision_needs_detail(&effective_sid, inc)
                            .then(|| (open_sid, open_pid, inc.version_number))
                    })
                });
                let matches_space_site = self
                    .active_space(ui)
                    .filter(|sp| sp.key == space_key)
                    .and_then(|_| self.active_space_site_id(ui))
                    .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                    .unwrap_or_else(|| self.site_scope.matches(&effective_sid));
                if matches_space_site {
                    self.root_pages_by_space
                        .insert(SiteKey::new(&effective_sid, &space_key), pages);
                }
                if self.site_scope.matches(&effective_sid) {
                    self.reload_my_pages_from_storage();
                }
                self.clamp_selections(ui);
                if let Some((sid, pid, ver)) = active_refresh {
                    self.request_page_detail_for_site_if_needed(ui, Some(&sid), &pid, Some(ver));
                }
            }
            CoreEvent::OpenIssueRevision { site_id, summary } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                if sync::is_open_issue_not_found(&summary) {
                    self.purge_deleted_issue(ui, &effective_sid, &summary.key, true);
                    return;
                }
                let matches_open = self.current_open_issue_identity().is_some_and(|(sid, key)| {
                    sid.eq_ignore_ascii_case(&effective_sid) && key == summary.key
                });
                if !matches_open {
                    // Late response after site/item switch: persist strictly to original site cache only.
                    let _ = self
                        .storage
                        .upsert_issue_revision_for_site(&effective_sid, &summary);
                    return;
                }
                if self.issue_revision_needs_detail(&effective_sid, &summary) {
                    let _ = self
                        .storage
                        .upsert_issue_revision_for_site(&effective_sid, &summary);
                    self.apply_issue_summary_to_lists(&effective_sid, &summary);
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&effective_sid),
                        &summary.key,
                        Some(&summary.updated_at),
                    );
                }
            }
            CoreEvent::OpenPageRevision { site_id, summary } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                if sync::is_open_page_not_found(&summary) {
                    self.purge_deleted_page(ui, &effective_sid, &summary.id, true);
                    return;
                }
                let matches_open = self.current_open_page_identity().is_some_and(|(sid, pid)| {
                    sid.eq_ignore_ascii_case(&effective_sid) && pid == summary.id
                });
                if !matches_open {
                    // Late response after site/item switch: persist strictly to original site cache only.
                    let _ = self
                        .storage
                        .upsert_authoritative_pages_for_site(
                            &effective_sid,
                            std::slice::from_ref(&summary),
                        );
                    return;
                }
                if self.page_revision_needs_detail(&effective_sid, &summary) {
                    let _ = self
                        .storage
                        .upsert_authoritative_pages_for_site(
                            &effective_sid,
                            std::slice::from_ref(&summary),
                        );
                    self.request_page_detail_for_site_if_needed(
                        ui,
                        Some(&effective_sid),
                        &summary.id,
                        Some(summary.version_number),
                    );
                }
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
                    self.reload_cached_preserving_ui(ui);
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
                self.clamp_selections(ui);
            }
            UiMsg::BoardsLoaded(Err(e)) => ui.error(e),
            UiMsg::BoardDataLoaded {
                seq,
                site_id,
                board_id,
                requested_sprint_id,
                columns,
                sprints,
                issues,
            } => {
                ui.tasks.finish(&format!("Board #{board_id}"), None);
                if seq != self.board_data_req_seq || !self.site_scope.matches(&site_id) {
                    return;
                }
                let still_active = self.active_board(ui).is_some_and(|b| b.id == board_id)
                    && self
                        .active_board_site_id(ui)
                        .is_none_or(|s| s.eq_ignore_ascii_case(&site_id));
                if !still_active {
                    return;
                }
                if !columns.is_empty() {
                    if self.board_site_ids.len() == self.boards.len() {
                        if let Some((b, _)) = self
                            .boards
                            .iter_mut()
                            .zip(self.board_site_ids.iter())
                            .find(|(b, b_sid)| {
                                b.id == board_id && b_sid.eq_ignore_ascii_case(&site_id)
                            })
                        {
                            b.columns = columns;
                        }
                    } else if let Some(b) = self.boards.iter_mut().find(|b| b.id == board_id) {
                        b.columns = columns;
                    }
                }
                let effective_sprint = requested_sprint_id.or_else(|| {
                    sprints
                        .iter()
                        .find(|s| s.state.eq_ignore_ascii_case("active"))
                        .map(|s| s.id)
                });
                self.sprints = sprints;
                self.board_issues = issues;
                self.board_issue_site_id = Some(site_id);
                self.active_board_sprint_id = effective_sprint;
                if let Some(sp_id) = effective_sprint {
                    if let Some(pos) = self.sprints.iter().position(|s| s.id == sp_id) {
                        ui.board_ui.selected_sprint = pos;
                    }
                }
                ui.board_ui.selected_col = 0;
                ui.board_ui.selected_row = 0;
                self.clamp_selections(ui);
            }
            UiMsg::BoardLoadFailed {
                seq,
                site_id,
                board_id,
                error,
            } => {
                ui.tasks
                    .finish(&format!("Board #{board_id}"), Some(error.clone()));
                if (seq != 0 && seq != self.board_data_req_seq)
                    || !self.site_scope.matches(&site_id)
                {
                    return;
                }
                ui.error(format!("Failed to load board: {error}"));
            }
            UiMsg::IssuesLoaded {
                seq,
                site_id,
                issues,
                label,
            } => {
                ui.tasks.finish(&format!("JQL: {label}"), None);
                if seq != self.issues_req_seq || !self.site_scope.matches(&site_id) {
                    return;
                }
                self.issue_site_ids = vec![site_id.clone(); issues.len()];
                self.issues = issues;
                ui.issue_ui.selected_issue = 0;
                if let Some(first) = self.issues.first().cloned() {
                    self.request_issue_detail_for_site(ui, Some(&site_id), &first.key);
                } else {
                    self.active_issue = None;
                    self.active_issue_site_id = None;
                    self.active_issue_target = None;
                    self.active_issue_detail_updated_at = None;
                    self.in_flight_issue_detail = None;
                    self.sync_open_item_watch(ui);
                }
                self.clamp_selections(ui);
            }
            UiMsg::IssuesLoadFailed { seq, label, error } => {
                ui.tasks.finish(&format!("JQL: {label}"), Some(error.clone()));
                if seq != 0 && seq != self.issues_req_seq {
                    return;
                }
                ui.error(format!("JQL search failed: {error}"));
            }
            UiMsg::SavedFiltersLoaded(filters) => {
                let mut combined = builtin_filters();
                for f in filters {
                    if !combined.iter().any(|b| b.id == f.id) {
                        combined.push(f);
                    }
                }
                self.saved_filters = combined;
                self.clamp_selections(ui);
            }
            UiMsg::IssueDetailLoaded {
                seq,
                site_id,
                key,
                detail,
                not_found,
            } => {
                if let Some(req_seq) = seq {
                    if self
                        .in_flight_issue_detail
                        .as_ref()
                        .is_some_and(|f| f.seq == req_seq)
                    {
                        self.in_flight_issue_detail = None;
                    }
                    if req_seq != self.issue_detail_req_seq {
                        return;
                    }
                } else if self.active_issue.as_ref().is_some_and(|cur| {
                    cur.summary.key != key
                        || self
                            .active_issue_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(&site_id))
                }) {
                    return;
                }
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                if let Some((ref target_sid, ref target_key)) = self.active_issue_target {
                    if !target_sid.eq_ignore_ascii_case(&site_id) || target_key != &key {
                        return;
                    }
                }
                match detail {
                    Ok(d) => {
                        let detail_ts = d.summary.updated_at.clone();
                        self.apply_issue_summary_to_lists(&site_id, &d.summary);
                        for overlay in &mut ui.overlays {
                            if let Overlay::Attachments(ov) = overlay {
                                if ov.owner_label == d.summary.key {
                                    ov.items = d.attachments.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                        self.active_issue = Some(d);
                        self.active_issue_site_id = Some(site_id.clone());
                        self.active_issue_target = Some((site_id, key));
                        self.active_issue_detail_updated_at = Some(detail_ts);
                    }
                    Err(err) => {
                        if not_found {
                            self.purge_deleted_issue(ui, &site_id, &key, false);
                        }
                        ui.error(format!("Failed to load issue detail: {err}"));
                    }
                }
            }
            UiMsg::IssueMutated {
                site_id,
                detail,
                toast,
                clear_draft_key,
            } => {
                if let Some(dk) = clear_draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                let key = detail.summary.key.clone();
                let summary = detail.summary.summary.clone();
                let is_create = toast.starts_with("Created ")
                    || clear_draft_key
                        .as_deref()
                        .is_some_and(|dk| dk.contains("issue:new"));
                ui.tasks.finish(&format!("Assign {key} to me"), None);
                ui.tasks.finish(&format!("Assign {key}"), None);
                ui.tasks.finish(&format!("Update {key}"), None);
                ui.tasks.finish("Create issue", None);
                if self.site_scope.matches(&site_id) {
                    let mut inserted_at_zero = false;
                    if let Some(pos) = self.issues.iter().enumerate().position(|(idx, i)| {
                        i.key == key
                            && self
                                .issue_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        self.issues[pos] = detail.summary.clone();
                    } else if is_create || !self.issues_custom_query {
                        let had_issues = !self.issues.is_empty();
                        self.issues.insert(0, detail.summary.clone());
                        self.issue_site_ids.insert(0, site_id.clone());
                        inserted_at_zero = had_issues;
                    }

                    if self
                        .board_issue_site_id
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(true)
                    {
                        if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == key) {
                            *it = detail.summary.clone();
                        } else if is_create {
                            let board_matches = self.active_board(ui).is_some_and(|b| {
                                b.project_key.as_deref().is_none_or(|pk| {
                                    pk.eq_ignore_ascii_case(&detail.summary.project_key)
                                })
                            });
                            let sprint_matches = match self.active_board_sprint_id {
                                None => true,
                                Some(sid) => detail.summary.sprint_id == Some(sid),
                            };
                            if board_matches && sprint_matches {
                                self.board_issues.insert(0, detail.summary.clone());
                            }
                        }
                    }

                    let (_, site_client) = self.site_id_and_client_for(Some(&site_id));
                    let known_me = self.known_me_for_site(&site_id, &site_client);
                    let is_open = !matches!(detail.summary.status.category, StatusCategory::Done);
                    let assigned_to_me = known_me.as_ref().map(|me| {
                        detail
                            .summary
                            .assignee
                            .as_ref()
                            .is_some_and(|u| u.id.eq_ignore_ascii_case(&me.id))
                    });
                    if let Some(pos) = self.my_issues.iter().enumerate().position(|(idx, it)| {
                        it.key == key
                            && self
                                .my_issue_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        if !is_open || assigned_to_me == Some(false) {
                            self.remove_my_issue(&site_id, &key);
                        } else {
                            self.my_issues[pos] = detail.summary.clone();
                        }
                    } else if is_open && assigned_to_me == Some(true) {
                        let show_pill = self.is_multi_site_all_mode();
                        let pill = show_pill.then(|| self.site_display_label(&site_id));
                        self.my_issues.insert(0, detail.summary.clone());
                        self.my_issue_site_ids.insert(0, site_id.clone());
                        self.my_issue_site_labels.insert(0, pill);
                    }

                    let should_update_active = is_create
                        || self.active_issue.is_none()
                        || self.active_issue.as_ref().is_some_and(|cur| {
                            cur.summary.key == key
                                && self
                                    .active_issue_site_id
                                    .as_deref()
                                    .map(|s| s.eq_ignore_ascii_case(&site_id))
                                    .unwrap_or(true)
                        });
                    if should_update_active {
                        let detail_ts = detail.summary.updated_at.clone();
                        self.active_issue = Some(detail);
                        self.active_issue_site_id = Some(site_id.clone());
                        self.active_issue_target = Some((site_id, key.clone()));
                        self.active_issue_detail_updated_at = Some(detail_ts);
                        if inserted_at_zero && is_create && ui.active_tab == WorkspaceTab::Issues {
                            ui.issue_ui.selected_issue = 0;
                        } else if inserted_at_zero {
                            ui.issue_ui.selected_issue =
                                ui.issue_ui.selected_issue.saturating_add(1);
                        }
                    } else if inserted_at_zero {
                        ui.issue_ui.selected_issue = ui.issue_ui.selected_issue.saturating_add(1);
                    }
                }
                self.refresh_scoped_cache_counts();
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &summary);
            }
            UiMsg::IssueTransitioned {
                site_id,
                key,
                to_status,
                toast,
            } => {
                ui.tasks.finish(&format!("Transition {key}"), None);
                self.apply_local_status_update(Some(&site_id), &key, &to_status);
                if matches!(to_status.category, StatusCategory::Done) {
                    self.remove_my_issue(&site_id, &key);
                }
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &format!("Transitioned to {}", to_status.name));
            }
            UiMsg::IssueTransitionFailed {
                site_id,
                key,
                previous_status,
                error,
            } => {
                ui.tasks
                    .finish(&format!("Transition {key}"), Some(error.clone()));
                if let Some(prev) = previous_status {
                    self.apply_local_status_update(Some(&site_id), &key, &prev);
                }
                self.clamp_selections(ui);
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueAssignFailed {
                site_id,
                key,
                previous_assignee,
                task_label,
                error,
            } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                if let Some(prev) = previous_assignee {
                    self.apply_local_assignee_update(Some(&site_id), &key, prev);
                } else {
                    self.reload_my_issues_from_storage();
                }
                self.clamp_selections(ui);
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueActionFailed { task_label, error } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::TransitionsLoaded {
                seq,
                site_id,
                issue_key,
                transitions,
            } => {
                if seq != self.transitions_req_seq.load(Ordering::Acquire) {
                    return;
                }
                if let Some(Overlay::Transition(ov)) = ui.overlays.last_mut() {
                    if ov.issue_key == issue_key
                        && site_matches(ov.site_id.as_deref(), Some(&site_id))
                    {
                        match transitions {
                            Ok(list) => ov.set_transitions(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::AssignableUsersLoaded {
                seq,
                site_id,
                query,
                users,
            } => {
                if seq != self.assignable_users_req_seq.load(Ordering::Acquire) {
                    return;
                }
                if let Some(Overlay::Assignee(ov)) = ui.overlays.last_mut() {
                    if ov.query.text == query
                        && site_matches(ov.site_id.as_deref(), Some(&site_id))
                    {
                        match users {
                            Ok(list) => ov.set_users(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::CommentAdded {
                site_id,
                issue_key,
                page_id,
                optimistic_id,
                task_label,
                clear_draft_key,
                result,
            } => match result {
                Ok(()) => {
                    if let Some(dk) = clear_draft_key.as_deref() {
                        self.clear_draft_and_save(dk);
                    }
                    ui.tasks.finish(&task_label, None);
                    ui.success("Comment posted");
                    if let Some(k) = issue_key {
                        self.notify_desktop_issue(&k, "Comment posted");
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == k
                        }) {
                            self.request_issue_detail_for_site(ui, Some(&site_id), &k);
                        }
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
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            self.request_page_detail_for_site(ui, Some(&site_id), &pid);
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to post comment (draft preserved): {e}");
                    ui.tasks.finish(&task_label, Some(msg.clone()));
                    if let Some(ref opt_id) = optimistic_id {
                        if let Some(ref k) = issue_key {
                            if let Some(detail) = &mut self.active_issue {
                                if detail.summary.key == *k {
                                    detail.comments.retain(|c| c.id != *opt_id);
                                }
                            }
                            self.bump_local_comment_count(Some(&site_id), k, -1);
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
            UiMsg::SpacesLoaded(Ok(spaces)) => {
                self.spaces = spaces;
                self.clamp_selections(ui);
            }
            UiMsg::SpacesLoaded(Err(e)) => ui.error(e),
            UiMsg::RootPagesLoaded {
                site_id,
                space_key,
                pages,
            } => {
                ui.tasks
                    .finish(&format!("Space {space_key}"), pages.as_ref().err().cloned());
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                match pages {
                    Ok(list) => {
                        let matches_space = self
                            .active_space(ui)
                            .filter(|sp| sp.key == space_key)
                            .and_then(|_| self.active_space_site_id(ui))
                            .map(|s| s.eq_ignore_ascii_case(&site_id))
                            .unwrap_or(true);
                        if matches_space {
                            self.root_pages_by_space
                                .insert(SiteKey::new(&site_id, &space_key), list);
                            self.clamp_selections(ui);
                        }
                    }
                    Err(e) => ui.error(format!("Failed to load root pages: {e}")),
                }
            }
            UiMsg::ChildPagesLoaded {
                site_id,
                parent_id,
                task_label,
                children,
            } => {
                // Finish before the scope check: a site switch while the
                // request was in flight must not leave the spinner running.
                ui.tasks.finish(&task_label, children.as_ref().err().cloned());
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                match children {
                    Ok(list) => {
                        self.page_children_map
                            .insert(SiteKey::new(&site_id, parent_id), list);
                        self.clamp_selections(ui);
                    }
                    Err(e) => ui.error(format!("Failed to load child pages: {e}")),
                }
            }
            UiMsg::PageDetailLoaded {
                seq,
                site_id,
                page_id,
                detail,
                not_found,
            } => {
                if let Some(req_seq) = seq {
                    if self
                        .in_flight_page_detail
                        .as_ref()
                        .is_some_and(|f| f.seq == req_seq)
                    {
                        self.in_flight_page_detail = None;
                    }
                    if req_seq != self.page_detail_req_seq {
                        return;
                    }
                } else if self.active_page.as_ref().is_some_and(|cur| {
                    cur.summary.id != page_id
                        || self
                            .active_page_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(&site_id))
                }) {
                    return;
                }
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                if let Some((ref target_sid, ref target_pid)) = self.active_page_target {
                    if !target_sid.eq_ignore_ascii_case(&site_id) || target_pid != &page_id {
                        return;
                    }
                }
                match detail {
                    Ok(d) => {
                        let detail_ver = d.summary.version_number;
                        // Keep tree rows updated for title/version changes when matching space/parent.
                        if let Some(roots) = self
                            .root_pages_by_space
                            .get_mut(&SiteKey::new(&site_id, &d.summary.space_key))
                        {
                            if let Some(existing) = roots.iter_mut().find(|p| p.id == d.summary.id) {
                                let had_children = existing.has_children == Some(true);
                                *existing = d.summary.clone();
                                if had_children {
                                    existing.has_children = Some(true);
                                }
                            }
                        }
                        if let Some(ref parent_id) = d.summary.parent_id {
                            if let Some(kids) =
                                self.page_children_map.get_mut(&SiteKey::new(&site_id, parent_id))
                            {
                                if let Some(existing) = kids.iter_mut().find(|p| p.id == d.summary.id) {
                                    let had_children = existing.has_children == Some(true);
                                    *existing = d.summary.clone();
                                    if had_children {
                                        existing.has_children = Some(true);
                                    }
                                }
                            }
                        }
                        for overlay in &mut ui.overlays {
                            if let Overlay::Attachments(ov) = overlay {
                                if ov.owner_label == d.summary.title
                                    || self
                                        .active_page
                                        .as_ref()
                                        .is_some_and(|prev| ov.owner_label == prev.summary.title)
                                {
                                    ov.owner_label = d.summary.title.clone();
                                    ov.items = d.attachments.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                        self.active_page = Some(d);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id, page_id));
                        self.active_page_detail_version = Some(detail_ver);
                    }
                    Err(e) => {
                        if not_found {
                            self.purge_deleted_page(ui, &site_id, &page_id, false);
                        }
                        ui.error(format!("Failed to load page: {e}"));
                    }
                }
            }
            UiMsg::PageMutated {
                site_id,
                detail,
                toast,
                clear_draft_key,
            } => {
                if let Some(dk) = clear_draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                ui.tasks.finish("Update page", None);
                ui.tasks.finish("Create page", None);
                let sk = detail.summary.space_key.clone();
                let title = detail.summary.title.clone();
                let pid_owned = detail.summary.id.clone();
                let mut parent_to_fetch: Option<(String, String)> = None;
                if self.site_scope.matches(&site_id) {
                    if let Some(pid) = &detail.summary.parent_id {
                        let parent = SiteKey::new(&site_id, pid);
                        if let Some(kids) = self.page_children_map.get_mut(&parent) {
                            if let Some(existing) = kids.iter_mut().find(|p| p.id == detail.summary.id) {
                                let prev_has_children = existing.has_children;
                                *existing = detail.summary.clone();
                                if existing.has_children.is_none() {
                                    existing.has_children = prev_has_children;
                                }
                            } else {
                                kids.push(detail.summary.clone());
                            }
                            self.expanded_pages.insert(parent);
                        } else {
                            // Do NOT insert a 1-element Vec when parent wasn't loaded yet, or
                            // pre-existing siblings would be hidden; mark parent & fetch live children.
                            let mut parent_title = String::new();
                            for (k, roots) in self.root_pages_by_space.iter_mut() {
                                if k.is_site(&site_id) {
                                    if let Some(p) = roots.iter_mut().find(|p| p.id == *pid) {
                                        p.has_children = Some(true);
                                        parent_title = p.title.clone();
                                    }
                                }
                            }
                            for (k, kids) in self.page_children_map.iter_mut() {
                                if k.is_site(&site_id) {
                                    if let Some(p) = kids.iter_mut().find(|p| p.id == *pid) {
                                        p.has_children = Some(true);
                                        if parent_title.is_empty() {
                                            parent_title = p.title.clone();
                                        }
                                    }
                                }
                            }
                            self.expanded_pages.insert(parent);
                            parent_to_fetch = Some((pid.clone(), parent_title));
                        }
                    } else {
                        let roots = self
                            .root_pages_by_space
                            .entry(SiteKey::new(&site_id, &sk))
                            .or_default();
                        if let Some(existing) = roots.iter_mut().find(|p| p.id == detail.summary.id) {
                            let prev_has_children = existing.has_children;
                            *existing = detail.summary.clone();
                            if existing.has_children.is_none() {
                                existing.has_children = prev_has_children;
                            }
                        } else {
                            roots.push(detail.summary.clone());
                        }
                    }

                    // Keep My Work recent pages updated.
                    if let Some(pos) = self.my_pages.iter().enumerate().position(|(idx, p)| {
                        p.id == pid_owned
                            && self
                                .my_page_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        self.my_pages[pos] = detail.summary.clone();
                    } else {
                        let show_pill = self.is_multi_site_all_mode();
                        let pill = show_pill.then(|| self.site_display_label(&site_id));
                        self.my_pages.insert(0, detail.summary.clone());
                        self.my_page_site_ids.insert(0, site_id.clone());
                        self.my_page_site_labels.insert(0, pill);
                        if self.my_pages.len() > 30 {
                            self.my_pages.truncate(30);
                            self.my_page_site_ids.truncate(30);
                            self.my_page_site_labels.truncate(30);
                        }
                    }

                    let active_matches_site = self
                        .active_page_site_id
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(true);
                    if active_matches_site {
                        let detail_ver = detail.summary.version_number;
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id.clone(), pid_owned));
                        self.active_page_detail_version = Some(detail_ver);
                    }
                }
                if let Some((parent_id, parent_title)) = parent_to_fetch {
                    self.request_page_children_for_site(ui, &site_id, &parent_id, &parent_title, true);
                }
                self.refresh_scoped_cache_counts();
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_page(&sk, &title);
            }
            UiMsg::PageActionFailed { task_label, error } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                ui.error(&error);
                self.notify_desktop("Atlassian Confluence Error", &error);
            }
            UiMsg::MyWorkLoaded {
                seq,
                issues,
                issue_site_ids,
                issue_site_labels,
                pages,
                page_site_ids,
                page_site_labels,
            } => {
                ui.tasks.finish("Loading My Work", None);
                if seq != self.my_work_req_seq {
                    return;
                }
                self.my_issues = issues;
                self.my_issue_site_ids = issue_site_ids;
                self.my_issue_site_labels = issue_site_labels;
                self.my_pages = pages;
                self.my_page_site_ids = page_site_ids;
                self.my_page_site_labels = page_site_labels;
                self.last_my_work_refresh_at = Some(Instant::now());
                self.clamp_selections(ui);
            }
            UiMsg::AttachmentsRefreshed {
                site_id,
                issue_key,
                page_id,
                attachments,
            } => {
                if let Ok(atts) = attachments {
                    if let Some(key) = issue_key {
                        let new_count = atts.len();
                        self.for_each_local_issue_mut(Some(&site_id), &key, |it| {
                            it.attachment_count = new_count;
                        });
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == key
                        }) {
                            if let Some(ref mut detail) = self.active_issue {
                                detail.attachments = atts.clone();
                                detail.summary.attachment_count = new_count;
                            }
                            for overlay in &mut ui.overlays {
                                if let Overlay::Attachments(ov) = overlay {
                                    ov.items = atts.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                    } else if let Some(pid) = page_id {
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            if let Some(ref mut page) = self.active_page {
                                page.attachments = atts.clone();
                            }
                            for overlay in &mut ui.overlays {
                                if let Overlay::Attachments(ov) = overlay {
                                    ov.items = atts.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            UiMsg::CommentsRefreshed {
                site_id,
                issue_key,
                page_id,
                issue_comments,
                issue_comment_total,
                page_comments,
            } => {
                if let (Some(key), Some(Ok(comments))) = (issue_key, issue_comments) {
                    let count = issue_comment_total.unwrap_or(0).max(comments.len());
                    self.for_each_local_issue_mut(Some(&site_id), &key, |it| {
                        it.comment_count = count;
                    });
                    if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                        sid.eq_ignore_ascii_case(&site_id) && cur_k == key
                    }) {
                        if let Some(ref mut detail) = self.active_issue {
                            detail.comments = comments;
                            detail.summary.comment_count = count;
                        }
                    }
                } else if let (Some(pid), Some(Ok(comments))) = (page_id, page_comments) {
                    if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                        sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                    }) {
                        if let Some(ref mut page) = self.active_page {
                            page.comments = comments;
                        }
                    }
                }
            }
            UiMsg::RemoteSearchLoaded { seq, query, hits } => {
                ui.tasks.finish(
                    &format!("Remote search: {query}"),
                    hits.as_ref().err().cloned(),
                );
                if seq != self.remote_search_req_seq {
                    return;
                }
                if ui.search_ui.query.text.trim() != query.trim() {
                    return;
                }
                match hits {
                    Ok((list, site_ids, labels)) => {
                        self.search_hits = list;
                        self.search_hit_site_ids = site_ids;
                        self.search_hit_site_labels = labels;
                        ui.search_ui.is_remote = true;
                        ui.search_ui.selected_hit = 0;
                        self.clamp_selections(ui);
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
                        self.reload_cached_preserving_ui(ui);
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
                self.reload_cached_preserving_ui(ui);
                self.maybe_refresh_open_items_after_sync(ui);
                ui.success(outcome.summary);
            }
            UiMsg::ImageLoaded {
                cache_key,
                rendered,
            } => match rendered {
                Ok(img) => {
                    let visible_key = match ui.overlays.last() {
                        Some(Overlay::ImagePreview(ov)) if !ov.protocol.is_pixel() => {
                            let area = ratatui::layout::Rect::new(0, 0, ui.term_width, ui.term_height);
                            ov.lookup_cached(area, self).map(|(key, _)| key)
                        }
                        _ => None,
                    };
                    let evicted = self.image_cache.insert(cache_key, img);
                    // A fetch started before navigation can complete after the visible image.
                    if visible_key.is_some_and(|key| evicted.contains(&key)) {
                        self.request_load_preview_image(ui);
                    }
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
                task_label,
                result,
            } => {
                ui.tasks.finish(&task_label, result.as_ref().err().cloned());
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
            UiMsg::AttachmentDownloaded { task_label, result } => match result {
                Ok(file) => {
                    ui.tasks.finish(&task_label, None);
                    ui.success(format!("Saved: {}", file.path.display()));
                    self.notify_desktop("Atlassian Download", &format!("Saved {}", file.filename));
                }
                Err(e) => {
                    ui.tasks.finish(&task_label, Some(e.clone()));
                    ui.error(format!("Download failed: {e}"));
                    self.notify_desktop("Atlassian Download Failed", &e);
                }
            },
            UiMsg::AttachmentUploaded {
                site_id,
                issue_key,
                page_id,
                result,
            } => match result {
                Ok(uploaded) => {
                    ui.tasks.finish("Upload attachment", None);
                    ui.success("Attachment uploaded");
                    self.notify_desktop("Atlassian Upload", "Attachment uploaded");
                    if let Some(k) = issue_key {
                        let added_len = uploaded.len();
                        let mut reconciled_total: Option<usize> = None;
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == k
                        }) {
                            if let Some(ref mut detail) = self.active_issue {
                                for att in &uploaded {
                                    let exists = !att.id.is_empty()
                                        && detail.attachments.iter().any(|e| e.id == att.id);
                                    if !exists {
                                        detail.attachments.push(att.clone());
                                    }
                                }
                                detail.summary.attachment_count = detail.attachments.len();
                                reconciled_total = Some(detail.attachments.len());
                                for overlay in &mut ui.overlays {
                                    if let Overlay::Attachments(ov) = overlay {
                                        if ov.owner_label == detail.summary.key {
                                            ov.items = detail.attachments.clone();
                                        }
                                    }
                                }
                            }
                            self.request_issue_detail_for_site(ui, Some(&site_id), &k);
                        }
                        self.for_each_local_issue_mut(Some(&site_id), &k, |it| {
                            if let Some(tot) = reconciled_total {
                                it.attachment_count = tot;
                            } else {
                                it.attachment_count = it.attachment_count.saturating_add(added_len);
                            }
                        });
                    }
                    if let Some(pid) = page_id {
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            if let Some(ref mut page) = self.active_page {
                                for att in &uploaded {
                                    let exists = !att.id.is_empty()
                                        && page.attachments.iter().any(|e| e.id == att.id);
                                    if !exists {
                                        page.attachments.push(att.clone());
                                    }
                                }
                                for overlay in &mut ui.overlays {
                                    if let Overlay::Attachments(ov) = overlay {
                                        if ov.owner_label == page.summary.title {
                                            ov.items = page.attachments.clone();
                                        }
                                    }
                                }
                            }
                            self.request_page_detail_for_site(ui, Some(&site_id), &pid);
                        }
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
/// A cache key scoped to one site: Confluence space keys and page ids are
/// only unique within a site (ATL-11). The site id is compared
/// case-insensitively by normalising it at construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteKey {
    pub site_id: String,
    pub id: String,
}

impl SiteKey {
    pub fn new(site_id: &str, id: impl Into<String>) -> Self {
        Self {
            site_id: site_id.trim().to_ascii_lowercase(),
            id: id.into(),
        }
    }

    pub fn is_site(&self, site_id: &str) -> bool {
        self.site_id.eq_ignore_ascii_case(site_id.trim())
    }
}

pub fn flatten_page_tree(
    roots: &[PageSummary],
    site_id: &str,
    children_map: &HashMap<SiteKey, Vec<PageSummary>>,
    expanded: &HashSet<SiteKey>,
) -> Vec<TreeRow> {
    fn walk(
        nodes: &[PageSummary],
        depth: usize,
        site_id: &str,
        children_map: &HashMap<SiteKey, Vec<PageSummary>>,
        expanded: &HashSet<SiteKey>,
        visited: &mut HashSet<String>,
        out: &mut Vec<TreeRow>,
    ) {
        for page in nodes {
            if !visited.insert(page.id.clone()) {
                continue;
            }
            let node = SiteKey::new(site_id, &page.id);
            let children = children_map.get(&node);
            let has_children =
                page.has_children == Some(true) || children.is_some_and(|c| !c.is_empty());
            let is_expanded = expanded.contains(&node);
            out.push(TreeRow {
                page: page.clone(),
                depth,
                expanded: is_expanded,
                has_children,
            });
            if is_expanded {
                if let Some(kids) = children {
                    walk(kids, depth + 1, site_id, children_map, expanded, visited, out);
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk(roots, 0, site_id, children_map, expanded, &mut visited, &mut out);
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

/// `true` when a row loaded from `row_site` belongs to `target`; either side
/// unknown matches (legacy single-site rows carry no site).
fn site_matches(row_site: Option<&str>, target: Option<&str>) -> bool {
    match (row_site, target) {
        (Some(row), Some(target)) => row.eq_ignore_ascii_case(target),
        _ => true,
    }
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

    fn two_site_app() -> (AppState, UiState) {
        let cfg = AppConfig {
            sites: ["site-a", "site-b"]
                .iter()
                .map(|id| SiteProfile {
                    id: (*id).into(),
                    name: id.to_uppercase(),
                    site_url: format!("https://{id}.example.com"),
                    deployment: DeploymentKind::Cloud,
                    account_id: "default".into(),
                    enabled: true,
                    ..Default::default()
                })
                .collect(),
            default_site: Some("site-a".into()),
            active_site: Some("site-a".into()),
            ..Default::default()
        };
        let auth = AuthManager::new("w8-tasks");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    fn page(id: &str, space: &str, title: &str) -> PageSummary {
        PageSummary {
            id: id.into(),
            space_key: space.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn space(key: &str) -> SpaceSummary {
        SpaceSummary {
            id: format!("sp-{key}"),
            key: key.into(),
            name: key.into(),
            space_type: "global".into(),
            ..Default::default()
        }
    }

    /// Two rows with the same key from two sites; the opened issue is site A's.
    fn shared_key_rows(app: &mut AppState, ui: &mut UiState) {
        app.site_scope = SiteScope::All;
        let row = || sample_issue("SHARED-1", "To Do", StatusCategory::Todo);
        app.issues = vec![row(), row()];
        app.issue_site_ids = vec!["site-a".into(), "site-b".into()];
        app.active_issue = Some(IssueDetail {
            summary: row(),
            ..Default::default()
        });
        app.active_issue_site_id = Some("site-a".into());
        ui.active_tab = WorkspaceTab::Issues;
        ui.issue_ui.selected_issue = 1;
    }

    #[tokio::test]
    async fn write_targets_site_of_selected_row_not_opened_issue() {
        let (mut app, mut ui) = two_site_app();
        shared_key_rows(&mut app, &mut ui);
        assert_eq!(
            app.selected_issue_ref(&ui),
            Some(("site-b".to_string(), "SHARED-1".to_string()))
        );

        crate::ui::action::dispatch(crate::ui::keymap::Action::OpenTransition, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::Transition(ov)) => assert_eq!(ov.site_id.as_deref(), Some("site-b")),
            other => panic!("expected transition overlay, got {other:?}"),
        }
        ui.pop_overlay();

        app.request_transition_issue_for_site(
            &mut ui,
            Some("site-b"),
            "SHARED-1",
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
        assert_eq!(app.issues[1].status.name, "Done", "site B row moved");
        assert_eq!(app.issues[0].status.name, "To Do", "site A row untouched");
        assert_eq!(
            app.active_issue.as_ref().unwrap().summary.status.name,
            "To Do",
            "opened site A issue untouched"
        );
    }

    #[tokio::test]
    async fn optimistic_assignee_update_is_site_scoped() {
        let (mut app, mut ui) = two_site_app();
        shared_key_rows(&mut app, &mut ui);
        let bob = UserSummary {
            id: "bob-b".into(),
            display_name: "Bob".into(),
            ..Default::default()
        };
        app.request_assign_user(&mut ui, Some("site-b"), "SHARED-1", Some(bob));
        assert_eq!(app.issues[1].assignee.as_ref().unwrap().id, "bob-b");
        assert!(app.issues[0].assignee.is_none());
        assert!(app.active_issue.as_ref().unwrap().summary.assignee.is_none());

        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueAssignFailed {
                site_id: "site-b".into(),
                key: "SHARED-1".into(),
                previous_assignee: Some(None),
                task_label: "Assign SHARED-1".into(),
                error: "HTTP 403".into(),
            },
        );
        assert!(app.issues[1].assignee.is_none(), "rollback on site B");
    }

    #[tokio::test]
    async fn tree_cache_does_not_collide_across_sites_with_same_space_key() {
        let (mut app, mut ui) = two_site_app();
        app.site_scope = SiteScope::All;
        app.spaces = vec![space("ENG"), space("ENG")];
        app.space_site_ids = vec!["site-a".into(), "site-b".into()];
        app.root_pages_by_space
            .insert(SiteKey::new("site-a", "ENG"), vec![page("1", "ENG", "A root")]);
        app.root_pages_by_space
            .insert(SiteKey::new("SITE-B", "ENG"), vec![page("1", "ENG", "B root")]);

        ui.confluence_ui.selected_space = 0;
        assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "A root");
        ui.confluence_ui.selected_space = 1;
        assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "B root");

        // Site B's children of page "1" never show under site A's page "1".
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ChildPagesLoaded {
                site_id: "site-b".into(),
                parent_id: "1".into(),
                task_label: "Child pages: B root".into(),
                children: Ok(vec![page("2", "ENG", "B child")]),
            },
        );
        app.expanded_pages.insert(SiteKey::new("site-a", "1"));
        ui.confluence_ui.selected_space = 0;
        assert_eq!(app.current_confluence_tree(&ui).len(), 1);
        app.expanded_pages.insert(SiteKey::new("site-b", "1"));
        ui.confluence_ui.selected_space = 1;
        let tree_b = app.current_confluence_tree(&ui);
        assert_eq!(tree_b.len(), 2);
        assert_eq!(tree_b[1].page.title, "B child");
    }

    #[tokio::test]
    async fn new_page_parent_only_when_active_page_in_selected_space_and_site() {
        let (mut app, mut ui) = two_site_app();
        app.site_scope = SiteScope::All;
        ui.active_tab = WorkspaceTab::Confluence;
        app.spaces = vec![space("ENG"), space("ENG"), space("OPS")];
        app.space_site_ids = vec!["site-a".into(), "site-b".into(), "site-a".into()];
        app.active_page = Some(PageDetail {
            summary: page("500", "ENG", "Opened"),
            ..Default::default()
        });
        app.active_page_site_id = Some("site-a".into());

        let open_form = |app: &mut AppState, ui: &mut UiState, space_idx: usize| {
            ui.confluence_ui.selected_space = space_idx;
            crate::ui::action::dispatch(crate::ui::keymap::Action::NewItem, app, ui);
            let form = match ui.overlays.last() {
                Some(Overlay::PageForm(ov)) => (ov.parent_id.clone(), ov.site_id.clone()),
                other => panic!("expected page form, got {other:?}"),
            };
            ui.pop_overlay();
            form
        };

        assert_eq!(
            open_form(&mut app, &mut ui, 0),
            (Some("500".into()), Some("site-a".into())),
            "same space and site: child of the opened page"
        );
        assert_eq!(
            open_form(&mut app, &mut ui, 1),
            (None, Some("site-b".into())),
            "same space key on another site: space root"
        );
        assert_eq!(
            open_form(&mut app, &mut ui, 2),
            (None, Some("site-a".into())),
            "other space on the same site: space root"
        );
    }

    #[tokio::test]
    async fn single_site_my_work_seed_is_site_scoped_and_mine() {
        let (mut app, _ui) = two_site_app();
        let mut mine = sample_issue("ATL-1", "To Do", StatusCategory::Todo);
        mine.assignee = Some(UserSummary {
            id: "me-a".into(),
            ..Default::default()
        });
        let mut theirs = sample_issue("ATL-2", "To Do", StatusCategory::Todo);
        theirs.assignee = Some(UserSummary {
            id: "someone".into(),
            ..Default::default()
        });
        let mut done = sample_issue("ATL-3", "Done", StatusCategory::Done);
        done.assignee = mine.assignee.clone();
        app.storage
            .upsert_issues_for_site("site-a", &[mine, theirs, done])
            .unwrap();
        app.site_scope = SiteScope::Only("site-a".into());
        app.me = Some(UserSummary {
            id: "me-a".into(),
            ..Default::default()
        });
        app.load_cached_initial();
        let keys: Vec<&str> = app.my_issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["ATL-1"]);
        assert_eq!(app.my_issue_site_ids, vec!["site-a".to_string()]);
    }

    #[tokio::test]
    async fn child_pages_loaded_finishes_task_even_when_scope_changed() {
        let (mut app, mut ui) = two_site_app();
        app.site_scope = SiteScope::Only("site-a".into());
        ui.tasks.begin("Child pages: Runbook");
        // The user switched to site-a while a site-b request was in flight.
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ChildPagesLoaded {
                site_id: "site-b".into(),
                parent_id: "42".into(),
                task_label: "Child pages: Runbook".into(),
                children: Ok(vec![]),
            },
        );
        assert!(
            ui.tasks.first_running().is_none(),
            "out-of-scope child-pages result must still finish its task"
        );
        assert!(!app.page_children_map.contains_key(&SiteKey::new("site-b", "42")));
    }

    #[tokio::test]
    async fn failed_requests_finish_their_tasks() {
        let (mut app, mut ui) = two_site_app();
        let cases: Vec<(&str, UiMsg)> = vec![
            (
                "Board #7",
                UiMsg::BoardLoadFailed {
                    seq: 0,
                    site_id: "default".into(),
                    board_id: 7,
                    error: "HTTP 500".into(),
                },
            ),
            (
                "JQL: Mine",
                UiMsg::IssuesLoadFailed {
                    seq: 0,
                    label: "Mine".into(),
                    error: "HTTP 400".into(),
                },
            ),
            (
                "Update ATL-1",
                UiMsg::IssueActionFailed {
                    task_label: "Update ATL-1".into(),
                    error: "HTTP 500".into(),
                },
            ),
            (
                "Create page",
                UiMsg::PageActionFailed {
                    task_label: "Create page".into(),
                    error: "HTTP 409".into(),
                },
            ),
            (
                "Download: a.pdf",
                UiMsg::AttachmentDownloaded {
                    task_label: "Download: a.pdf".into(),
                    result: Err("HTTP 404".into()),
                },
            ),
            (
                "HTML: report.html",
                UiMsg::HtmlAttachmentLoaded {
                    title: "Page — report.html".into(),
                    source_url: String::new(),
                    task_label: "HTML: report.html".into(),
                    result: Err("HTTP 403".into()),
                },
            ),
        ];
        for (label, msg) in cases {
            ui.tasks.begin(label);
            app.handle_ui_msg(&mut ui, msg);
            assert!(
                ui.tasks.running().all(|t| t.label != label),
                "{label} still running after its failure message"
            );
            assert!(
                ui.tasks.last().unwrap().display().contains("failed"),
                "{label} should be shown as failed"
            );
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
            has_children: Some(true),
            ..Default::default()
        };
        let child = PageSummary {
            id: "101".into(),
            parent_id: Some("100".into()),
            title: "TUI Design".into(),
            has_children: Some(false),
            ..Default::default()
        };
        let mut children_map = HashMap::new();
        children_map.insert(SiteKey::new("default", "100"), vec![child]);

        let mut expanded = HashSet::new();
        let collapsed = flatten_page_tree(&[root.clone()], "default", &children_map, &expanded);
        assert_eq!(collapsed.len(), 1);
        assert!(!collapsed[0].expanded);
        assert!(collapsed[0].has_children);

        expanded.insert(SiteKey::new("DEFAULT", "100"));
        let open = flatten_page_tree(&[root.clone()], "default", &children_map, &expanded);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].depth, 0);
        assert!(open[0].expanded);
        assert_eq!(open[1].depth, 1);
        assert_eq!(open[1].page.title, "TUI Design");

        // The same page id on another site is a different node.
        let other = flatten_page_tree(&[root], "site-b", &children_map, &expanded);
        assert_eq!(other.len(), 1);
        assert!(!other[0].expanded);
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
            app.nav_history[0].target,
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
                site_id: "default".into(),
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
                site_id: "default".into(),
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
                site_id: "default".into(),
                issue_key: Some("ATL-99".into()),
                page_id: None,
                optimistic_id: Some(opt_id),
                task_label: "Comment on ATL-99".into(),
                clear_draft_key: None,
                result: Err("network error".into()),
            },
        );
        assert!(app.active_issue.as_ref().unwrap().comments.is_empty());
        assert_eq!(app.active_issue.as_ref().unwrap().summary.comment_count, 0);
    }

    #[tokio::test]
    async fn tui_async_detail_sequence_and_multisite_sync_preserve_open_issue_and_404_clears() {
        use crate::models::{IssueDetail, IssueSummary};
        use crate::sync::MultiSiteSyncOutcome;

        let cfg = AppConfig::default();
        let auth = AuthManager::new("test-seq");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage.clone(),
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        let mk = |key: &str, summary: &str, updated: &str| IssueDetail {
            summary: IssueSummary {
                id: key.into(),
                key: key.into(),
                project_key: "ATL".into(),
                summary: summary.into(),
                updated_at: updated.into(),
                ..Default::default()
            },
            description_markdown: format!("Body of {key}"),
            ..Default::default()
        };

        let d1 = mk("ATL-1", "First issue", "2026-11-18T10:00:00Z");
        let d2 = mk("ATL-2", "Second issue", "2026-11-17T10:00:00Z");
        storage.upsert_issue_detail_for_site("default", &d1).unwrap();
        storage.upsert_issue_detail_for_site("default", &d2).unwrap();

        app.active_issue = Some(d2.clone());
        app.active_issue_site_id = Some("default".into());

        // MultiSiteSyncFinished must preserve currently open ATL-2 instead of hijacking to ATL-1
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MultiSiteSyncFinished(MultiSiteSyncOutcome {
                summary: "Synced".into(),
                total_sites: 1,
                succeeded_sites: 1,
                per_site: vec![],
            }),
        );
        assert_eq!(app.active_issue.as_ref().unwrap().summary.key, "ATL-2");

        // Stale out-of-order response (seq=1 when current seq=2) must be ignored
        app.issue_detail_req_seq = 2;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueDetailLoaded {
                seq: Some(1),
                site_id: "default".into(),
                key: "ATL-1".into(),
                detail: Ok(d1),
                not_found: false,
            },
        );
        assert_eq!(app.active_issue.as_ref().unwrap().summary.key, "ATL-2");

        // Matching seq with not_found=true clears active_issue and removes zombie from lists
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueDetailLoaded {
                seq: Some(2),
                site_id: "default".into(),
                key: "ATL-2".into(),
                detail: Err("HTTP 404".into()),
                not_found: true,
            },
        );
        assert!(app.active_issue.is_none());
        assert!(!app.issues.iter().any(|i| i.key == "ATL-2"));
    }

    #[tokio::test]
    async fn cached_detail_reconciled_summary_still_fetches_changed_revision_and_deduplicates() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use axum::{routing::get, Json, Router};
        use serde_json::json;
        use tokio::net::TcpListener;

        let issue_detail_hits = Arc::new(AtomicUsize::new(0));
        let page_detail_hits = Arc::new(AtomicUsize::new(0));

        let ih = issue_detail_hits.clone();
        let ph = page_detail_hits.clone();
        let mock = Router::new()
            .route(
                "/rest/api/3/issue/ATL-1",
                get(move || {
                    let ih = ih.clone();
                    async move {
                        ih.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Json(json!({
                            "id": "100",
                            "key": "ATL-1",
                            "fields": {
                                "project": { "key": "ATL" },
                                "summary": "Remote Issue V2",
                                "issuetype": { "name": "Task" },
                                "status": { "id": "3", "name": "Done", "statusCategory": { "key": "done" } },
                                "description": {
                                    "type": "doc",
                                    "version": 1,
                                    "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "REMOTE_ISSUE_BODY_V2" }] }]
                                },
                                "updated": "2026-11-20T12:05:00.000+0000"
                            }
                        }))
                    }
                }),
            )
            .route("/rest/api/3/issue/ATL-1/remotelink", get(|| async { Json(json!([])) }))
            .route(
                "/wiki/api/v2/pages/1001",
                get(move || {
                    let ph = ph.clone();
                    async move {
                        ph.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Json(json!({
                            "id": "1001",
                            "spaceId": "5001",
                            "spaceKey": "ENG",
                            "title": "Remote Page V2",
                            "status": "current",
                            "version": { "number": 2, "createdAt": "2026-11-20T12:05:00.000Z" },
                            "body": {
                                "atlas_doc_format": {
                                    "representation": "atlas_doc_format",
                                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"REMOTE_PAGE_BODY_V2\"}]}]}"
                                }
                            },
                            "_links": { "webui": "/spaces/ENG/pages/1001" }
                        }))
                    }
                }),
            )
            .route("/wiki/api/v2/pages/1001/labels", get(|| async { Json(json!({"results": []})) }))
            .route("/wiki/api/v2/pages/1001/attachments", get(|| async { Json(json!({"results": []})) }))
            .route("/wiki/api/v2/pages/1001/footer-comments", get(|| async { Json(json!({"results": []})) }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, mock).await;
        });

        let mut cfg = AppConfig::default();
        cfg.site_url = base_url.clone();
        cfg.sync_interval_secs = 300;
        let tmp_dir = tempfile::tempdir().unwrap();
        let auth = AuthManager::new("dedup-test")
            .with_fallback_path(tmp_dir.path().join("creds.json"));
        auth.save_credentials(&StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "bob@example.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        })
        .unwrap();
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage.clone(),
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        // Seed cached v1 details, then upsert newer v2 summary rows (so get_*_detail_for_site
        // reconciles summary to v2 while detail_json still has OLD v1 body!).
        let old_issue_detail = IssueDetail {
            summary: IssueSummary {
                id: "100".into(),
                key: "ATL-1".into(),
                project_key: "ATL".into(),
                summary: "Old V1".into(),
                updated_at: "2026-11-20T10:00:00.000+0000".into(),
                ..Default::default()
            },
            description_markdown: "OLD_V1_DESC".into(),
            ..Default::default()
        };
        storage.upsert_issue_detail_for_site("default", &old_issue_detail).unwrap();
        let mut new_issue_summary = old_issue_detail.summary.clone();
        new_issue_summary.summary = "Remote Issue V2".into();
        new_issue_summary.updated_at = "2026-11-20T12:05:00.000+0000".into();
        storage.upsert_issues_for_site("default", std::slice::from_ref(&new_issue_summary)).unwrap();

        let old_page_detail = PageDetail {
            summary: PageSummary {
                id: "1001".into(),
                space_key: "ENG".into(),
                title: "Old Page V1".into(),
                version_number: 1,
                updated_at: "2026-11-20T10:00:00.000Z".into(),
                ..Default::default()
            },
            body_markdown: "OLD_V1_BODY".into(),
            ..Default::default()
        };
        storage.upsert_page_detail_for_site("default", &old_page_detail).unwrap();
        let mut new_page_summary = old_page_detail.summary.clone();
        new_page_summary.title = "Remote Page V2".into();
        new_page_summary.version_number = 2;
        new_page_summary.updated_at = "2026-11-20T12:05:00.000Z".into();
        storage.upsert_pages_for_site("default", std::slice::from_ref(&new_page_summary)).unwrap();

        // Load from cache: reconciled summary now reads v2, yet body is still v1!
        app.active_page = Some(old_page_detail);
        app.active_page_site_id = Some("default".into());
        app.load_cached_initial();
        assert_eq!(app.active_issue.as_ref().unwrap().summary.updated_at, "2026-11-20T12:05:00.000+0000");
        assert_eq!(app.active_issue.as_ref().unwrap().description_markdown, "OLD_V1_DESC");
        assert_eq!(app.active_page.as_ref().unwrap().summary.version_number, 2);
        assert_eq!(app.active_page.as_ref().unwrap().body_markdown, "OLD_V1_BODY");

        // Concurrent OpenIssueRevision + IssuesUpdated + SyncFinished for the same v2 revision
        // must STILL trigger remote fetch (not blocked by reconciled summary!) AND deduplicate to 1 call!
        app.handle_core_event(
            &mut ui,
            CoreEvent::OpenIssueRevision {
                site_id: "default".into(),
                summary: new_issue_summary.clone(),
            },
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::IssuesUpdated {
                site_id: "default".into(),
                issues: vec![new_issue_summary],
            },
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::OpenPageRevision {
                site_id: "default".into(),
                summary: new_page_summary.clone(),
            },
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::SpacePagesUpdated {
                site_id: "default".into(),
                space_key: "ENG".into(),
                pages: vec![new_page_summary],
            },
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::SyncFinished {
                summary: "Synced".into(),
            },
        );

        let mut got_issue = false;
        let mut got_page = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && (!got_issue || !got_page) {
            if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(250), ui_rx.recv()).await {
                if matches!(msg, UiMsg::IssueDetailLoaded { .. }) {
                    got_issue = true;
                }
                if matches!(msg, UiMsg::PageDetailLoaded { .. }) {
                    got_page = true;
                }
                app.handle_ui_msg(&mut ui, msg);
            }
        }
        assert!(got_issue && got_page, "both details must be fetched remotely");
        assert_eq!(issue_detail_hits.load(Ordering::SeqCst), 1, "issue detail must be deduplicated to 1 HTTP call");
        assert_eq!(page_detail_hits.load(Ordering::SeqCst), 1, "page detail must be deduplicated to 1 HTTP call");
        assert!(app.active_issue.as_ref().unwrap().description_markdown.contains("REMOTE_ISSUE_BODY_V2"));
        assert!(app.active_page.as_ref().unwrap().body_markdown.contains("REMOTE_PAGE_BODY_V2"));

        srv.abort();
    }

    #[tokio::test]
    async fn multisite_same_key_id_late_response_isolated_and_open_edit_forms_preserved() {
        let mut cfg = AppConfig::default();
        cfg.sites = vec![
            SiteProfile {
                id: "site-a".into(),
                name: "Site A".into(),
                site_url: "https://site-a.example.com".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: None,
                account_id: "default".into(),
                enabled: true,
                ..Default::default()
            },
            SiteProfile {
                id: "site-b".into(),
                name: "Site B".into(),
                site_url: "https://site-b.example.com".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: None,
                account_id: "default".into(),
                enabled: true,
                ..Default::default()
            },
        ];
        cfg.default_site = Some("site-a".into());
        cfg.active_site = Some("site-a".into());

        let auth = AuthManager::new("ms-iso");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage.clone(),
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        let issue_a = IssueDetail {
            summary: IssueSummary {
                id: "1".into(),
                key: "SHARED-1".into(),
                project_key: "SHARED".into(),
                summary: "Site A Issue".into(),
                updated_at: "2026-11-20T10:00:00.000+0000".into(),
                ..Default::default()
            },
            description_markdown: "SITE_A_ISSUE_BODY".into(),
            ..Default::default()
        };
        let issue_b = IssueDetail {
            summary: IssueSummary {
                id: "1".into(),
                key: "SHARED-1".into(),
                project_key: "SHARED".into(),
                summary: "Site B Issue".into(),
                updated_at: "2026-11-20T10:00:00.000+0000".into(),
                ..Default::default()
            },
            description_markdown: "SITE_B_ISSUE_BODY".into(),
            ..Default::default()
        };
        let page_a = PageDetail {
            summary: PageSummary {
                id: "1001".into(),
                space_key: "ENG".into(),
                title: "Site A Page".into(),
                version_number: 1,
                updated_at: "2026-11-20T10:00:00.000Z".into(),
                ..Default::default()
            },
            body_markdown: "SITE_A_PAGE_BODY".into(),
            ..Default::default()
        };
        let page_b = PageDetail {
            summary: PageSummary {
                id: "1001".into(),
                space_key: "ENG".into(),
                title: "Site B Page".into(),
                version_number: 1,
                updated_at: "2026-11-20T10:00:00.000Z".into(),
                ..Default::default()
            },
            body_markdown: "SITE_B_PAGE_BODY".into(),
            ..Default::default()
        };
        storage.upsert_issue_detail_for_site("site-a", &issue_a).unwrap();
        storage.upsert_issue_detail_for_site("site-b", &issue_b).unwrap();
        storage.upsert_page_detail_for_site("site-a", &page_a).unwrap();
        storage.upsert_page_detail_for_site("site-b", &page_b).unwrap();

        app.load_cached_initial();
        let seq_a_before_switch = app.issue_detail_req_seq;
        assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site A Issue");

        // Switch from site-a to site-b
        app.apply_site_switcher_command(SiteSwitcherCommand::SelectSite("site-b".into()), &mut ui);
        assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site B Issue");
        assert_eq!(app.active_issue_site_id.as_deref(), Some("site-b"));

        // Late OpenIssueRevision and IssueDetailLoaded from site-a must NOT overwrite site-b's active_issue,
        // while site-a's SQLite cache receives the updated summary!
        let mut late_a_rev = issue_a.summary.clone();
        late_a_rev.summary = "Site A Late V2".into();
        late_a_rev.updated_at = "2026-11-20T12:00:00.000+0000".into();
        app.handle_core_event(
            &mut ui,
            CoreEvent::OpenIssueRevision {
                site_id: "site-a".into(),
                summary: late_a_rev,
            },
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueDetailLoaded {
                seq: Some(seq_a_before_switch),
                site_id: "site-a".into(),
                key: "SHARED-1".into(),
                detail: Ok(issue_a),
                not_found: false,
            },
        );
        assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site B Issue");
        assert_eq!(
            storage.get_issue_detail_for_site("site-a", "SHARED-1").unwrap().unwrap().summary.summary,
            "Site A Late V2"
        );
        assert_eq!(
            storage.get_issue_detail_for_site("site-b", "SHARED-1").unwrap().unwrap().summary.summary,
            "Site B Issue"
        );

        // Open edit forms preserve unsaved user text when background detail updates arrive
        let mut issue_form = IssueFormOverlay::edit_issue(&issue_b);
        issue_form.description.set_text("UNSAVED_USER_ISSUE_DRAFT");
        ui.push_overlay(Overlay::IssueForm(issue_form));

        let mut updated_b = issue_b.clone();
        updated_b.description_markdown = "REMOTE_UPDATED_SITE_B".into();
        updated_b.summary.updated_at = "2026-11-20T12:10:00.000+0000".into();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueDetailLoaded {
                seq: Some(app.issue_detail_req_seq),
                site_id: "site-b".into(),
                key: "SHARED-1".into(),
                detail: Ok(updated_b),
                not_found: false,
            },
        );
        assert_eq!(app.active_issue.as_ref().unwrap().description_markdown, "REMOTE_UPDATED_SITE_B");
        match ui.overlays.last() {
            Some(Overlay::IssueForm(ov)) => {
                assert_eq!(ov.description.text, "UNSAVED_USER_ISSUE_DRAFT");
            }
            other => panic!("expected IssueForm overlay intact, got {other:?}"),
        }

        // Same for PageFormOverlay + CommentOverlay
        ui.pop_overlay();
        app.active_page = Some(page_b.clone());
        app.active_page_site_id = Some("site-b".into());
        app.active_page_target = Some(("site-b".into(), "1001".into()));
        let mut page_form = PageFormOverlay::edit_page(&page_b);
        page_form.body.set_text("UNSAVED_USER_PAGE_DRAFT");
        ui.push_overlay(Overlay::PageForm(page_form));

        let mut updated_page_b = page_b;
        updated_page_b.summary.version_number = 2;
        updated_page_b.body_markdown = "REMOTE_PAGE_B_V2".into();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageDetailLoaded {
                seq: Some(app.page_detail_req_seq),
                site_id: "site-b".into(),
                page_id: "1001".into(),
                detail: Ok(updated_page_b),
                not_found: false,
            },
        );
        assert_eq!(app.active_page.as_ref().unwrap().body_markdown, "REMOTE_PAGE_B_V2");
        match ui.overlays.last() {
            Some(Overlay::PageForm(ov)) => {
                assert_eq!(ov.body.text, "UNSAVED_USER_PAGE_DRAFT");
                assert_eq!(ov.prev_version, 1, "base version remains pinned for 409 conflict safety");
            }
            other => panic!("expected PageForm overlay intact, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn panel_attachment_and_comment_refresh_updates_independently_of_page_version() {
        use axum::{routing::get, Json, Router};
        use serde_json::json;
        use tokio::net::TcpListener;

        let mock = Router::new()
            .route(
                "/wiki/api/v2/pages/1001/attachments",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "att-live-1",
                            "title": "spec-v2.pdf",
                            "mediaType": "application/pdf",
                            "fileSize": 4096,
                            "downloadLink": "/download/spec-v2.pdf"
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/1001/footer-comments",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "cmt-live-1",
                            "version": { "createdAt": "2026-11-20T12:20:00.000Z", "authorId": "Carol" },
                            "body": {
                                "atlas_doc_format": {
                                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"LIVE_COMMENT_WITHOUT_BODY_BUMP\"}]}]}"
                                }
                            }
                        }]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, mock).await;
        });

        let mut cfg = AppConfig::default();
        cfg.site_url = base_url.clone();
        cfg.sync_interval_secs = 300;
        let tmp_dir = tempfile::tempdir().unwrap();
        let auth = AuthManager::new("panel-refresh")
            .with_fallback_path(tmp_dir.path().join("creds.json"));
        auth.save_credentials(&StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "bob@example.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        })
        .unwrap();
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Confluence,
            auth,
            client,
            storage.clone(),
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);

        let page = PageDetail {
            summary: PageSummary {
                id: "1001".into(),
                space_key: "ENG".into(),
                title: "Spec".into(),
                version_number: 1,
                updated_at: "2026-11-20T10:00:00.000Z".into(),
                ..Default::default()
            },
            body_markdown: "BODY_V1".into(),
            ..Default::default()
        };
        storage.upsert_page_detail_for_site("default", &page).unwrap();
        app.active_page = Some(page);
        app.active_page_site_id = Some("default".into());
        app.active_page_target = Some(("default".into(), "1001".into()));
        app.active_page_detail_version = Some(1);

        // 1) Attachments panel open + focus return refreshes attachments even though page version stays 1
        ui.push_overlay(Overlay::Attachments(
            crate::ui::overlays::attachments::AttachmentsOverlay::from_app(&app, &ui),
        ));
        app.request_attachments_refresh(&ui);
        let msg = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
        assert_eq!(app.active_page.as_ref().unwrap().attachments.len(), 1);
        assert_eq!(app.active_page.as_ref().unwrap().attachments[0].filename, "spec-v2.pdf");
        match ui.overlays.last() {
            Some(Overlay::Attachments(ov)) => assert_eq!(ov.items.len(), 1),
            other => panic!("expected AttachmentsOverlay updated, got {other:?}"),
        }

        // 2) Comment panel open with unsaved draft text refreshes comments while preserving draft
        ui.pop_overlay();
        ui.push_overlay(Overlay::Comment(CommentOverlay::with_draft(
            CommentTarget::Page {
                page_id: "1001".into(),
                title: "Spec".into(),
            },
            "UNSAVED_COMMENT_DRAFT",
        )));
        app.request_comments_refresh(&ui);
        let msg2 = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_ui_msg(&mut ui, msg2);
        assert_eq!(app.active_page.as_ref().unwrap().comments.len(), 1);
        assert!(app.active_page.as_ref().unwrap().comments[0]
            .body_markdown
            .contains("LIVE_COMMENT_WITHOUT_BODY_BUMP"));
        match ui.overlays.last() {
            Some(Overlay::Comment(ov)) => assert_eq!(ov.draft_text(), "UNSAVED_COMMENT_DRAFT"),
            other => panic!("expected CommentOverlay intact, got {other:?}"),
        }

        srv.abort();
    }
    fn board(id: i64, project: &str) -> BoardSummary {
        BoardSummary {
            id,
            name: format!("Board {id}"),
            board_type: "scrum".into(),
            project_key: Some(project.into()),
            ..Default::default()
        }
    }

    fn named_board(id: i64, name: &str) -> BoardSummary {
        BoardSummary {
            id,
            name: name.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn my_work_404_removal_keeps_site_vectors_aligned() {
        let (mut app, _ui) = two_site_app();
        let row = |k: &str| sample_issue(k, "To Do", StatusCategory::Todo);
        app.my_issues = vec![row("SHARED-1"), row("SHARED-1"), row("B-2")];
        app.my_issue_site_ids = vec!["site-a".into(), "site-b".into(), "site-b".into()];
        app.my_issue_site_labels = vec![Some("A".into()), Some("B".into()), Some("B".into())];
        app.remove_my_issue("site-b", "SHARED-1");
        let keys: Vec<&str> = app.my_issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["SHARED-1", "B-2"]);
        assert_eq!(app.my_issue_site_ids, vec!["site-a".to_string(), "site-b".to_string()]);
        assert_eq!(app.my_issue_site_labels, vec![Some("A".to_string()), Some("B".to_string())]);

        app.my_pages = vec![page("1", "ENG", "A"), page("1", "ENG", "B"), page("2", "ENG", "B2")];
        app.my_page_site_ids = vec!["site-a".into(), "site-b".into(), "site-b".into()];
        app.my_page_site_labels = vec![Some("A".into()), Some("B".into()), Some("B".into())];
        app.remove_my_page("site-b", "1");
        let titles: Vec<&str> = app.my_pages.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "B2"]);
        assert_eq!(app.my_page_site_ids, vec!["site-a".to_string(), "site-b".to_string()]);
        assert_eq!(app.my_page_site_labels, vec![Some("A".to_string()), Some("B".to_string())]);
    }

    #[tokio::test]
    async fn boards_updated_for_other_site_is_ignored_in_single_site_scope() {
        let (mut app, mut ui) = two_site_app();
        app.site_scope = SiteScope::Only("site-a".into());
        app.boards = vec![named_board(1, "A board")];
        app.board_site_ids = vec!["site-a".into()];

        app.handle_core_event(
            &mut ui,
            CoreEvent::BoardsUpdated {
                site_id: "site-b".into(),
                items: vec![named_board(2, "B board")],
            },
        );
        assert_eq!(app.boards[0].name, "A board", "site B list must not replace site A's");
        app.handle_core_event(
            &mut ui,
            CoreEvent::ProjectsUpdated {
                site_id: "site-b".into(),
                items: vec![ProjectSummary {
                    key: "BPROJ".into(),
                    ..Default::default()
                }],
            },
        );
        assert!(app.projects.is_empty());
        app.handle_core_event(
            &mut ui,
            CoreEvent::SpacesUpdated {
                site_id: "site-b".into(),
                items: vec![space("BSPACE")],
            },
        );
        assert!(app.spaces.is_empty());

        app.handle_core_event(
            &mut ui,
            CoreEvent::BoardsUpdated {
                site_id: "SITE-A".into(),
                items: vec![named_board(3, "A board 2"), named_board(4, "A board 3")],
            },
        );
        assert_eq!(app.boards.len(), 2);
        assert_eq!(app.board_site_ids, vec!["SITE-A".to_string(); 2]);

        // A legacy daemon sends no site: it is the effective site's list.
        app.handle_core_event(
            &mut ui,
            CoreEvent::SpacesUpdated {
                site_id: String::new(),
                items: vec![space("ENG")],
            },
        );
        assert_eq!(app.spaces.len(), 1);
        assert_eq!(app.space_site_ids, vec!["site-a".to_string()]);
    }

    #[tokio::test]
    async fn pin_toggle_writes_to_row_site() {
        let (mut app, mut ui) = two_site_app();
        let proj = ProjectSummary {
            id: "1".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            ..Default::default()
        };
        for sid in ["site-a", "site-b"] {
            app.storage
                .upsert_projects_for_site(sid, std::slice::from_ref(&proj))
                .unwrap();
            app.storage.upsert_spaces_for_site(sid, &[space("DOC")]).unwrap();
        }
        app.site_scope = SiteScope::All;
        app.reload_projects_all_sites();
        app.spaces = vec![space("DOC"), space("DOC")];
        app.space_site_ids = vec!["site-a".into(), "site-b".into()];
        let b_row = app
            .project_site_ids
            .iter()
            .position(|s| s == "site-b")
            .expect("site B project row");

        ui.active_tab = WorkspaceTab::Directory;
        ui.directory_ui.section = 0;
        ui.directory_ui.selected_project = b_row;
        crate::ui::action::dispatch(crate::ui::keymap::Action::TogglePin, &mut app, &mut ui);
        let pinned = |sid: &str| app.storage.list_projects_for_site(sid).unwrap()[0].pinned;
        assert!(pinned("site-b"), "site B row pinned");
        assert!(!pinned("site-a"), "site A project with the same key untouched");

        ui.directory_ui.section = 1;
        ui.directory_ui.selected_space = 1;
        crate::ui::action::dispatch(crate::ui::keymap::Action::TogglePin, &mut app, &mut ui);
        let space_pinned = |sid: &str| app.storage.list_spaces_for_site(sid).unwrap()[0].pinned;
        assert!(space_pinned("site-b"));
        assert!(!space_pinned("site-a"));
    }

    #[tokio::test]
    async fn board_load_uses_site_of_selected_row_when_ids_collide() {
        let (mut app, _ui) = two_site_app();
        app.site_scope = SiteScope::All;
        app.boards = vec![named_board(10, "A board"), named_board(10, "B board")];
        app.board_site_ids = vec!["site-a".into(), "site-b".into()];
        assert_eq!(app.board_row_target(1), Some((10, Some("site-b"))));
        assert_eq!(app.board_row_target(0), Some((10, Some("site-a"))));
        assert_eq!(app.board_row_target(2), None);
        let (sid, _client) = app.site_id_and_client_for(app.board_row_target(1).unwrap().1);
        assert_eq!(sid, "site-b");
    }
    #[tokio::test]
    async fn drafts_are_site_scoped_only_with_multiple_sites() {
        let (mut app, _ui) = two_site_app();
        assert_eq!(
            app.scoped_draft_key(Some("Site-B"), "comment:issue:ENG-1".into()),
            "site:site-b:comment:issue:ENG-1"
        );
        assert_ne!(
            app.scoped_draft_key(Some("site-a"), "comment:issue:ENG-1".into()),
            app.scoped_draft_key(Some("site-b"), "comment:issue:ENG-1".into())
        );
        assert_eq!(app.scoped_draft_key(None, "issue:new".into()), "issue:new");
        // Single-site users keep the keys their saved drafts already use.
        app.config.sites.truncate(1);
        assert_eq!(
            app.scoped_draft_key(Some("site-a"), "comment:issue:ENG-1".into()),
            "comment:issue:ENG-1"
        );
    }

    #[tokio::test]
    async fn back_history_and_cross_links_keep_the_source_site() {
        let (mut app, mut ui) = two_site_app();
        shared_key_rows(&mut app, &mut ui);
        app.active_issue_site_id = Some("site-b".into());
        ui.issue_ui.selected_issue = 0;

        crate::ui::action::dispatch(crate::ui::keymap::Action::OpenCrossLinks, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::CrossLink(ov)) => assert_eq!(ov.site_id.as_deref(), Some("site-b")),
            other => panic!("expected cross-link overlay, got {other:?}"),
        }
        ui.pop_overlay();

        app.jump_to_cross_ref_for_site(
            &mut ui,
            Some("site-b"),
            CrossRefTarget::ConfluencePage {
                page_id: "555".into(),
                title: None,
            },
        );
        assert_eq!(app.nav_history.len(), 1);
        assert_eq!(app.nav_history[0].site_id.as_deref(), Some("site-b"));

        // Back reopens SHARED-1 on site B: the site-B row is selected, not site A's.
        assert!(app.pop_nav_history(&mut ui));
        assert_eq!(ui.active_tab, WorkspaceTab::Issues);
        assert_eq!(ui.issue_ui.selected_issue, 1);
    }

    #[tokio::test]
    async fn image_preview_is_keyed_and_fetched_by_owner_site() {
        let (mut app, mut ui) = two_site_app();
        shared_key_rows(&mut app, &mut ui);
        app.active_issue_site_id = Some("site-b".into());
        app.active_issue.as_mut().unwrap().attachments = vec![AttachmentItem {
            id: "10".into(),
            filename: "shot.png".into(),
            mime_type: "image/png".into(),
            ..Default::default()
        }];
        crate::ui::action::dispatch(crate::ui::keymap::Action::OpenImagePreview, &mut app, &mut ui);
        let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
            panic!("expected image preview overlay, got {:?}", ui.overlays.last());
        };
        assert_eq!(ov.site_id.as_deref(), Some("site-b"));
        let att = ov.current().unwrap().clone();
        assert_eq!(ov.cache_id(&att), "site-b/10");
        let mut unsited = ov.clone();
        unsited.site_id = None;
        assert_eq!(unsited.cache_id(&att), "10");
    }

    #[tokio::test]
    async fn default_space_index_prefers_configured_key() {
        let (mut app, _ui) = two_site_app();
        app.site_scope = SiteScope::All;
        app.config.active_site = None;
        app.spaces = vec![space("AAA"), space("ENG"), space("ENG")];
        app.space_site_ids = vec!["site-b".into(), "site-b".into(), "site-a".into()];
        assert_eq!(app.resolve_default_space_index(), 0, "nothing configured");

        app.config.default_space_key = Some(" eng ".into());
        assert_eq!(
            app.resolve_default_space_index(),
            2,
            "case-insensitive key on the configured site (site-a)"
        );
        app.config.default_site = Some("site-b".into());
        assert_eq!(app.resolve_default_space_index(), 1, "site-b's ENG");
        app.config.default_site = Some("site-a".into());
        app.space_site_ids = vec!["site-a".into(), "site-b".into(), "site-c".into()];
        assert_eq!(
            app.resolve_default_space_index(),
            1,
            "first ENG without a site match"
        );

        app.config.default_space_key = Some("NOPE".into());
        assert_eq!(app.resolve_default_space_index(), 0, "unknown key");
    }

    #[tokio::test]
    async fn default_board_index_prefers_board_id_then_project() {
        let (mut app, _ui) = two_site_app();
        app.boards = vec![
            board(1, "OPS"),
            board(2, "ENG"),
            board(42, "WEB"),
            board(5, "ENG"),
        ];
        app.board_site_ids = vec!["site-a".into(); 4];
        assert_eq!(app.resolve_default_board_index(), 0, "nothing configured");

        app.config.default_project = Some("eng".into());
        assert_eq!(
            app.resolve_default_board_index(),
            1,
            "first board of the project"
        );
        app.config.default_board_id = Some(42);
        assert_eq!(
            app.resolve_default_board_index(),
            2,
            "board id beats project"
        );
        app.config.default_board_id = Some(999);
        assert_eq!(
            app.resolve_default_board_index(),
            1,
            "missing id falls back to project"
        );
        app.config.default_project = Some("NONE".into());
        assert_eq!(app.resolve_default_board_index(), 0);
    }

    #[tokio::test]
    async fn site_switch_selects_default_space_not_zero() {
        let (mut app, mut ui) = two_site_app();
        app.config.default_space_key = Some("OPS".into());
        app.storage
            .upsert_spaces_for_site("site-b", &[space("API"), space("ENG"), space("OPS")])
            .unwrap();
        app.storage
            .upsert_boards_for_site("site-b", &[board(7, "API"), board(8, "OPS")])
            .unwrap();
        app.config.default_project = Some("OPS".into());
        // The user had moved on site A; a site switch starts from the defaults.
        ui.confluence_ui.user_moved_space = true;
        ui.board_ui.user_moved_board = true;

        app.apply_site_switcher_command(SiteSwitcherCommand::SelectSite("site-b".into()), &mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 2);
        assert_eq!(app.active_space(&ui).unwrap().key, "OPS");
        assert_eq!(app.active_board(&ui).unwrap().id, 8);

        // A background reload keeps a selection the user made.
        ui.confluence_ui.selected_space = 1;
        ui.confluence_ui.user_moved_space = true;
        app.load_cached_initial();
        app.apply_default_selection(&mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 1);

        // All Sites: the default site's OPS, after site-a's rows.
        app.storage
            .upsert_spaces_for_site("site-a", &[space("OPS"), space("ZED")])
            .unwrap();
        app.config.default_site = Some("site-b".into());
        app.apply_site_switcher_command(SiteSwitcherCommand::SelectAllSites, &mut ui);
        let idx = ui.confluence_ui.selected_space;
        assert_eq!(idx, 4, "site-a: OPS, ZED; site-b: API, ENG, OPS");
        assert_eq!(app.active_space(&ui).unwrap().key, "OPS");
        assert_eq!(app.active_space_site_id(&ui), Some("site-b"));
    }

    #[tokio::test]
    async fn preload_reads_root_pages_of_default_space() {
        let (mut app, mut ui) = two_site_app();
        app.config.default_space_key = Some("OPS".into());
        app.storage
            .upsert_spaces_for_site("site-a", &[space("ENG"), space("OPS")])
            .unwrap();
        app.storage
            .upsert_spaces_for_site("site-b", &[space("OPS")])
            .unwrap();
        app.storage
            .upsert_authoritative_pages_for_site("site-a", &[page("10", "OPS", "A ops home")])
            .unwrap();
        app.storage
            .upsert_authoritative_pages_for_site("site-a", &[page("11", "ENG", "A eng home")])
            .unwrap();
        app.storage
            .upsert_authoritative_pages_for_site("site-b", &[page("20", "OPS", "B ops home")])
            .unwrap();

        // Single site (site-a): the default space's roots, not the first space's.
        app.load_cached_initial();
        assert!(app
            .root_pages_by_space
            .contains_key(&SiteKey::new("site-a", "OPS")));
        assert!(!app
            .root_pages_by_space
            .contains_key(&SiteKey::new("site-a", "ENG")));
        app.apply_default_selection(&mut ui);
        assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "A ops home");

        // All Sites with site-b as the default site: site-b's OPS.
        app.root_pages_by_space.clear();
        app.site_scope = SiteScope::All;
        app.config.active_site = None;
        app.config.default_site = Some("site-b".into());
        app.load_cached_initial();
        assert!(app
            .root_pages_by_space
            .contains_key(&SiteKey::new("site-b", "OPS")));
        assert!(!app
            .root_pages_by_space
            .contains_key(&SiteKey::new("site-a", "ENG")));
        app.apply_default_selection(&mut ui);
        assert_eq!(app.active_space_site_id(&ui), Some("site-b"));
        assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "B ops home");
    }
}
