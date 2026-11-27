//! Application & UI state, Kanban grouping, Confluence page tree, cross-app history, and async tasks.

mod cache;
mod events;
mod helpers;
mod messages;
mod navigation;
mod requests_confluence;
mod requests_jira;
mod requests_shared;
mod state;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use crate::{
    atlassian::{AtlassianClient, SiteClientPool},
    auth::AuthManager,
    config::{AppConfig, DraftStore, Persona},
    img_preview::{DecodedImageCache, RenderedTerminalImage},
    models::{
        BoardSummary, CrossRefTarget, IssueDetail, IssueSummary, OpenItemTarget, PageDetail,
        PageSummary, ProjectSummary, SavedFilter, SiteScope, SpaceSummary, SprintSummary,
        UnifiedSearchHit, UserSummary,
    },
    storage::{CacheCounts, Storage},
    sync::SyncController,
};

pub use helpers::{build_kanban_columns_for_board, builtin_filters, flatten_page_tree, SiteKey};
pub(crate) use state::{CachedLineHit, CachedRenderedDoc};
pub use state::{
    config_protocol_to_img_pref, space_group_key, BoardUiState, ConfluenceUiState,
    DirectoryUiState, ExternalEditorTarget, IssueUiState, KanbanColumn, MyWorkUiState, PaneFocus,
    SearchUiState, SpacePaneRow, TreeRow, UiMsg, UiState, WorkspaceTab, TAB_LABELS,
};

/// Maximum uncached sibling issues/pages warmed per interactive TUI list/tree load.
pub(super) const INTERACTIVE_SIBLING_PREFETCH_LIMIT: usize = 3;
/// Bounded concurrency for interactive TUI sibling detail prefetch.
pub(super) const INTERACTIVE_SIBLING_PREFETCH_CONCURRENCY: usize = 2;

pub(super) type RemoteSearchHits = (Vec<UnifiedSearchHit>, Vec<String>, Vec<Option<String>>);
pub(super) type SiteTaggedIssue = (String, Option<String>, IssueSummary);
pub(super) type SiteTaggedPage = (String, Option<String>, PageSummary);
pub(super) type SiteTaggedHit = (String, Option<String>, UnifiedSearchHit);

pub(super) const FOCUS_CHECK_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
pub(super) struct InFlightIssueDetail {
    pub(super) site_id: String,
    pub(super) key: String,
    pub(super) target_updated_at: Option<String>,
    pub(super) seq: u64,
}

#[derive(Debug, Clone)]
pub(super) struct InFlightPageDetail {
    pub(super) site_id: String,
    pub(super) page_id: String,
    pub(super) target_version: Option<i32>,
    pub(super) seq: u64,
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
    /// Pages the user collapsed in this session; space-home auto-expand leaves them collapsed.
    pub user_collapsed_pages: HashSet<SiteKey>,
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
    pub(super) active_board_sprint_id: Option<i64>,
    /// Timestamp of the last live My Work fetch this session.
    pub(super) last_my_work_refresh_at: Option<Instant>,
    /// Site IDs for currently opened issue / page details.
    pub active_issue_site_id: Option<String>,
    pub active_page_site_id: Option<String>,
    /// Canonical `(site_id, key/page_id)` target currently open or loading on screen.
    pub active_issue_target: Option<(String, String)>,
    pub active_page_target: Option<(String, String)>,
    /// Actual `detail_updated_at` / `detail_version` of the currently loaded detail body
    /// (prevents reconciled summary columns in cached detail from masking a changed revision).
    pub(super) active_issue_detail_updated_at: Option<String>,
    pub(super) active_page_detail_version: Option<i32>,
    /// In-flight remote detail request trackers for cross-path deduplication.
    pub(super) in_flight_issue_detail: Option<InFlightIssueDetail>,
    pub(super) in_flight_page_detail: Option<InFlightPageDetail>,
    /// Last open-item target sent to the background sync engine.
    pub(super) last_watched_open_item: Option<OpenItemTarget>,
    /// Last focus-gain check timestamp for short OS-burst debounce.
    pub(super) last_focus_check_at: Option<Instant>,
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
            user_collapsed_pages: HashSet::new(),
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
}
