//! MCP `initialize` metadata and JSON Schema tool catalog definitions.

use serde_json::{json, Value};

use super::{McpOptions, LATEST_PROTOCOL_VERSION, SERVER_NAME, SUPPORTED_PROTOCOL_VERSIONS};

pub(super) fn initialize_result(message: &Value, options: McpOptions) -> Value {
    let requested = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let instructions = if options.read_only {
        "Atlassian Jira & Confluence read-only tools supporting both Atlassian Cloud and Data Center. Use offline=true for local SQLite cache reads. Write, sync, and attachment download tools are disabled in --read-only mode."
    } else {
        "Atlassian Jira & Confluence tools supporting both Atlassian Cloud and Data Center. Use offline=true for local SQLite cache reads. Destructive operations (delete_jira_issue, delete_confluence_page) require confirm: true."
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": instructions
    })
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

pub(super) fn tool_catalog_for(options: McpOptions) -> Vec<Value> {
    let mut tools = tool_catalog();
    if options.read_only {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| options.allows_tool(name))
        });
    }
    tools
}

fn workflow_tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

/// Cost note for the catalog tools whose refresh re-downloads every page of the catalog.
fn catalog_refresh_cost(noun: &str) -> String {
    format!("refresh: true re-downloads the full catalog (one request per page of results; hundreds of {noun} means several requests and seconds of latency). Default reads the local cache with no network call; only an empty cache is fetched.")
}

pub(super) fn tool_catalog() -> Vec<Value> {
    let refresh_property = json!({
        "type": "boolean",
        "default": false,
        "description": "Re-download the full catalog, one request per page of results. Default false reads the local cache."
    });
    let text_replacement = json!({
        "type": "object",
        "description": "Replace one unique occurrence within a single source text node.",
        "properties": {
            "old_text": { "type": "string", "minLength": 1, "description": "Exact unique text snippet to replace within one text node." },
            "new_text": { "type": "string", "description": "Replacement text that substitutes for old_text." }
        },
        "required": ["old_text", "new_text"],
        "additionalProperties": false
    });
    let offline_property = json!({
        "type": "boolean",
        "default": false,
        "description": "Read from local SQLite cache only; default false calls the live API."
    });
    let mut tools = vec![
        // --- Jira ---
        json!({
            "name": "get_myself",
            "description": "Return the currently authenticated Atlassian user profile.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_projects",
            "description": format!("List accessible Jira projects from SQLite cache or refresh from Jira API. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("projects")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of projects to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_boards",
            "description": format!("List Jira Agile boards, optionally filtered by project key. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("boards")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Optional Jira project key (e.g. ENG)." },
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of boards to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_sprints",
            "description": "List sprints for a Jira Agile board. Returns active and future sprints only. Pass next_cursor as cursor to continue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer", "description": "Numeric Jira Agile board ID from list_boards." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of sprints to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." },
                    "offline": offline_property.clone()
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_board_issues",
            "description": "Fetch issues on a Jira Agile board (optionally filtered by sprint_id). A live call returns at most 100 issues; pass next_cursor as cursor to continue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_id": { "type": "integer", "description": "Numeric Jira Agile board ID from list_boards." },
                    "sprint_id": { "type": "integer", "description": "Optional numeric sprint ID from list_sprints to filter issues." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of issues to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." },
                    "offline": offline_property.clone()
                },
                "required": ["board_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_jira_issues",
            "description": "Search Jira-only issues using explicit JQL (jql), plain text (query), project filter, or offline SQLite cache. Use search_all for cross-product Jira + Confluence search; pass jql explicitly for custom fields (e.g. `customField = \"foo\"`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "jql": { "type": "string", "description": "Explicit raw JQL expression (e.g. `customField = \"value\" ORDER BY updated DESC`). Takes precedence over query." },
                    "query": { "type": "string", "description": "Plain text search query (or compatible standard-field JQL expression)." },
                    "project": { "type": "string", "description": "Optional Jira project key filter (e.g. ENG)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of issues to return (1–1000, default 50)." },
                    "page_token": { "type": "string", "description": "Cloud pagination cursor (`next_page_token`). Mutually exclusive with `start_at`." },
                    "start_at": { "type": "integer", "minimum": 0, "description": "Data Center 0-based pagination offset (`next_start_at`). Mutually exclusive with `page_token`." },
                    "offline": { "type": "boolean", "default": false, "description": "Search local SQLite cache only (plain query/project); default false calls Jira." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_jira_issue",
            "description": "Get the Jira issue key, summary, status, and full Markdown description. The omitted object lists skipped counts, standard fields, and populated custom fields (null means count unknown). Pass fields: [...] to fetch only specific omitted or custom fields by key (by field name, customfield_<id>, or custom field display name), or compact: false for the full ticket.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to fetch (e.g. ENG-42)." },
                    "offline": offline_property.clone(),
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Optional list of omitted standard fields, customfield_<id> keys, or custom field display names to project without loading the full ticket."
                    }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_jira_issue",
            "description": "Create a new Jira issue from a Markdown description and return the created issue key and summary (set compact: false for full details).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_key": { "type": "string", "minLength": 1, "description": "Target Jira project key (e.g. ENG)." },
                    "issue_type": { "type": "string", "default": "Task", "description": "Jira issue type name, such as Task, Bug, or Story (default Task)." },
                    "summary": { "type": "string", "minLength": 1, "description": "Concise single-line Jira issue title." },
                    "description_markdown": { "type": "string", "description": "Optional issue description formatted in Markdown." },
                    "priority": { "type": "string", "description": "Optional Jira priority name (e.g. High, Medium, Low)." },
                    "assignee_id": { "type": "string", "description": "Account ID or 'me'; a display name or email is resolved when the ID is rejected." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Optional list of Jira label strings to attach." },
                    "parent_key": { "type": "string", "description": "Optional parent or epic issue key (e.g. ENG-10)." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["project_key", "summary"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_jira_issue",
            "description": "Update standard Jira issue fields and Markdown description; use update_jira_custom_fields for customfield_<digits> keys. Choose one body operation per request; set dry_run: true to preview changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to update (e.g. ENG-42)." },
                    "summary": { "type": "string", "description": "New single-line title to replace the issue summary." },
                    "description_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "priority": { "type": "string", "description": "New Jira priority name (e.g. High, Medium, Low)." },
                    "assignee_id": { "type": ["string", "null"], "description": "Account ID or 'me'; a display name or email is resolved when the ID is rejected." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Replaces all labels; pass [] to clear." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_jira_transitions",
            "description": "List available workflow transitions for a Jira issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to inspect transitions for (e.g. ENG-42)." }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "transition_jira_issue",
            "description": "Transition a Jira issue to a new workflow status with an optional Markdown comment.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to transition (e.g. ENG-42)." },
                    "transition": { "type": "string", "minLength": 1, "description": "Transition ID or target status/transition name." },
                    "comment_markdown": { "type": "string", "description": "Optional Markdown comment posted alongside the workflow transition." }
                },
                "required": ["key", "transition"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "assign_jira_issue",
            "description": "Assign a Jira issue. assignee_id is required: an account id (Cloud) or username (Data Center), \"me\", or null to unassign.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to assign or unassign (e.g. ENG-42)." },
                    "assignee_id": { "type": ["string", "null"], "description": "Account ID (Cloud), username (Data Center), or 'me'; a display name or email is resolved when the ID is rejected. null unassigns." }
                },
                "required": ["key", "assignee_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_jira_comment",
            "description": "Add a Markdown comment to a Jira issue and return the created comment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to comment on (e.g. ENG-42)." },
                    "body_markdown": { "type": "string", "minLength": 1, "description": "Comment content formatted in Markdown." }
                },
                "required": ["key", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "upload_jira_attachment",
            "description": "Upload a local file as an attachment on a Jira issue and return the created attachment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to attach the file to (e.g. ENG-42)." },
                    "path": { "type": "string", "minLength": 1, "description": "Local filesystem path of the file to upload." }
                },
                "required": ["key", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_jira_issue",
            "description": "Permanently delete a Jira issue and remove it from the local cache. Destructive: requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Jira issue key to permanently delete (e.g. ENG-42)." },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["key", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("set_jira_parent",
            "Set the parent of an existing Jira Cloud issue. On Data Center, assign an eligible non-subtask issue to an epic.",
            json!({
                "key": {"type": "string", "description": "Child Jira issue key to reparent (e.g. ENG-42)."},
                "parent_key": {"type": "string", "description": "Target parent or epic issue key (e.g. ENG-10)."}
            }),
            &["key", "parent_key"]),
        workflow_tool("update_jira_custom_fields",
            "Set explicit customfield_<digits> values on a Jira issue and return the read-back values. Use update_jira_issue for standard fields or description edits.",
            json!({
                "key": {"type": "string", "description": "Jira issue key to update (e.g. ENG-42)."},
                "fields": {
                    "type": "object",
                    "minProperties": 1,
                    "propertyNames": {"pattern": "^customfield_[0-9]+$"},
                    "description": "Non-empty map of customfield_<digits> keys to native JSON values."
                }
            }),
            &["key", "fields"]),
        workflow_tool("create_jira_issue_link",
            "Link two existing issues with a configured Jira link type name and return the link ID.",
            json!({
                "inward_key": {"type": "string", "description": "Source issue key on inward end (for \"Blocks\", the blocking issue)."},
                "outward_key": {"type": "string", "description": "Target issue key on outward end (for \"Blocks\", the blocked issue)."},
                "link_type": {"type": "string", "description": "Configured Jira link type name (e.g. \"Blocks\", \"Relates\", \"Duplicate\")."}
            }),
            &["inward_key", "outward_key", "link_type"]),
        workflow_tool("move_jira_issues_to_sprint",
            "Move existing issues to an open or active sprint and confirm membership.",
            json!({
                "sprint_id": {"type": "integer", "minimum": 1, "description": "Numeric target open or active sprint ID from list_sprints."},
                "issue_keys": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 50,
                    "items": {"type": "string"},
                    "description": "Non-empty list of 1–50 Jira issue keys to move."
                }
            }),
            &["sprint_id", "issue_keys"]),
        workflow_tool("add_jira_worklog",
            "Log work in positive seconds and retain the remaining time estimate. Start time is RFC 3339 when supplied.",
            json!({
                "key": {"type": "string", "description": "Jira issue key to log work against (e.g. ENG-42)."},
                "seconds": {"type": "integer", "minimum": 1, "maximum": i32::MAX, "description": "Positive integer duration spent in seconds (e.g. 1800 for 30m)."},
                "started": {"type": "string", "format": "date-time", "description": "Optional worklog start timestamp in RFC 3339 format."},
                "comment_markdown": {"type": "string", "description": "Optional Markdown description of the logged work."}
            }),
            &["key", "seconds"]),
        // --- Confluence ---
        json!({
            "name": "list_confluence_spaces",
            "description": format!("List accessible Confluence spaces from cache or refresh from Confluence API. Returns limit items from the cache; pass next_cursor as cursor to continue, and refresh: true to reload the full catalog. {}", catalog_refresh_cost("spaces")),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": refresh_property.clone(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of spaces to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Offset from next_cursor." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_space_pages",
            "description": "List top-level root pages in a Confluence space (use list_child_pages for direct children of a page). A live call returns at most 250 pages; pass next_cursor as cursor to continue. has_children is null when unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_key": { "type": "string", "minLength": 1, "description": "Confluence space key from list_confluence_spaces (e.g. ENG)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of pages to return (1–1000, default 50)." },
                    "cursor": { "type": "string", "description": "Value from next_cursor. Valid only with the same arguments." },
                    "offline": offline_property.clone()
                },
                "required": ["space_key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_child_pages",
            "description": "List direct child pages of a Confluence page (use list_space_pages for top-level space roots). A live call returns at most 250 pages; pass next_cursor as cursor to continue. has_children is null when unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Parent Confluence page ID whose direct children to list." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100, "description": "Maximum number of child pages to return (1–1000, default 100)." },
                    "cursor": { "type": "string", "description": "Value from next_cursor. Valid only with the same arguments." },
                    "offline": offline_property.clone()
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_confluence_page",
            "description": "Get page identifiers, version, and full Markdown. Use compact: false for native body, blocks, TOC, comments, and attachments. The omitted object lists what was left out; null means the count is unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to fetch." },
                    "offline": offline_property.clone(),
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_confluence_page",
            "description": "Create a new Confluence page from Markdown and return the created page ID, version, and title (set compact: false for full details).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space_id_or_key": { "type": "string", "minLength": 1, "description": "Target Confluence space ID or space key (e.g. ENG)." },
                    "parent_id": { "type": "string", "description": "Optional parent Confluence page ID to nest under." },
                    "title": { "type": "string", "minLength": 1, "description": "Unique page title within the target Confluence space." },
                    "body_markdown": { "type": "string", "description": "Optional initial page body formatted in Markdown." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["space_id_or_key", "title"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_confluence_page",
            "description": "Update a Confluence page. Choose one body operation per request and pass its current prev_version. Set dry_run to preview the changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to update." },
                    "title": { "type": "string", "description": "Optional new title to rename the Confluence page." },
                    "body_markdown": { "type": "string", "description": "Replace the entire body." },
                    "append_markdown": { "type": "string", "description": "Append without converting the original body." },
                    "replace_text": text_replacement.clone(),
                    "replace_section": {
                        "type": "object",
                        "description": "Replace content under an exact top-level or layout-column heading on a Confluence Cloud ADF page.",
                        "properties": {
                            "heading": { "type": "string", "minLength": 1, "description": "Exact heading text identifying the section to replace." },
                            "markdown": { "type": "string", "description": "Replacement Markdown body for the matched section." }
                        },
                        "required": ["heading", "markdown"],
                        "additionalProperties": false
                    },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview the computed changes without saving." },
                    "prev_version": { "type": "integer", "minimum": 1, "description": "Current integer version_number from get_confluence_page (required for body edits)." },
                    "version_message": { "type": "string", "description": "Optional short summary recorded in page version history." },
                    "compact": { "type": "boolean", "default": true, "description": "Set false for the full response structure." }
                },
                "required": ["page_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "add_confluence_comment",
            "description": "Add a Markdown footer comment to a Confluence page and return the created comment metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to comment on." },
                    "body_markdown": { "type": "string", "minLength": 1, "description": "Footer comment content formatted in Markdown." }
                },
                "required": ["page_id", "body_markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_confluence_pages",
            "description": "Search Confluence-only pages locally (SQLite FTS5) or remotely via CQL. Use search_all for cross-product Jira + Confluence search; pass cql explicitly for custom expressions (e.g. `customMeta = \"value\"`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Plain text search query (or compatible standard-field CQL expression)." },
                    "cql": { "type": "string", "description": "Explicit raw CQL expression (e.g. `space = ENG AND title ~ \"Spec\"`). Takes precedence over query." },
                    "remote": { "type": "boolean", "default": false, "description": "Search live Confluence API via CQL; default false searches local cache." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30, "description": "Maximum number of pages to return (1–1000, default 30)." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_confluence_page",
            "description": "Delete a Confluence page and remove it from the local cache. Destructive: requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID to permanently delete." },
                    "confirm": { "const": true, "description": "Must be literal boolean true." }
                },
                "required": ["page_id", "confirm"],
                "additionalProperties": false
            }
        }),
        workflow_tool("upload_confluence_attachment",
            "Upload a local file as an attachment on a Confluence page and return the created attachment metadata.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to attach the file to."},
                "path": {"type": "string", "description": "Local filesystem path of the file to upload."}
            }),
            &["page_id", "path"]),
        workflow_tool("move_confluence_page",
            "Move a page under a parent. Data Center requires the current version and a parent in the same space.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to move."},
                "parent_id": {"type": "string", "description": "Destination parent Confluence page ID."},
                "prev_version": {"type": "integer", "minimum": 1, "description": "Current integer version_number from get_confluence_page (required on Data Center)."}
            }),
            &["page_id", "parent_id"]),
        workflow_tool("add_confluence_labels",
            "Add global labels to an existing page and keep its other labels.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to add labels to."},
                "labels": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": "string"},
                    "description": "Non-empty list of global label names to add."
                }
            }),
            &["page_id", "labels"]),
        workflow_tool("remove_confluence_label",
            "Remove one named label from an existing page.",
            json!({
                "page_id": {"type": "string", "description": "Confluence page ID to remove the label from."},
                "label": {"type": "string", "description": "Exact Confluence label name to remove."}
            }),
            &["page_id", "label"]),
        // --- Unified & Multi-Site ---
        json!({
            "name": "list_sites",
            "description": "List all registered Atlassian Cloud and Data Center sites with deployment, enabled state, and SQLite cache counts.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_all",
            "description": "Unified search across Jira issues and Confluence pages (local SQLite FTS5 or remote JQL/CQL). Prefer search_jira_issues for Jira-only JQL/pagination or search_confluence_pages for Confluence-only CQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "description": "Search query text (or JQL/CQL expression when remote is true)." },
                    "scope": { "type": "string", "enum": ["all", "jira", "confluence"], "default": "all", "description": "Products to search: all (default), jira, or confluence." },
                    "remote": { "type": "boolean", "default": false, "description": "Query live Jira/Confluence APIs; default false searches local SQLite cache." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 30, "description": "Maximum number of combined hits to return (1–1000, default 30)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "sync_workspace",
            "description": "Synchronize Jira projects, boards, sprints, recent issues, and Confluence spaces & pages into the local SQLite cache. Cost: walks every project, board, sprint, your open issues and every Confluence space (paginated) for each enabled site (or only the site named by site) — tens to hundreds of API requests and typically 10–120 s on large Data Center instances. Call once per session or when the cache is known stale; prefer per-item tools or refresh: true on a single list.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download an attachment from a Jira issue or Confluence page to disk and return its absolute path, metadata, and image flag for direct multimodal agent inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "minLength": 1, "description": "Jira issue key (e.g. ENG-42). Provide either issue_key or page_id." },
                    "page_id": { "type": "string", "minLength": 1, "description": "Confluence page ID. Provide either issue_key or page_id." },
                    "attachment_id": { "type": "string", "minLength": 1, "description": "Optional attachment ID or filename to download (defaults to first attachment)." },
                    "dest_dir": { "type": "string", "minLength": 1, "description": "Optional destination directory (defaults to configured download_dir)." }
                },
                "additionalProperties": false
            }
        }),
    ];

    // Every tool accepts the optional `site` selector; `list_sites` validates it but always
    // lists every site.
    for tool in &mut tools {
        let is_list_sites = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n == "list_sites");
        let description = if is_list_sites {
            "Accepted for a uniform call shape and checked against the configured sites; every site is listed regardless."
        } else {
            "Optional target Atlassian site ID, name, or URL."
        };
        if let Some(props) = tool.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut) {
            props.insert(
                "site".to_string(),
                json!({ "type": "string", "description": description }),
            );
        }
    }
    tools
}

