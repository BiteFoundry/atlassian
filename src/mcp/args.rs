//! MCP tool argument parser (`Args`) and upfront schema/range validation.

use std::{
    collections::{BTreeSet, HashMap},
    sync::LazyLock,
};

use serde_json::{Map, Value};

use crate::atlassian::body::{BodyEdit, SectionReplacement};

use super::{catalog::tool_catalog, BodyInputs, ASSIGNEE_REQUIRED};

/// Argument names a tool accepts: the properties of its own input schema in [tool_catalog],
/// the one place they are declared. None for a tool the catalog does not list.
pub(super) fn allowed_tool_keys(name: &str) -> Option<&'static BTreeSet<String>> {
    static ALLOWED: LazyLock<HashMap<String, BTreeSet<String>>> = LazyLock::new(|| {
        tool_catalog()
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let properties = tool.pointer("/inputSchema/properties")?.as_object()?;
                Some((name, properties.keys().cloned().collect()))
            })
            .collect()
    });
    ALLOWED.get(name)
}

/// Validates tool arguments before dispatching to Atlassian API or SQLite storage.
pub(crate) fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);
    let _ = args.optional_str("site")?;
    // Every schema says additionalProperties: false: reject an undeclared (often misspelled)
    // argument instead of dropping it silently.
    if let (Some(allowed), Some(given)) = (allowed_tool_keys(name), arguments.as_object()) {
        if let Some(key) = given.keys().find(|key| !allowed.contains(key.as_str())) {
            let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
            return Err(format!(
                "unknown argument '{key}' for {name}; allowed: {}",
                allowed.join(", ")
            ));
        }
    }

    match name {
        "list_sites" | "get_myself" | "sync_workspace" => Ok(()),
        "list_projects" => {
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_boards" => {
            let _ = args.optional_str("project")?;
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_sprints" => {
            let _ = args.required_i64("board_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_board_issues" => {
            let _ = args.required_i64("board_id")?;
            let _ = args.optional_i64("sprint_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "search_jira_issues" => {
            let _ = args.optional_str("jql")?;
            let _ = args.optional_str("query")?;
            let _ = args.optional_str("project")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let page_token = args.optional_str("page_token")?;
            let start_at = args.optional_nonneg_usize("start_at")?;
            if page_token.is_some() && start_at.is_some() {
                return Err("specify either 'page_token' or 'start_at', not both".to_string());
            }
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_jira_issue" | "list_jira_transitions" => {
            let _ = args.required_str("key")?;
            if name == "get_jira_issue" {
                args.validate_detail()?;
            }
            Ok(())
        }
        "create_jira_issue" => {
            let _ = args.required_str("project_key")?;
            let _ = args.required_str("summary")?;
            let _ = args.optional_str("issue_type")?;
            let _ = args.optional_text("description_markdown")?;
            let _ = args.optional_str("priority")?;
            let _ = args.optional_str("assignee_id")?;
            let _ = args.optional_str_array("labels")?;
            let _ = args.optional_str("parent_key")?;
            let _ = args.optional_bool("compact")?;
            Ok(())
        }
        "update_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.optional_bool("dry_run")?;
            let _ = args.optional_bool("compact")?;
            let summary = args.optional_str("summary")?;
            let (full, append, replacement) = args.body_inputs("description_markdown")?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                None,
            )
            .map_err(|e| e.to_string())?;
            let priority = args.optional_str("priority")?;
            let assignee = args.optional_nullable_str("assignee_id")?;
            let labels = args.optional_str_array_opt("labels")?;
            if summary.is_none()
                && edit.is_none()
                && priority.is_none()
                && assignee.is_none()
                && labels.is_none()
            {
                return Err("at least one Jira issue change is required".to_string());
            }
            Ok(())
        }
        "transition_jira_issue" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("transition")?;
            let _ = args.optional_text("comment_markdown")?;
            Ok(())
        }
        "assign_jira_issue" => {
            let _ = args.required_str("key")?;
            if args.optional_nullable_str("assignee_id")?.is_none() {
                return Err(ASSIGNEE_REQUIRED.to_string());
            }
            Ok(())
        }
        "add_jira_comment" => {
            let _ = args.required_str("key")?;
            let _ = args.required_text("body_markdown")?;
            Ok(())
        }
        "upload_jira_attachment" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("path")?;
            Ok(())
        }
        "delete_jira_issue" => {
            let _ = args.required_str("key")?;
            args.require_confirm_true()?;
            Ok(())
        }
        "list_confluence_spaces" => {
            let _ = args.optional_bool("refresh")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            Ok(())
        }
        "list_space_pages" => {
            let _ = args.required_str("space_key")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "get_confluence_page" => {
            let _ = args.required_str("page_id")?;
            args.validate_detail()
        }
        "list_child_pages" => {
            let _ = args.required_str("page_id")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            let _ = args.optional_str("cursor")?;
            let _ = args.optional_bool("offline")?;
            Ok(())
        }
        "create_confluence_page" => {
            let _ = args.required_str("space_id_or_key")?;
            let _ = args.required_str("title")?;
            let _ = args.optional_str("parent_id")?;
            let _ = args.optional_text("body_markdown")?;
            let _ = args.optional_bool("compact")?;
            Ok(())
        }
        "update_confluence_page" => {
            let _ = args.required_str("page_id")?;
            let _ = args.optional_bool("dry_run")?;
            let _ = args.optional_bool("compact")?;
            let title = args.optional_str("title")?;
            let (full, append, replacement) = args.body_inputs("body_markdown")?;
            let section = args.section_replacement()?;
            let edit = BodyEdit::from_inputs(
                full.as_deref(),
                append.as_deref(),
                replacement.as_ref(),
                section.as_ref(),
            )
            .map_err(|e| e.to_string())?;
            let version = args.page_version()?;
            let message = args.optional_str("version_message")?;
            if title.is_none() && edit.is_none() && message.is_none() {
                return Err("at least one Confluence page change is required".to_string());
            }
            if edit.is_some() && version.unwrap_or(0) <= 0 {
                return Err("prev_version (>= 1) is required for body edits. Read the current page and use its version_number.".to_string());
            }
            Ok(())
        }
        "add_confluence_comment" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_text("body_markdown")?;
            Ok(())
        }
        "search_confluence_pages" => {
            let q = args.optional_str("query")?;
            let cql = args.optional_str("cql")?;
            let _ = args.optional_bool("remote")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            if q.is_none() && cql.is_none() {
                return Err("either query or cql is required".to_string());
            }
            Ok(())
        }
        "delete_confluence_page" => {
            let _ = args.required_str("page_id")?;
            args.require_confirm_true()?;
            Ok(())
        }
        "search_all" => {
            let _ = args.required_str("query")?;
            if let Some(scope) = args.optional_str("scope")? {
                if !matches!(scope.to_ascii_lowercase().as_str(), "all" | "jira" | "confluence") {
                    return Err(format!(
                        "invalid scope '{scope}' (expected all, jira, or confluence)"
                    ));
                }
            }
            let _ = args.optional_bool("remote")?;
            if let Some(l) = args.optional_usize("limit")? {
                validate_range("limit", l, 1, 1_000)?;
            }
            Ok(())
        }
        "set_jira_parent" => {
            let _ = args.required_str("key")?;
            let _ = args.required_str("parent_key")?;
            Ok(())
        }
        "update_jira_custom_fields" => {
            let _ = args.required_str("key")?;
            let _ = args.custom_fields()?;
            Ok(())
        }
        "create_jira_issue_link" => {
            let _ = args.required_str("inward_key")?;
            let _ = args.required_str("outward_key")?;
            let _ = args.required_str("link_type")?;
            Ok(())
        }
        "move_jira_issues_to_sprint" => {
            let _ = args.required_i64("sprint_id")?;
            let _ = args.required_str_array("issue_keys")?;
            Ok(())
        }
        "add_jira_worklog" => {
            let _ = args.required_str("key")?;
            let _ = args.worklog_seconds()?;
            let _ = args.optional_str("started")?;
            let _ = args.optional_text("comment_markdown")?;
            Ok(())
        }
        "upload_confluence_attachment" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("path")?;
            Ok(())
        }
        "move_confluence_page" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("parent_id")?;
            let _ = args.page_version()?;
            Ok(())
        }
        "add_confluence_labels" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str_array("labels")?;
            Ok(())
        }
        "remove_confluence_label" => {
            let _ = args.required_str("page_id")?;
            let _ = args.required_str("label")?;
            Ok(())
        }
        "download_attachment" => {
            let issue_key = args.optional_str("issue_key")?;
            let page_id = args.optional_str("page_id")?;
            let _ = args.optional_str("attachment_id")?;
            let _ = args.optional_str("dest_dir")?;
            match (issue_key, page_id) {
                (None, None) => Err("either issue_key or page_id is required".to_string()),
                (Some(_), Some(_)) => {
                    Err("specify either issue_key or page_id, not both".to_string())
                }
                _ => Ok(()),
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}


pub(super) fn validate_range(field: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if value < min || value > max {
        Err(format!("{field} must be between {min} and {max}"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
thread_local! {
    /// Argument names read through [Args::get] while a test records them (Some).
    pub(super) static ARGS_READ: std::cell::RefCell<Option<BTreeSet<String>>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    pub(super) fn new(value: &'a Value) -> Self {
        Self { value }
    }

    pub(super) fn get(&self, key: &str) -> Result<Option<&'a Value>, String> {
        #[cfg(test)]
        ARGS_READ.with(|read| {
            if let Some(keys) = read.borrow_mut().as_mut() {
                keys.insert(key.to_string());
            }
        });
        let Some(obj) = self.value.as_object() else {
            return Err("arguments must be a JSON object".to_string());
        };
        Ok(obj.get(key))
    }

    pub(super) fn required_str(&self, key: &str) -> Result<String, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        let raw = val
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string"))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("'{key}' must not be empty"));
        }
        Ok(trimmed.to_string())
    }

    pub(super) fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("'{key}' must be a string")),
        }
    }

    pub(super) fn optional_nullable_str(&self, key: &str) -> Result<Option<Option<String>>, String> {
        match self.get(key)? {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(Some(None))
                } else {
                    Ok(Some(Some(trimmed.to_string())))
                }
            }
            Some(_) => Err(format!("'{key}' must be a string or null")),
        }
    }

    pub(super) fn required_text(&self, key: &str) -> Result<String, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        let raw = val
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string"))?;
        if raw.trim().is_empty() {
            return Err(format!("'{key}' must not be empty"));
        }
        Ok(raw.to_string())
    }

    pub(super) fn optional_text(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("'{key}' must be a string")),
        }
    }

    fn validate_detail(&self) -> Result<(), String> {
        let _ = self.optional_bool("offline")?;
        let _ = self.optional_bool("compact")?;
        Ok(())
    }

    pub(super) fn body_inputs(&self, full: &str) -> Result<BodyInputs, String> {
        let replacement = self
            .get("replace_text")?
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|e| format!("invalid replace_text: {e}"))
            })
            .transpose()?;
        Ok((
            self.optional_text(full)?,
            self.optional_text("append_markdown")?,
            replacement,
        ))
    }

    pub(super) fn section_replacement(&self) -> Result<Option<SectionReplacement>, String> {
        self.get("replace_section")?
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("invalid replace_section: {error}"))
            })
            .transpose()
    }

    pub(super) fn page_version(&self) -> Result<Option<i32>, String> {
        self.optional_i64("prev_version")?
            .map(|version| {
                i32::try_from(version)
                    .map_err(|_| "prev_version is outside the supported version range".to_string())
            })
            .transpose()
    }

    pub(super) fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("'{key}' must be a boolean")),
        }
    }

    pub(super) fn required_i64(&self, key: &str) -> Result<i64, String> {
        let val = self
            .get(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))?;
        val.as_i64()
            .ok_or_else(|| format!("'{key}' must be an integer"))
    }

    pub(super) fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .map(Some)
                .ok_or_else(|| format!("'{key}' must be an integer")),
        }
    }

    pub(super) fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("'{key}' must be a positive integer"))?;
                usize::try_from(n)
                    .map(Some)
                    .map_err(|_| format!("'{key}' is too large"))
            }
        }
    }

    pub(super) fn optional_nonneg_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("'{key}' must be a non-negative integer"))?;
                usize::try_from(n)
                    .map(Some)
                    .map_err(|_| format!("'{key}' is too large"))
            }
        }
    }

    pub(super) fn optional_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        Ok(self.optional_str_array_opt(key)?.unwrap_or_default())
    }

    pub(super) fn optional_str_array_opt(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.get(key)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let s = item
                        .as_str()
                        .ok_or_else(|| format!("'{key}' items must be strings"))?;
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("'{key}' must be an array of strings")),
        }
    }

    pub(super) fn custom_fields(&self) -> Result<&'a Map<String, Value>, String> {
        self.get("fields")?
            .and_then(Value::as_object)
            .filter(|fields| !fields.is_empty())
            .ok_or_else(|| {
                "fields must be a nonempty JSON object keyed by custom field ID".to_string()
            })
    }

    pub(super) fn worklog_seconds(&self) -> Result<i32, String> {
        i32::try_from(self.required_i64("seconds")?)
            .ok()
            .filter(|&seconds| seconds > 0)
            .ok_or_else(|| "seconds must be a positive 32-bit integer".to_string())
    }

    pub(super) fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .get(key)?
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| format!("'{key}' must be a nonempty array of strings"))?;
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("'{key}' items must be nonempty strings"))
            })
            .collect()
    }

    pub(super) fn require_confirm_true(&self) -> Result<(), String> {
        match self.get("confirm")? {
            Some(Value::Bool(true)) => Ok(()),
            _ => Err("destructive operation requires confirm: true".to_string()),
        }
    }
}

