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
    /// The board this sprint was listed under.
    pub board_id: i64,
    /// The board that owns the sprint (Jira `originBoardId`); differs from `board_id` when a
    /// sprint is shared across boards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_board_id: Option<i64>,
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
pub struct IssueCustomField {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
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
    /// Collections this read could not confirm in full (failed or truncated subresource).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
    /// Populated Jira `customfield_*` entries keyed by field ID (excluding built-in story points / sprint).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub custom_fields: std::collections::BTreeMap<String, IssueCustomField>,
}

const ISSUE_COMPACT_OMITTED_FIELDS: &[&str] = &[
    "summary.id",
    "summary.project_key",
    "summary.priority",
    "summary.issue_type",
    "summary.assignee",
    "summary.reporter",
    "summary.parent_key",
    "summary.parent_summary",
    "summary.story_points",
    "summary.sprint_id",
    "summary.sprint_name",
    "summary.created_at",
    "summary.updated_at",
    "summary.web_url",
    "description_raw",
    "description_blocks",
];

const PAGE_COMPACT_OMITTED_FIELDS: &[&str] = &[
    "summary.parent_id",
    "summary.status",
    "summary.author_name",
    "summary.updated_at",
    "summary.position",
    "summary.has_children",
    "summary.web_url",
    "body_raw",
    "body_representation",
    "blocks",
];

impl IssueDetail {
    pub(crate) fn output(&self, compact: bool) -> serde_json::Value {
        if compact {
            serde_json::json!({
                "key": self.summary.key,
                "summary": self.summary.summary,
                "status": self.summary.status,
                "description_markdown": self.description_markdown,
            })
        } else {
            serde_json::json!(self)
        }
    }

    pub(crate) fn compact_output(&self, hint: &str) -> serde_json::Value {
        let known = |name: &str, len: usize| -> serde_json::Value {
            if self.incomplete.iter().any(|n| n == name) {
                serde_json::Value::Null
            } else {
                serde_json::json!(len)
            }
        };
        let custom_field_summaries: Vec<serde_json::Value> = self
            .custom_fields
            .iter()
            .map(|(id, cf)| match cf.name.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(name) => serde_json::json!({ "id": id, "name": name }),
                None => serde_json::json!({ "id": id }),
            })
            .collect();
        serde_json::json!({
            "key": self.summary.key,
            "summary": self.summary.summary,
            "status": self.summary.status,
            "description_markdown": self.description_markdown,
            "omitted": {
                "comments": self.summary.comment_count,
                "attachments": self.attachments.len(),
                "subtasks": self.subtasks.len(),
                "linked_issues": self.linked_issues.len(),
                "linked_pages": known("linked_pages", self.linked_pages.len()),
                "fix_versions": self.fix_versions.len(),
                "components": self.components.len(),
                "labels": self.summary.labels.len(),
                "fields": ISSUE_COMPACT_OMITTED_FIELDS,
                "custom_fields": custom_field_summaries,
            },
            "hint": hint,
        })
    }

    /// Returns only the requested standard or custom fields by issue key, keeping responses compact
    /// while distinguishing truly absent fields (`missing_fields`) from omitted ones.
    pub(crate) fn projected_output(&self, requested_fields: &[String]) -> serde_json::Value {
        let mut fields_out = serde_json::Map::new();
        let mut missing_fields: Vec<String> = Vec::new();
        let mut incomplete_out: Vec<String> = Vec::new();

        let mut push_incomplete = |col: &str| {
            if self.incomplete.iter().any(|n| n == col)
                && !incomplete_out.iter().any(|n| n == col)
            {
                incomplete_out.push(col.to_string());
            }
        };

        for raw in requested_fields {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let norm = trimmed.to_ascii_lowercase();
            let stripped = norm
                .strip_prefix("summary.")
                .or_else(|| norm.strip_prefix("custom_fields."))
                .unwrap_or(&norm);

            let matched_standard = match stripped {
                "id" => Some(("summary.id", serde_json::json!(self.summary.id))),
                "key" => Some(("key", serde_json::json!(self.summary.key))),
                "project_key" => Some(("summary.project_key", serde_json::json!(self.summary.project_key))),
                "summary" => Some(("summary", serde_json::json!(self.summary.summary))),
                "status" => Some(("status", serde_json::json!(self.summary.status))),
                "priority" => Some(("summary.priority", serde_json::json!(self.summary.priority))),
                "issue_type" => Some(("summary.issue_type", serde_json::json!(self.summary.issue_type))),
                "assignee" => Some(("summary.assignee", serde_json::json!(self.summary.assignee))),
                "reporter" => Some(("summary.reporter", serde_json::json!(self.summary.reporter))),
                "parent_key" => Some(("summary.parent_key", serde_json::json!(self.summary.parent_key))),
                "parent_summary" => Some(("summary.parent_summary", serde_json::json!(self.summary.parent_summary))),
                "labels" => Some(("labels", serde_json::json!(self.summary.labels))),
                "story_points" => Some(("summary.story_points", serde_json::json!(self.summary.story_points))),
                "sprint_id" => Some(("summary.sprint_id", serde_json::json!(self.summary.sprint_id))),
                "sprint_name" => Some(("summary.sprint_name", serde_json::json!(self.summary.sprint_name))),
                "comment_count" => Some(("summary.comment_count", serde_json::json!(self.summary.comment_count))),
                "attachment_count" => Some(("summary.attachment_count", serde_json::json!(self.summary.attachment_count))),
                "created_at" => Some(("summary.created_at", serde_json::json!(self.summary.created_at))),
                "updated_at" => Some(("summary.updated_at", serde_json::json!(self.summary.updated_at))),
                "web_url" => Some(("summary.web_url", serde_json::json!(self.summary.web_url))),
                "description_markdown" | "description" => {
                    Some(("description_markdown", serde_json::json!(self.description_markdown)))
                }
                "description_raw" => Some(("description_raw", serde_json::json!(self.description_raw))),
                "description_blocks" => Some(("description_blocks", serde_json::json!(self.description_blocks))),
                "fix_versions" => Some(("fix_versions", serde_json::json!(self.fix_versions))),
                "components" => Some(("components", serde_json::json!(self.components))),
                "subtasks" => Some(("subtasks", serde_json::json!(self.subtasks))),
                "linked_issues" => Some(("linked_issues", serde_json::json!(self.linked_issues))),
                "linked_pages" => {
                    push_incomplete("linked_pages");
                    Some(("linked_pages", serde_json::json!(self.linked_pages)))
                }
                "attachments" => Some(("attachments", serde_json::json!(self.attachments))),
                "comments" => {
                    push_incomplete("comments");
                    Some(("comments", serde_json::json!(self.comments)))
                }
                "custom_fields" => Some(("custom_fields", serde_json::json!(self.custom_fields))),
                _ => None,
            };

            if let Some((out_key, val)) = matched_standard {
                fields_out.insert(out_key.to_string(), val);
                continue;
            }

            // Match populated custom field by ID (case-insensitive) or human display name.
            let mut matched_custom = false;
            for (cf_id, cf) in &self.custom_fields {
                let id_matches = cf_id.eq_ignore_ascii_case(stripped);
                let name_matches = cf
                    .name
                    .as_deref()
                    .map(|n| n.trim().eq_ignore_ascii_case(trimmed))
                    .unwrap_or(false);
                if id_matches || name_matches {
                    fields_out.insert(cf_id.clone(), serde_json::json!(cf));
                    matched_custom = true;
                }
            }

            if !matched_custom && !missing_fields.iter().any(|m| m == trimmed) {
                missing_fields.push(trimmed.to_string());
            }
        }

        let mut out = serde_json::json!({
            "key": self.summary.key,
            "fields": fields_out,
        });
        if !missing_fields.is_empty() {
            out["missing_fields"] = serde_json::json!(missing_fields);
        }
        if !incomplete_out.is_empty() {
            out["incomplete"] = serde_json::json!(incomplete_out);
        }
        out
    }

    pub(crate) fn write_output(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.summary.key,
            "summary": self.summary.summary,
            "status": self.summary.status,
            "web_url": self.summary.web_url,
        })
    }
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
    #[serde(
        default,
        deserialize_with = "deserialize_assignee_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub assignee_id: Option<Option<String>>,
    pub labels: Option<Vec<String>>,
}

fn deserialize_assignee_patch<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JqlSearchPage {
    pub issues: Vec<IssueSummary>,
    pub next_page_token: Option<String>,
    pub next_start_at: Option<usize>,
    pub total: Option<usize>,
}

/// One page of a collection plus what the source said about the rest.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListPage<T> {
    pub items: Vec<T>,
    /// None when the source gave no way to tell.
    pub has_more: Option<bool>,
    pub next_cursor: Option<String>,
}

/// Every page of a collection that could be walked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Collected<T> {
    pub items: Vec<T>,
    /// True only when the server said there is nothing more. A cache prune needs this.
    pub complete: bool,
}

pub(crate) const OFFSET_CURSOR_ERROR: &str =
    "cursor must be a non-negative integer offset taken from next_cursor";

/// A missing or blank cursor means offset 0.
pub(crate) fn parse_offset_cursor(cursor: Option<&str>) -> Result<usize, String> {
    match cursor.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(0),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| OFFSET_CURSOR_ERROR.to_string()),
    }
}

/// The input is the whole ordered set, or at least its first offset + limit + 1 rows.
pub(crate) fn slice_page<T>(items: Vec<T>, offset: usize, limit: usize) -> ListPage<T> {
    let has_more = items.len() > offset.saturating_add(limit);
    let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    ListPage {
        items: page,
        has_more: Some(has_more),
        next_cursor: has_more.then(|| offset.saturating_add(limit).to_string()),
    }
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
    pub has_children: Option<bool>,
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
    /// Collections this read could not confirm in full (failed or truncated subresource).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
}

impl PageDetail {
    pub(crate) fn output(&self, compact: bool) -> serde_json::Value {
        if compact {
            serde_json::json!({
                "id": self.summary.id,
                "title": self.summary.title,
                "space_id": self.summary.space_id,
                "space_key": self.summary.space_key,
                "version_number": self.summary.version_number,
                "body_markdown": self.body_markdown,
            })
        } else {
            serde_json::json!(self)
        }
    }

    pub(crate) fn compact_output(&self, hint: &str) -> serde_json::Value {
        let known = |name: &str, len: usize| -> serde_json::Value {
            if self.incomplete.iter().any(|n| n == name) {
                serde_json::Value::Null
            } else {
                serde_json::json!(len)
            }
        };
        serde_json::json!({
            "id": self.summary.id,
            "title": self.summary.title,
            "space_id": self.summary.space_id,
            "space_key": self.summary.space_key,
            "version_number": self.summary.version_number,
            "body_markdown": self.body_markdown,
            "omitted": {
                "comments": known("comments", self.comments.len()),
                "attachments": known("attachments", self.attachments.len()),
                "labels": known("labels", self.labels.len()),
                "ancestors": known("ancestors", self.ancestors.len()),
                "toc": self.toc.len(),
                "linked_issue_keys": self.linked_issue_keys.len(),
                "fields": PAGE_COMPACT_OMITTED_FIELDS,
            },
            "hint": hint,
        })
    }

    pub(crate) fn write_output(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.summary.id,
            "title": self.summary.title,
            "space_id": self.summary.space_id,
            "space_key": self.summary.space_key,
            "version_number": self.summary.version_number,
            "web_url": self.summary.web_url,
        })
    }
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
    ProjectsUpdated {
        site_id: String,
        items: Vec<ProjectSummary>,
    },
    BoardsUpdated {
        site_id: String,
        items: Vec<BoardSummary>,
    },
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
    SpacesUpdated {
        site_id: String,
        items: Vec<SpaceSummary>,
    },
    SpacePagesUpdated {
        site_id: String,
        space_key: String,
        pages: Vec<PageSummary>,
    },
    OpenIssueRevision {
        site_id: String,
        summary: Box<IssueSummary>,
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

    #[test]
    fn slice_page_boundaries() {
        let p0 = slice_page(Vec::<usize>::new(), 0, 50);
        assert_eq!(p0.items.len(), 0);
        assert_eq!(p0.has_more, Some(false));
        assert_eq!(p0.next_cursor, None);

        let p50 = slice_page((0..50).collect(), 0, 50);
        assert_eq!(p50.items.len(), 50);
        assert_eq!(p50.has_more, Some(false));
        assert_eq!(p50.next_cursor, None);

        let p51_first = slice_page((0..51).collect(), 0, 50);
        assert_eq!(p51_first.items.len(), 50);
        assert_eq!(p51_first.has_more, Some(true));
        assert_eq!(p51_first.next_cursor.as_deref(), Some("50"));

        let p51_second = slice_page((0..51).collect(), 50, 50);
        assert_eq!(p51_second.items, vec![50]);
        assert_eq!(p51_second.has_more, Some(false));
        assert_eq!(p51_second.next_cursor, None);

        let past_end = slice_page((0..10).collect(), 99, 50);
        assert!(past_end.items.is_empty());
        assert_eq!(past_end.has_more, Some(false));
        assert_eq!(past_end.next_cursor, None);
    }

    #[test]
    fn parse_offset_cursor_accepts_blank_and_rejects_text() {
        assert_eq!(parse_offset_cursor(None), Ok(0));
        assert_eq!(parse_offset_cursor(Some("   ")), Ok(0));
        assert_eq!(parse_offset_cursor(Some("100")), Ok(100));
        assert_eq!(
            parse_offset_cursor(Some("abc")),
            Err(OFFSET_CURSOR_ERROR.to_string())
        );
        assert_eq!(
            parse_offset_cursor(Some("-1")),
            Err(OFFSET_CURSOR_ERROR.to_string())
        );
    }

    #[test]
    fn compact_output_counts_and_unknowns() {
        let comments = (0..20)
            .map(|i| IssueComment {
                id: format!("c{i}"),
                ..Default::default()
            })
            .collect();
        let issue = IssueDetail {
            summary: IssueSummary {
                key: "ENG-42".into(),
                summary: "Rollout".into(),
                comment_count: 25,
                labels: vec!["ops".into(), "canary".into()],
                ..Default::default()
            },
            description_markdown: "Steps".into(),
            attachments: vec![AttachmentItem::default(), AttachmentItem::default()],
            comments,
            incomplete: vec!["linked_pages".into(), "comments".into()],
            ..Default::default()
        };

        let compact = issue.compact_output("h");
        let obj = compact.as_object().unwrap();
        assert_eq!(obj.len(), 6);
        assert_eq!(compact["hint"], "h");
        assert_eq!(compact["omitted"]["comments"], 25);
        assert_eq!(compact["omitted"]["attachments"], 2);
        assert_eq!(compact["omitted"]["labels"], 2);
        assert!(compact["omitted"]["linked_pages"].is_null());
        assert_eq!(
            compact["omitted"]["fields"],
            serde_json::json!(ISSUE_COMPACT_OMITTED_FIELDS)
        );
        let full = issue.output(false);
        assert_eq!(full["incomplete"], serde_json::json!(["linked_pages", "comments"]));

        let clean_issue = IssueDetail {
            incomplete: Vec::new(),
            ..issue
        };
        assert!(clean_issue.output(false).get("incomplete").is_none());

        let page = PageDetail {
            summary: PageSummary {
                id: "701".into(),
                title: "Plan".into(),
                space_id: "77".into(),
                space_key: "ENG".into(),
                version_number: 7,
                ..Default::default()
            },
            body_markdown: "# Plan".into(),
            ancestors: vec![PageAncestor {
                id: "700".into(),
                title: "Root".into(),
            }],
            toc: vec![TocEntry {
                level: 1,
                text: "Plan".into(),
                line_hint: 1,
            }],
            incomplete: vec!["labels".into()],
            ..Default::default()
        };

        let page_compact = page.compact_output("h");
        let pobj = page_compact.as_object().unwrap();
        assert_eq!(pobj.len(), 8);
        assert_eq!(page_compact["hint"], "h");
        assert!(page_compact["omitted"]["labels"].is_null());
        assert_eq!(page_compact["omitted"]["comments"], 0);
        assert_eq!(page_compact["omitted"]["ancestors"], 1);
        assert_eq!(page_compact["omitted"]["toc"], 1);
        assert_eq!(page_compact["omitted"]["linked_issue_keys"], 0);
        assert_eq!(
            page_compact["omitted"]["fields"],
            serde_json::json!(PAGE_COMPACT_OMITTED_FIELDS)
        );
        assert_eq!(page.output(false)["incomplete"], serde_json::json!(["labels"]));

        let clean_page = PageDetail {
            incomplete: Vec::new(),
            ..page
        };
        assert!(clean_page.output(false).get("incomplete").is_none());
    }

    #[test]
    fn compact_surfaces_custom_fields_and_projected_output_fetches_by_id_or_name() {
        let mut custom_fields = std::collections::BTreeMap::new();
        custom_fields.insert(
            "customfield_10042".to_string(),
            IssueCustomField {
                name: Some("Acceptance Criteria".to_string()),
                value: serde_json::json!("- Must return 200 OK\n- Must log audit event"),
                markdown: None,
            },
        );
        custom_fields.insert(
            "customfield_10099".to_string(),
            IssueCustomField {
                name: None,
                value: serde_json::json!("Tier-1"),
                markdown: None,
            },
        );

        let issue = IssueDetail {
            summary: IssueSummary {
                key: "ENG-99".into(),
                summary: "Ship AC projection".into(),
                assignee: Some(UserSummary {
                    id: "u1".into(),
                    display_name: "Elena Park".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            description_markdown: "Main body".into(),
            comments: vec![IssueComment {
                id: "c1".into(),
                body_markdown: "Looks good".into(),
                ..Default::default()
            }],
            incomplete: vec!["linked_pages".into()],
            custom_fields,
            ..Default::default()
        };

        let compact = issue.compact_output("get_jira_issue with fields:[...] or compact:false");
        assert_eq!(
            compact["omitted"]["custom_fields"],
            serde_json::json!([
                { "id": "customfield_10042", "name": "Acceptance Criteria" },
                { "id": "customfield_10099" }
            ])
        );

        // Fetch by custom field display name, standard shorthand, customfield ID, and unknown field.
        let projected = issue.projected_output(&[
            "Acceptance Criteria".to_string(),
            "assignee".to_string(),
            "customfield_10099".to_string(),
            "linked_pages".to_string(),
            "nonexistent_field".to_string(),
        ]);
        assert_eq!(projected["key"], "ENG-99");
        assert_eq!(
            projected["fields"]["customfield_10042"]["name"],
            "Acceptance Criteria"
        );
        assert_eq!(
            projected["fields"]["customfield_10042"]["value"],
            "- Must return 200 OK\n- Must log audit event"
        );
        assert_eq!(
            projected["fields"]["summary.assignee"]["display_name"],
            "Elena Park"
        );
        assert_eq!(projected["fields"]["customfield_10099"]["value"], "Tier-1");
        assert_eq!(
            projected["missing_fields"],
            serde_json::json!(["nonexistent_field"])
        );
        assert_eq!(projected["incomplete"], serde_json::json!(["linked_pages"]));
        // Compact/full backwards compatibility when deserializing legacy JSON without custom_fields
        let legacy: IssueDetail =
            serde_json::from_str(r#"{"summary":{"id":"","key":"OLD-1","project_key":"","summary":"","status":{"id":"","name":"","category":"todo"},"priority":null,"issue_type":{"id":"","name":"","subtask":false,"epic":false},"assignee":null,"reporter":null,"parent_key":null,"parent_summary":null,"labels":[],"story_points":null,"sprint_id":null,"sprint_name":null,"comment_count":0,"attachment_count":0,"created_at":"","updated_at":"","web_url":""},"description_raw":"","description_markdown":"","description_blocks":[],"fix_versions":[],"components":[],"subtasks":[],"linked_issues":[],"linked_pages":[],"attachments":[],"comments":[]}"#)
                .unwrap();
        assert!(legacy.custom_fields.is_empty());
    }
}
