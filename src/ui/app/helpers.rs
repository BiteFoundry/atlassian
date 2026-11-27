//! Pure helper functions for filters, Kanban column grouping, page-tree flattening, and site keys.

use std::collections::{HashMap, HashSet};

use regex::Regex;

use crate::{
    atlassian::AtlassianClient,
    models::{BoardSummary, IssueSummary, PageSummary, SavedFilter, StatusCategory},
};

use super::state::{KanbanColumn, TreeRow};

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

/// Replaces a tree row with a fresh detail summary, keeping a known children state: a row with
/// children stays expandable and a settled leaf stays a leaf when the detail does not say.
pub(super) fn keep_known_children_state(existing: &mut PageSummary, incoming: &PageSummary) {
    let prev = existing.has_children;
    *existing = incoming.clone();
    if prev == Some(true) || existing.has_children.is_none() {
        existing.has_children = prev;
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
            // A loaded, empty child list or `has_children == Some(false)` is a confirmed leaf.
            let expandable = match children {
                Some(kids) => !kids.is_empty(),
                None => page.has_children != Some(false),
            };
            let is_expanded = expanded.contains(&node);
            out.push(TreeRow {
                page: page.clone(),
                depth,
                expanded: is_expanded,
                has_children,
                expandable,
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

pub(super) fn scan_issue_keys(text: &str) -> Vec<String> {
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
pub(super) fn site_matches(row_site: Option<&str>, target: Option<&str>) -> bool {
    match (row_site, target) {
        (Some(row), Some(target)) => row.eq_ignore_ascii_case(target),
        _ => true,
    }
}


