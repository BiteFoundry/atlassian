//! UI state containers, workspace tabs, pane focus, and async UI message definitions.

use std::{collections::HashSet, time::Instant};

use crate::{
    config::{
        AppConfig, DensityMode, ImageProtocolPreference as ConfigImageProtocol, Persona,
        SiteProfile,
    },
    img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps},
    models::{
        AttachmentItem, BoardColumn, BoardSummary, ConfluenceComment, DownloadedFile, IssueComment,
        IssueDetail, IssueStatus, IssueSummary, IssueTransition, PageDetail, PageSummary,
        SavedFilter, SpaceSummary, SprintSummary, StatusCategory, UpdateCheckResult, UserSummary,
    },
    sync::MultiSiteSyncOutcome,
    ui::{
        hitmap::HitMap,
        keymap::Keymap,
        overlays::Overlay,
        text_edit::TextBuffer,
        theme::Theme,
        toast::{TaskBoard, ToastQueue},
    },
};

use super::RemoteSearchHits;

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
    /// Children are confirmed: `has_children == Some(true)` or loaded children exist.
    pub has_children: bool,
    /// The row can be expanded: not a confirmed leaf. Unknown (`has_children == None`, children
    /// not loaded yet) is expandable, so expanding it loads the children and settles the row.
    pub expandable: bool,
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
        hits: Result<RemoteSearchHits, String>,
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
        content_kind: bitefoundry_html::ContentKind,
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

