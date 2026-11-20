//! Shared normalized domain models across Jira, Jira Agile, Confluence, ADF/Rich Documents, Storage, Sync, and TUI.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Jira status category (Atlassian's 3-color lozenge taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatusCategory {
    #[default]
    Todo,
    InProgress,
    Done,
}

impl StatusCategory {
    pub fn from_atlassian_key(key: &str) -> Self {
        match key.trim().to_ascii_lowercase().as_str() {
            "indeterminate" | "inprogress" | "in_progress" | "in progress" => Self::InProgress,
            "done" | "complete" | "completed" => Self::Done,
            _ => Self::Todo,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    pub fn badge_label(self) -> &'static str {
        match self {
            Self::Todo => "TO DO",
            Self::InProgress => "IN PROGRESS",
            Self::Done => "DONE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserSummary {
    /// Cloud `accountId` or Data Center `name`/`key`.
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssueStatus {
    pub id: String,
    pub name: String,
    pub category: StatusCategory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssuePriority {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssueType {
    pub id: String,
    pub name: String,
    pub subtask: bool,
    pub epic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectSummary {
    pub id: String,
    pub key: String,
    pub name: String,
    pub project_type: String,
    pub lead_name: Option<String>,
    pub avatar_url: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BoardColumn {
    pub name: String,
    pub status_ids: Vec<String>,
    pub status_names: Vec<String>,
    pub category_hint: StatusCategory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BoardSummary {
    pub id: i64,
    pub name: String,
    /// `"scrum"` | `"kanban"` | `"simple"`
    pub board_type: String,
    pub project_key: Option<String>,
    pub project_name: Option<String>,
    pub columns: Vec<BoardColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SprintSummary {
    pub id: i64,
    pub board_id: i64,
    pub name: String,
    /// `"active"` | `"future"` | `"closed"`
    pub state: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub goal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttachmentItem {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub author_name: Option<String>,
    pub created_at: String,
    pub content_url: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub filename: String,
    pub size: u64,
    pub mime_type: String,
}

// ============================================================================
// Unified Rich Document AST (`DocBlock` — ADF JSON / Storage XHTML / Wiki Markup)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub link_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InlineNode {
    Text {
        text: String,
        #[serde(default)]
        style: InlineStyle,
    },
    Mention {
        id: String,
        display: String,
    },
    StatusLozenge {
        text: String,
        category: StatusCategory,
    },
    IssueRef {
        key: String,
    },
    PageRef {
        page_id: String,
        title: String,
    },
    Emoji {
        text: String,
    },
    HardBreak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PanelKind {
    #[default]
    Info,
    Note,
    Warning,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    pub checked: bool,
    pub content: Vec<InlineNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum DocBlock {
    Paragraph {
        spans: Vec<InlineNode>,
    },
    Heading {
        level: u8,
        spans: Vec<InlineNode>,
    },
    BulletList {
        items: Vec<Vec<DocBlock>>,
    },
    OrderedList {
        start: usize,
        items: Vec<Vec<DocBlock>>,
    },
    TaskList {
        items: Vec<TaskItem>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Blockquote {
        blocks: Vec<DocBlock>,
    },
    Panel {
        kind: PanelKind,
        title: Option<String>,
        blocks: Vec<DocBlock>,
    },
    Expand {
        title: String,
        blocks: Vec<DocBlock>,
    },
    Table {
        headers: Vec<Vec<InlineNode>>,
        rows: Vec<Vec<Vec<InlineNode>>>,
    },
    Media {
        id_or_filename: String,
        alt: Option<String>,
        url: Option<String>,
    },
    Rule,
}

// ============================================================================
// Jira Issue Models
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubtaskSummary {
    pub key: String,
    pub summary: String,
    pub status: IssueStatus,
    pub issue_type: String,
    pub assignee_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LinkedIssueItem {
    pub link_type: String,
    /// `"outward"` | `"inward"`
    pub direction: String,
    pub relation_label: String,
    pub key: String,
    pub summary: String,
    pub status: IssueStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LinkedConfluencePage {
    pub page_id: String,
    pub title: String,
    pub space_key: Option<String>,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target_kind", rename_all = "snake_case")]
pub enum CrossRefTarget {
    JiraIssue {
        key: String,
        label: Option<String>,
    },
    ConfluencePage {
        page_id: String,
        title: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssueComment {
    pub id: String,
    pub issue_key: String,
    pub author: UserSummary,
    pub body_raw: String,
    pub body_markdown: String,
    pub blocks: Vec<DocBlock>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssueTransition {
    pub id: String,
    pub name: String,
    pub to_status: IssueStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IssueSummary {
    pub id: String,
    pub key: String,
    pub project_key: String,
    pub summary: String,
    pub status: IssueStatus,
    pub priority: Option<IssuePriority>,
    pub issue_type: IssueType,
    pub assignee: Option<UserSummary>,
    pub reporter: Option<UserSummary>,
    pub parent_key: Option<String>,
    pub parent_summary: Option<String>,
    pub labels: Vec<String>,
    pub story_points: Option<f64>,
    pub sprint_id: Option<i64>,
    pub sprint_name: Option<String>,
    pub comment_count: usize,
    pub attachment_count: usize,
    pub created_at: String,
    pub updated_at: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IssueDetail {
    pub summary: IssueSummary,
    pub description_raw: String,
    pub description_markdown: String,
    pub description_blocks: Vec<DocBlock>,
    pub fix_versions: Vec<String>,
    pub components: Vec<String>,
    pub subtasks: Vec<SubtaskSummary>,
    pub linked_issues: Vec<LinkedIssueItem>,
    pub linked_pages: Vec<LinkedConfluencePage>,
    pub attachments: Vec<AttachmentItem>,
    pub comments: Vec<IssueComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutgoingIssue {
    pub project_key: String,
    pub issue_type: String,
    pub summary: String,
    pub description_markdown: String,
    pub priority: Option<String>,
    pub assignee_id: Option<String>,
    pub labels: Vec<String>,
    pub parent_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IssueUpdate {
    pub summary: Option<String>,
    pub description_markdown: Option<String>,
    pub priority: Option<String>,
    /// `Some(Some(id))` assigns user, `Some(None)` unassigns, `None` leaves unchanged.
    pub assignee_id: Option<Option<String>>,
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JqlSearchPage {
    pub issues: Vec<IssueSummary>,
    pub next_page_token: Option<String>,
    pub next_start_at: Option<usize>,
    pub total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub id: String,
    pub name: String,
    pub jql: String,
    pub builtin: bool,
}

// ============================================================================
// Confluence Models
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpaceSummary {
    pub id: String,
    pub key: String,
    pub name: String,
    /// `"global"` | `"personal"` | `"collaboration"`
    pub space_type: String,
    pub homepage_id: Option<String>,
    pub description: Option<String>,
    pub web_url: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PageAncestor {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PageSummary {
    pub id: String,
    pub space_id: String,
    pub space_key: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub status: String,
    pub version_number: i32,
    pub author_name: Option<String>,
    pub updated_at: String,
    pub position: i32,
    pub has_children: bool,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TocEntry {
    pub level: u8,
    pub text: String,
    pub line_hint: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConfluenceComment {
    pub id: String,
    pub page_id: String,
    pub author_name: String,
    pub body_raw: String,
    pub body_markdown: String,
    pub blocks: Vec<DocBlock>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PageDetail {
    pub summary: PageSummary,
    pub ancestors: Vec<PageAncestor>,
    pub labels: Vec<String>,
    pub body_raw: String,
    /// `"atlas_doc_format"` | `"storage"` | `"markdown"`
    pub body_representation: String,
    pub body_markdown: String,
    pub blocks: Vec<DocBlock>,
    pub toc: Vec<TocEntry>,
    pub attachments: Vec<AttachmentItem>,
    pub comments: Vec<ConfluenceComment>,
    pub linked_issue_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutgoingPage {
    pub space_id_or_key: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub body_markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PageUpdate {
    pub title: String,
    pub body_markdown: String,
    /// Optional raw body (`atlas_doc_format` JSON or Confluence `storage` XHTML) preserved verbatim
    /// on title-only edits so macros, mentions, status lozenges, and panels are not lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_raw_passthrough: Option<String>,
    /// Representation matching `body_raw_passthrough` (`"atlas_doc_format"` or `"storage"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_representation: Option<String>,
    /// Previous version number (client sends `prev_version + 1` to Atlassian).
    pub prev_version: i32,
    pub version_message: Option<String>,
}

// ============================================================================
// Unified Search, Updater & Background Sync Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchHitKind {
    JiraIssue,
    ConfluencePage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedSearchHit {
    pub kind: SearchHitKind,
    pub id_or_key: String,
    pub container: String,
    pub title: String,
    pub snippet: String,
    pub status_or_version: String,
    pub updated_at: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpenItemTarget {
    JiraIssue {
        site_id: String,
        key: String,
    },
    ConfluencePage {
        site_id: String,
        page_id: String,
    },
}

impl OpenItemTarget {
    pub fn site_id(&self) -> &str {
        match self {
            Self::JiraIssue { site_id, .. } | Self::ConfluencePage { site_id, .. } => site_id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    SyncStarted,
    SyncFinished {
        summary: String,
    },
    ProjectsUpdated(Vec<ProjectSummary>),
    BoardsUpdated(Vec<BoardSummary>),
    BoardIssuesUpdated {
        site_id: String,
        board_id: i64,
        sprint: Option<SprintSummary>,
        issues: Vec<IssueSummary>,
    },
    IssuesUpdated {
        site_id: String,
        issues: Vec<IssueSummary>,
    },
    SpacesUpdated(Vec<SpaceSummary>),
    SpacePagesUpdated {
        site_id: String,
        space_key: String,
        pages: Vec<PageSummary>,
    },
    OpenIssueRevision {
        site_id: String,
        summary: IssueSummary,
    },
    OpenPageRevision {
        site_id: String,
        summary: PageSummary,
    },
    SyncError(String),
}

// ============================================================================
// Multi-Site Federation Models
// ============================================================================

/// Scope filter for federated vs single-site queries across SQLite, Sync, TUI, CLI, HTTP, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "scope", content = "site_id", rename_all = "snake_case")]
pub enum SiteScope {
    #[default]
    All,
    Only(String),
}

impl SiteScope {
    pub fn from_optional(site_id: Option<&str>) -> Self {
        match site_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) if !id.eq_ignore_ascii_case("all") => Self::Only(id.to_string()),
            _ => Self::All,
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn site_id(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Only(id) => Some(id.as_str()),
        }
    }

    pub fn matches(&self, site_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(target) => target.eq_ignore_ascii_case(site_id.trim()),
        }
    }
}

/// Aggregated status and cache statistics for one registered Atlassian site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SiteStatusSummary {
    pub site_id: String,
    pub name: String,
    pub site_url: String,
    pub deployment: crate::config::DeploymentKind,
    pub auth_kind: String,
    pub enabled: bool,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub issue_count: usize,
    pub page_count: usize,
}

/// Site-tagged wrapper around [`ProjectSummary`] for federated multi-site views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FederatedProject {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub project: ProjectSummary,
}

impl FederatedProject {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        project: ProjectSummary,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            project,
        }
    }
}

/// Site-tagged wrapper around [`BoardSummary`] for federated multi-site views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FederatedBoard {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub board: BoardSummary,
}

impl FederatedBoard {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        board: BoardSummary,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            board,
        }
    }
}

/// Site-tagged wrapper around [`IssueSummary`] for federated multi-site views.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FederatedIssue {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub issue: IssueSummary,
}

impl FederatedIssue {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        issue: IssueSummary,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            issue,
        }
    }
}

/// Site-tagged wrapper around [`SpaceSummary`] for federated multi-site views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FederatedSpace {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub space: SpaceSummary,
}

impl FederatedSpace {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        space: SpaceSummary,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            space,
        }
    }
}

/// Site-tagged wrapper around [`PageSummary`] for federated multi-site views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FederatedPage {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub page: PageSummary,
}

impl FederatedPage {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        page: PageSummary,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            page,
        }
    }
}

/// Site-tagged wrapper around [`UnifiedSearchHit`] for federated multi-site search results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederatedSearchHit {
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(flatten)]
    pub hit: UnifiedSearchHit,
}

impl FederatedSearchHit {
    pub fn new(
        site_id: impl Into<String>,
        site_name: Option<String>,
        hit: UnifiedSearchHit,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            site_name,
            hit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeploymentKind;

    #[test]
    fn site_scope_matching_and_serde() {
        assert_eq!(SiteScope::from_optional(None), SiteScope::All);
        assert_eq!(SiteScope::from_optional(Some("  ")), SiteScope::All);
        assert_eq!(SiteScope::from_optional(Some("all")), SiteScope::All);
        assert_eq!(
            SiteScope::from_optional(Some("acme")),
            SiteScope::Only("acme".into())
        );

        assert!(SiteScope::All.is_all());
        assert!(SiteScope::All.matches("any-site"));
        assert_eq!(SiteScope::All.site_id(), None);

        let only = SiteScope::Only("acme".into());
        assert!(!only.is_all());
        assert_eq!(only.site_id(), Some("acme"));
        assert!(only.matches("ACME"));
        assert!(!only.matches("corp-dc"));

        let json = serde_json::to_string(&only).unwrap();
        let decoded: SiteScope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, only);
    }

    #[test]
    fn federated_wrappers_flatten_site_metadata_cleanly() {
        let status = SiteStatusSummary {
            site_id: "acme".into(),
            name: "Acme Cloud".into(),
            site_url: "https://acme.atlassian.net".into(),
            deployment: DeploymentKind::Cloud,
            auth_kind: "cloud_oauth".into(),
            enabled: true,
            last_synced_at: Some("2026-11-19T14:00:00Z".into()),
            last_error: None,
            issue_count: 12,
            page_count: 5,
        };
        let status_json = serde_json::to_string(&status).unwrap();
        let status_back: SiteStatusSummary = serde_json::from_str(&status_json).unwrap();
        assert_eq!(status_back, status);

        let hit = FederatedSearchHit::new(
            "acme",
            Some("Acme Cloud".into()),
            UnifiedSearchHit {
                kind: SearchHitKind::JiraIssue,
                id_or_key: "ENG-1".into(),
                container: "ENG".into(),
                title: "Multi-site search".into(),
                snippet: "...search...".into(),
                status_or_version: "In Progress".into(),
                updated_at: "2026-11-19T14:00:00Z".into(),
                web_url: "https://acme.atlassian.net/browse/ENG-1".into(),
            },
        );
        let val = serde_json::to_value(&hit).unwrap();
        assert_eq!(val["site_id"], "acme");
        assert_eq!(val["site_name"], "Acme Cloud");
        assert_eq!(val["id_or_key"], "ENG-1");
        let roundtrip: FederatedSearchHit = serde_json::from_value(val).unwrap();
        assert_eq!(roundtrip, hit);
    }
}
