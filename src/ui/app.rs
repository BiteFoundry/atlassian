//! Application & UI state, Kanban grouping, Confluence page tree, cross-app history, and async tasks.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use regex::Regex;
use tokio::sync::mpsc;

use crate::{
    adf,
    atlassian::AtlassianClient,
    auth::AuthManager,
    config::{AppConfig, Persona},
    img_preview::{self, RenderedTerminalImage},
    models::{
        AttachmentItem, BoardColumn, BoardSummary, CoreEvent, CrossRefTarget, DownloadedFile,
        IssueDetail, IssueSummary, IssueTransition, IssueUpdate, OutgoingIssue, OutgoingPage,
        PageDetail, PageSummary, PageUpdate, ProjectSummary, SavedFilter, SpaceSummary,
        SprintSummary, StatusCategory, UnifiedSearchHit, UpdateCheckResult, UserSummary,
    },
    storage::{CacheCounts, Storage},
    sync::{self, SyncController},
    ui::{
        hitmap::HitMap,
        keymap::Keymap,
        overlays::{
            assignee::AssigneeOverlay,
            comment::{CommentOverlay, CommentTarget},
            issue_form::IssueFormOverlay,
            page_form::PageFormOverlay,
            path_input::{PathInputOverlay, UploadTarget},
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
    "1:보드",
    "2:이슈/JQL",
    "3:내 작업",
    "4:위키/트리",
    "5:통합검색",
    "6:프로젝트/동기화",
];

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
        to_status: crate::models::IssueStatus,
        toast: String,
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
        pages: Vec<PageSummary>,
    },
    RemoteSearchLoaded {
        query: String,
        hits: Result<Vec<UnifiedSearchHit>, String>,
    },
    ImageLoaded {
        cache_key: String,
        rendered: Result<RenderedTerminalImage, String>,
    },
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
    pub editor_request: Option<(ExternalEditorTarget, String)>,
    pub board_ui: BoardUiState,
    pub issue_ui: IssueUiState,
    pub my_work_ui: MyWorkUiState,
    pub confluence_ui: ConfluenceUiState,
    pub search_ui: SearchUiState,
    pub directory_ui: DirectoryUiState,
}

impl UiState {
    pub fn from_config_with_persona(config: &AppConfig, persona: Persona) -> Self {
        let active_tab = WorkspaceTab::from_persona(persona);
        Self {
            active_tab,
            focus: PaneFocus::Main,
            theme: Theme::default(),
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
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,

    pub me: Option<UserSummary>,
    pub syncing: bool,
    pub sync_summary: String,

    pub projects: Vec<ProjectSummary>,
    pub boards: Vec<BoardSummary>,
    pub sprints: Vec<SprintSummary>,
    pub board_issues: Vec<IssueSummary>,

    pub saved_filters: Vec<SavedFilter>,
    pub issues: Vec<IssueSummary>,
    pub active_issue: Option<IssueDetail>,

    pub spaces: Vec<SpaceSummary>,
    pub root_pages_by_space: HashMap<String, Vec<PageSummary>>,
    pub page_children_map: HashMap<String, Vec<PageSummary>>,
    pub expanded_pages: HashSet<String>,
    pub active_page: Option<PageDetail>,

    pub my_issues: Vec<IssueSummary>,
    pub my_pages: Vec<PageSummary>,

    pub search_hits: Vec<UnifiedSearchHit>,
    pub cache_counts: CacheCounts,

    /// Cross-App navigation stack (`Jira <-> Confluence`).
    pub nav_history: Vec<CrossRefTarget>,

    /// Decoded Half-Block image cache (`"{att_id}:{cols}x{rows}"` -> `RenderedTerminalImage`).
    pub image_cache: HashMap<String, RenderedTerminalImage>,
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
        Self {
            config,
            persona,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
            me: None,
            syncing: false,
            sync_summary: String::new(),
            projects: Vec::new(),
            boards: Vec::new(),
            sprints: Vec::new(),
            board_issues: Vec::new(),
            saved_filters,
            issues: Vec::new(),
            active_issue: None,
            spaces: Vec::new(),
            root_pages_by_space: HashMap::new(),
            page_children_map: HashMap::new(),
            expanded_pages: HashSet::new(),
            active_page: None,
            my_issues: Vec::new(),
            my_pages: Vec::new(),
            search_hits: Vec::new(),
            cache_counts: CacheCounts::default(),
            nav_history: Vec::new(),
            image_cache: HashMap::new(),
        }
    }

    pub fn image_cache_key(att_id: &str, cols: u16, rows: u16) -> String {
        format!("{att_id}:{cols}x{rows}")
    }

    pub fn site_display(&self) -> String {
        let raw = self.config.site_url.trim();
        if raw.is_empty() {
            "미설정".to_string()
        } else {
            raw.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        }
    }

    pub fn load_cached_initial(&mut self) {
        if let Ok(projs) = self.storage.list_projects() {
            self.projects = projs;
        }
        if let Ok(boards) = self.storage.list_boards(None) {
            self.boards = boards;
            if let Some(first_board) = self.boards.first() {
                let bid = first_board.id;
                if let Ok(sprints) = self.storage.list_sprints(bid) {
                    self.sprints = sprints;
                }
                if let Ok(bi) = self.storage.list_board_issues(bid, None, 150) {
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
        if let Ok(issues) = self.storage.list_issues(None, 200) {
            self.issues = issues.clone();
            self.my_issues = issues.iter().take(20).cloned().collect();
            if let Some(first) = issues.first() {
                if let Ok(Some(detail)) = self.storage.get_issue_detail(&first.key) {
                    self.active_issue = Some(detail);
                }
            }
        }
        if let Ok(spaces) = self.storage.list_spaces() {
            self.spaces = spaces;
            if let Some(first_space) = self.spaces.first() {
                if let Ok(roots) = self.storage.list_space_root_pages(&first_space.key) {
                    self.root_pages_by_space
                        .insert(first_space.key.clone(), roots);
                }
            }
        }
        if let Ok(pages) = self.storage.list_recent_pages(30) {
            self.my_pages = pages;
        }
        if let Ok(counts) = self.storage.cache_counts() {
            self.cache_counts = counts;
        }
        self.refresh_local_search("");
    }

    pub fn spawn_bootstrap(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if !self.auth.is_logged_in() || self.config.site_url.trim().is_empty() {
            ui.info("오프라인 캐시 모드 (`jira login`으로 연결하세요)");
            return;
        }
        ui.tasks.begin("초기 동기화");
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
        let q = ui.issue_ui.local_filter.text.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.issues.clone();
        }
        self.issues
            .iter()
            .filter(|it| {
                it.key.to_ascii_lowercase().contains(&q)
                    || it.summary.to_ascii_lowercase().contains(&q)
                    || it.status.name.to_ascii_lowercase().contains(&q)
                    || it
                        .assignee
                        .as_ref()
                        .is_some_and(|a| a.display_name.to_ascii_lowercase().contains(&q))
            })
            .cloned()
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
        if let Ok(hits) = self.storage.search_unified(query, 50) {
            self.search_hits = hits;
        }
    }

    // ------------------------------------------------------------------
    // Async requests
    // ------------------------------------------------------------------

    pub fn request_board_load(&self, ui: &mut UiState, board_id: i64, sprint_id: Option<i64>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("보드 #{board_id}"));
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
        // Optimistic local column update so Kanban card moves right away!
        self.apply_local_status_update(key, &tr.to_status);
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} → {} 전환됨", tr.to_status.name));
            return;
        }
        ui.tasks.begin(format!("{key} 전환"));
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
                        toast: format!("{key_owned} → {} 전환 완료", tr.to_status.name),
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
                    let _ = tx.send(UiMsg::IssueActionFailed(format!("상태 전환 실패: {e}")));
                }
            }
        });
    }

    fn apply_local_status_update(&mut self, key: &str, to_status: &crate::models::IssueStatus) {
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

    pub fn request_assign_to_me(&mut self, ui: &mut UiState, key: &str) {
        let Some(me) = self.me.clone().or_else(|| self.client.cached_me()) else {
            ui.error("현재 사용자 정보가 아직 로드되지 않았습니다");
            return;
        };
        for it in &mut self.board_issues {
            if it.key == key {
                it.assignee = Some(me.clone());
            }
        }
        for it in &mut self.issues {
            if it.key == key {
                it.assignee = Some(me.clone());
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key {
                detail.summary.assignee = Some(me.clone());
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} 내게 할당됨"));
            return;
        }
        ui.tasks.begin(format!("{key} 내게 할당"));
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
                            toast: format!("{key_owned} 내게 할당 완료"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(e.to_string()));
                    }
                },
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueActionFailed(format!("내게 할당 실패: {e}")));
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

    pub fn request_assign_user(&self, ui: &mut UiState, key: &str, user: Option<UserSummary>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("{key} 담당자 지정"));
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        let label = user
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| "미할당".to_string());
        tokio::spawn(async move {
            let id_ref = user.as_ref().map(|u| u.id.as_str());
            match client.assign_issue(&key_owned, id_ref).await {
                Ok(()) => match client.get_issue(&key_owned).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail(&detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            detail,
                            toast: format!("{key_owned} 담당자 → {label}"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(e.to_string()));
                    }
                },
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueActionFailed(format!("담당자 지정 실패: {e}")));
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
            ui.tasks.begin(format!("{key} 수정"));
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
                            toast: format!("{key} 수정 완료"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(format!("이슈 수정 실패: {e}")));
                    }
                }
            });
        } else {
            ui.tasks.begin("이슈 생성");
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
                            toast: format!("{k} 생성 완료"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed(format!("이슈 생성 실패: {e}")));
                    }
                }
            });
        }
    }

    pub fn request_submit_comment(&self, ui: &mut UiState, overlay: &CommentOverlay) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        let body = overlay.body.text.clone();
        match &overlay.target {
            CommentTarget::Issue(key) => {
                ui.tasks.begin(format!("{key} 댓글"));
                let k = key.clone();
                tokio::spawn(async move {
                    let res = client
                        .add_issue_comment(&k, &body)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::CommentAdded {
                        issue_key: Some(k),
                        page_id: None,
                        result: res,
                    });
                });
            }
            CommentTarget::Page { page_id, title } => {
                ui.tasks.begin(format!("{title} 댓글"));
                let pid = page_id.clone();
                tokio::spawn(async move {
                    let res = client
                        .add_page_comment(&pid, &body)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::CommentAdded {
                        issue_key: None,
                        page_id: Some(pid),
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
        ui.tasks.begin(format!("스페이스 {space_key}"));
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
        ui.tasks.begin(format!("하위 페이지: {}", page.title));
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
            ui.tasks.begin("페이지 수정");
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
                            toast: "Confluence 페이지 저장 완료".to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed(format!("페이지 수정 실패: {e}")));
                    }
                }
            });
        } else {
            ui.tasks.begin("페이지 생성");
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
                            toast: "새 Confluence 페이지 생성 완료".to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed(format!("페이지 생성 실패: {e}")));
                    }
                }
            });
        }
    }

    pub fn request_my_work_refresh(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("내 작업 불러오기");
        let client = self.client.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let issues = match client
                .search_issues_jql(AtlassianClient::my_open_issues_jql(), 40, None, None)
                .await
            {
                Ok(p) => {
                    let _ = storage.upsert_issues(&p.issues);
                    p.issues
                }
                Err(_) => storage.list_issues(None, 25).unwrap_or_default(),
            };
            let pages = match client
                .search_confluence_cql("type = page ORDER BY lastmodified DESC", 25)
                .await
            {
                Ok(ps) => {
                    let _ = storage.upsert_pages(&ps);
                    ps
                }
                Err(_) => storage.list_recent_pages(25).unwrap_or_default(),
            };
            let _ = tx.send(UiMsg::MyWorkLoaded { issues, pages });
        });
    }

    pub fn request_remote_search(&self, ui: &mut UiState, query: &str) {
        let q = query.trim().to_string();
        if q.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("원격 검색: {q}"));
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let escaped = q.replace('"', "\\\"");
            let jql = format!("text ~ \"{escaped}\" ORDER BY updated DESC");
            let cql = format!("text ~ \"{escaped}\" AND type = page");
            let (j_res, c_res) = tokio::join!(
                client.search_issues_jql(&jql, 25, None, None),
                client.search_confluence_cql(&cql, 25),
            );
            let mut hits = Vec::new();
            let mut err_msg = None;
            match j_res {
                Ok(page) => {
                    for it in page.issues {
                        hits.push(UnifiedSearchHit {
                            kind: crate::models::SearchHitKind::JiraIssue,
                            id_or_key: it.key,
                            container: it.project_key,
                            title: it.summary,
                            snippet: it.status.name.clone(),
                            status_or_version: it.status.name,
                            updated_at: it.updated_at,
                            web_url: it.web_url,
                        });
                    }
                }
                Err(e) => err_msg = Some(e.to_string()),
            }
            match c_res {
                Ok(pages) => {
                    for pg in pages {
                        hits.push(UnifiedSearchHit {
                            kind: crate::models::SearchHitKind::ConfluencePage,
                            id_or_key: pg.id,
                            container: pg.space_key,
                            title: pg.title,
                            snippet: format!("v{}", pg.version_number),
                            status_or_version: format!("v{}", pg.version_number),
                            updated_at: pg.updated_at,
                            web_url: pg.web_url,
                        });
                    }
                }
                Err(e) => {
                    if err_msg.is_none() {
                        err_msg = Some(e.to_string());
                    }
                }
            }
            let res = if hits.is_empty() {
                if let Some(err) = err_msg {
                    Err(err)
                } else {
                    Ok(hits)
                }
            } else {
                Ok(hits)
            };
            let _ = tx.send(UiMsg::RemoteSearchLoaded { query: q, hits: res });
        });
    }

    pub fn request_upload_attachment(&self, ui: &mut UiState, overlay: &PathInputOverlay) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let path = PathBuf::from(overlay.path.text.trim());
        ui.tasks.begin("첨부파일 업로드");
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        match &overlay.target {
            UploadTarget::Issue(key) => {
                let k = key.clone();
                tokio::spawn(async move {
                    let res = client
                        .upload_issue_attachment(&k, &path)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        issue_key: Some(k),
                        page_id: None,
                        result: res,
                    });
                });
            }
            UploadTarget::Page(pid) => {
                let id = pid.clone();
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
        ui.tasks.begin(format!("다운로드: {}", att.filename));
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
        let Some(att) = ov.current().cloned() else {
            return;
        };
        let (cols, rows) = ov.target_dimensions(ui.term_width, ui.term_height);
        let cache_key = Self::image_cache_key(&att.id, cols, rows);
        if self.image_cache.contains_key(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let client = self.client.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => img_preview::render_halfblocks(&bytes, cols, rows)
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiMsg::ImageLoaded {
                cache_key,
                rendered: res,
            });
        });
    }

    pub fn request_check_update(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("업데이트 확인");
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
                ui.tasks.finish("동기화", None);
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
                ui.tasks.finish("동기화", Some(err.clone()));
                ui.error(format!("동기화 오류: {err}"));
            }
        }
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::MeLoaded(Ok(me)) => {
                self.me = Some(me);
            }
            UiMsg::MeLoaded(Err(err)) => {
                ui.error(format!("사용자 확인 실패: {err}"));
            }
            UiMsg::BootstrapFinished(res) => {
                match res {
                    Ok(summary) => {
                        self.sync_summary = summary.clone();
                        ui.tasks.finish("초기 동기화", None);
                        self.load_cached_initial();
                        ui.success(summary);
                    }
                    Err(err) => {
                        ui.tasks.finish("초기 동기화", Some(err.clone()));
                        ui.error(format!("초기 동기화 실패: {err}"));
                    }
                }
            }
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
                ui.tasks.finish(&format!("보드 #{board_id}"), None);
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
                ui.error(format!("보드 로드 실패: {err}"));
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
                ui.error(format!("JQL 검색 실패: {err}"));
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
                Err(err) => ui.error(format!("이슈 상세 로드 실패: {err}")),
            },
            UiMsg::IssueMutated { detail, toast } => {
                let key = detail.summary.key.clone();
                if let Some(it) = self.issues.iter_mut().find(|i| i.key == key) {
                    *it = detail.summary.clone();
                } else {
                    self.issues.insert(0, detail.summary.clone());
                }
                if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == key) {
                    *it = detail.summary.clone();
                }
                self.active_issue = Some(detail);
                ui.success(toast);
            }
            UiMsg::IssueTransitioned {
                key,
                to_status,
                toast,
            } => {
                ui.tasks.finish(&format!("{key} 전환"), None);
                self.apply_local_status_update(&key, &to_status);
                ui.success(toast);
            }
            UiMsg::IssueActionFailed(err) => {
                ui.error(err);
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
                result,
            } => match result {
                Ok(()) => {
                    ui.success("댓글이 등록되었습니다");
                    if let Some(k) = issue_key {
                        self.request_issue_detail(ui, &k);
                    }
                    if let Some(pid) = page_id {
                        self.request_page_detail(ui, &pid);
                    }
                }
                Err(e) => ui.error(format!("댓글 등록 실패: {e}")),
            },
            UiMsg::SpacesLoaded(Ok(spaces)) => self.spaces = spaces,
            UiMsg::SpacesLoaded(Err(e)) => ui.error(e),
            UiMsg::RootPagesLoaded { space_key, pages } => {
                ui.tasks.finish(&format!("스페이스 {space_key}"), None);
                match pages {
                    Ok(list) => {
                        self.root_pages_by_space.insert(space_key, list);
                    }
                    Err(e) => ui.error(format!("루트 페이지 로드 실패: {e}")),
                }
            }
            UiMsg::ChildPagesLoaded {
                parent_id,
                children,
            } => match children {
                Ok(list) => {
                    self.page_children_map.insert(parent_id, list);
                }
                Err(e) => ui.error(format!("하위 페이지 로드 실패: {e}")),
            },
            UiMsg::PageDetailLoaded { page_id: _, detail } => match detail {
                Ok(d) => self.active_page = Some(d),
                Err(e) => ui.error(format!("페이지 로드 실패: {e}")),
            },
            UiMsg::PageMutated { detail, toast } => {
                let sk = detail.summary.space_key.clone();
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
                    let roots = self.root_pages_by_space.entry(sk).or_default();
                    if let Some(existing) = roots.iter_mut().find(|p| p.id == detail.summary.id) {
                        *existing = detail.summary.clone();
                    } else {
                        roots.push(detail.summary.clone());
                    }
                }
                self.active_page = Some(detail);
                ui.success(toast);
            }
            UiMsg::PageActionFailed(err) => ui.error(err),
            UiMsg::MyWorkLoaded { issues, pages } => {
                ui.tasks.finish("내 작업 불러오기", None);
                self.my_issues = issues;
                self.my_pages = pages;
            }
            UiMsg::RemoteSearchLoaded { query, hits } => {
                ui.tasks.finish(&format!("원격 검색: {query}"), None);
                match hits {
                    Ok(list) => {
                        self.search_hits = list;
                        ui.search_ui.is_remote = true;
                        ui.search_ui.selected_hit = 0;
                    }
                    Err(e) => ui.error(format!("원격 검색 실패: {e}")),
                }
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
            UiMsg::AttachmentDownloaded(res) => match res {
                Ok(file) => ui.success(format!("저장됨: {}", file.path.display())),
                Err(e) => ui.error(format!("다운로드 실패: {e}")),
            },
            UiMsg::AttachmentUploaded {
                issue_key,
                page_id,
                result,
            } => match result {
                Ok(_) => {
                    ui.success("첨부파일 업로드 완료");
                    if let Some(k) = issue_key {
                        self.request_issue_detail(ui, &k);
                    }
                    if let Some(pid) = page_id {
                        self.request_page_detail(ui, &pid);
                    }
                }
                Err(e) => ui.error(format!("업로드 실패: {e}")),
            },
            UiMsg::UpdateChecked(res) => {
                ui.tasks.finish("업데이트 확인", None);
                match res {
                    Ok(info) => {
                        if info.update_available {
                            ui.info(format!(
                                "새 버전 {} 사용 가능 (현재 {})",
                                info.latest_version, info.current_version
                            ));
                        } else {
                            ui.success(format!("최신 버전입니다 ({})", info.current_version));
                        }
                        ui.directory_ui.last_update = Some(info);
                    }
                    Err(e) => ui.error(format!("업데이트 확인 실패: {e}")),
                }
            }
        }
    }
}

pub fn builtin_filters() -> Vec<SavedFilter> {
    vec![
        SavedFilter {
            id: "builtin:my-open".to_string(),
            name: "내게 할당된 미해결".to_string(),
            jql: AtlassianClient::my_open_issues_jql().to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:reported".to_string(),
            name: "내가 보고한 이슈".to_string(),
            jql: "reporter = currentUser() ORDER BY updated DESC".to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:recent".to_string(),
            name: "최근 업데이트".to_string(),
            jql: "ORDER BY updated DESC".to_string(),
            builtin: true,
        },
        SavedFilter {
            id: "builtin:all-open".to_string(),
            name: "미해결 전체".to_string(),
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
            title: "아키텍처".into(),
            has_children: true,
            ..Default::default()
        };
        let child = PageSummary {
            id: "101".into(),
            parent_id: Some("100".into()),
            title: "TUI 설계".into(),
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
        assert_eq!(open[1].page.title, "TUI 설계");
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
                title: Some("설계 문서".into()),
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
}
