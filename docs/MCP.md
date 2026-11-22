# `atlassian` / `confluence` MCP Server

`atlassian mcp` or `confluence mcp` (alias `mcp-server`) runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over **stdio** using newline-delimited JSON-RPC 2.0. Call `tools/list` to discover the active tool schemas. The server covers Atlassian Jira, Jira Agile, Confluence, and unified search/sync to MCP-compatible clients such as Claude Code, Claude Desktop, and Codex.

## Requirements

1. Build or install the `atlassian` binary (and optional `confluence` / `conf` copy/symlink for persona auto-detection):
   ```bash
   cargo build --release
   ```
2. Authenticate once (the MCP server never prompts interactively on stdin):
   ```bash
   atlassian login --url https://acme.atlassian.net --email you@acme.com
   # or for Data Center with PAT:
   atlassian login --url https://jira.company.com --deployment datacenter --pat <PAT>
   ```
3. Ensure `atlassian` (e.g. `target/release/atlassian.exe` on Windows or on `PATH`) is accessible to your MCP host.

## Starting manually

```bash
atlassian mcp              # stdio: one JSON-RPC 2.0 object per line (all 37 tools)
atlassian mcp --read-only  # read-only session (15 query tools; write, sync, and download calls rejected)
confluence mcp             # same tool catalog under the confluence persona
confluence mcp --read-only # read-only session under the confluence persona
atlassian mcp-server       # alias
```

Passing `--read-only` exposes only the 15 read-only query tools (`get_myself`, `list_sites`, `list_projects`, `list_boards`, `list_sprints`, `get_board_issues`, `search_jira_issues`, `get_jira_issue`, `list_jira_transitions`, `list_confluence_spaces`, `list_space_pages`, `list_child_pages`, `get_confluence_page`, `search_confluence_pages`, and `search_all`) in `tools/list` and rejects direct `tools/call` invocations of any other tool before remote writes, workspace syncs, or attachment downloads occur.

`stdout` strictly carries single-line JSON-RPC 2.0 messages; diagnostics and startup warnings are written to `stderr` only.

## Client configuration

When upgrading from 0.3.x, change `{"command":"jira","args":["mcp"]}` to `{"command":"atlassian","args":["mcp"]}`. For an absolute executable path on Windows, replace `jira.exe` with `atlassian.exe`.

### Claude Code

```bash
claude mcp add atlassian -- atlassian mcp
# or using the confluence persona / explicit Windows binary path:
claude mcp add atlassian -- C:\Users\me\source\atlassian\target\release\atlassian.exe mcp
```

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "atlassian": {
      "command": "atlassian",
      "args": ["mcp"]
    }
  }
}
```

On Windows with an explicit executable path (or `confluence` persona):

```json
{
  "mcpServers": {
    "atlassian": {
      "command": "C:\\Users\\me\\source\\atlassian\\target\\release\\atlassian.exe",
      "args": ["mcp"]
    }
  }
}
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.atlassian]
command = "atlassian"
args = ["mcp"]
```

Or with the `confluence` persona binary:

```toml
[mcp_servers.atlassian]
command = "confluence"
args = ["mcp"]
```

## Protocol

- **Transport**: JSON-RPC 2.0 over stdio, **newline-delimited JSON** (one JSON object per line).
- **Supported `protocolVersion`s**: `2024-11-05`, `2025-03-26`, `2025-06-18` (unknown versions fall back to `2025-06-18`).
- **`serverInfo`**: `{ "name": "atlassian", "version": "<crate version>" }`.
- **Notifications**: requests without `id` or methods starting with `notifications/` (such as `notifications/initialized`) emit no response.
- **`ping`**: returns `{}`.
- **Error codes**: `-32700` parse error, `-32600` invalid JSON-RPC envelope, `-32601` method not found.
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with structured JSON `{ "ok": false, "error": { "code", "message", "http_status"?, "details"? } }` on validation or API failures.

## Tools

### Jira Tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_myself` | _(none)_ | `{ "user": {...} }` |
| `list_projects` | `refresh?` (bool), `limit?` (1..1000, default 50), `cursor?` (string) | `{ "projects": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `list_boards` | `project?` (string), `refresh?` (bool), `limit?` (1..1000, default 50), `cursor?` (string) | `{ "boards": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `list_sprints` | `board_id` (int), `limit?` (1..1000, default 50), `cursor?` (string), `offline?` (bool) | `{ "board_id": ..., "sprints": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `get_board_issues` | `board_id` (int), `sprint_id?` (int), `limit?` (1..1000, default 50), `cursor?` (string), `offline?` (bool) | `{ "board_id": ..., "sprint_id": ..., "issues": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `search_jira_issues` | `jql?`, `query?`, `project?`, `limit?` (1..1000), `page_token?` (Cloud), `start_at?` (Data Center, >= 0), `offline?` (bool) | `{ "jql": "...", "issues": [...], "next_page_token": ..., "next_start_at": ..., "total": ... }` or offline `{ "offline": true, "hits"\|"issues": [...] }` |
| `get_jira_issue` | `key` (string), `offline?` (bool), `compact?` (bool) | `{ "issue": {...} }` |
| `create_jira_issue` | `project_key`, `summary`, `issue_type?`, `description_markdown?`, `priority?`, `assignee_id?` (`"me"`, account ID, or display name/email), `labels?`, `parent_key?`, `compact?` (bool, default true) | `{ "ok": true, "site_id": "...", "issue": { "key", "summary", "status", "web_url" } }` (or full detail when `compact: false`) |
| `update_jira_issue` | `key`, `summary?`, `description_markdown?`, `append_markdown?`, `replace_text?`, `priority?`, `assignee_id?` (`string\|null`, `"me"` or name/email), `labels?` (replaces all labels; `[]` clears), `dry_run?` (bool), `compact?` (bool, default true) | Compact updated `issue` (or full detail when `compact: false`) or a `preview` |
| `list_jira_transitions` | `key` (string) | `{ "key": "...", "transitions": [...] }` |
| `transition_jira_issue` | `key`, `transition`, `comment_markdown?` | `{ "ok": true, "site_id": "...", "key": "...", "transition": "...", "transition_id": "...", "to_status"?: "..." }` |
| `assign_jira_issue` | `key`, `assignee_id?` (`string\|null`, `"me"`, account ID, or display name/email) | `{ "ok": true, "site_id": "...", "key": "...", "assignee_id": ..., "resolved_assignee"?: {...} }` |
| `add_jira_comment` | `key`, `body_markdown` | `{ "ok": true, "comment": {...} }` |
| `upload_jira_attachment` | `key`, `path` (local file path) | `{ "ok": true, "key": "...", "attachments": [...] }` |
| `delete_jira_issue` | `key`, `confirm` (must be literal `true`) | `{ "ok": true, "key": "..." }` |
| `set_jira_parent` | `key`, `parent_key` | Confirmed `parent_key` (epic key on Data Center) |
| `update_jira_custom_fields` | `key`, `fields` (JSON object keyed by `customfield_<digits>`) | Values read back in `custom_fields` |
| `create_jira_issue_link` | `inward_key`, `outward_key`, `link_type` (configured name) | `link` with ID and ends |
| `move_jira_issues_to_sprint` | `sprint_id`, `issue_keys` (1–50 distinct keys) | `sprint_id` and confirmed `issue_keys` |
| `add_jira_worklog` | `key`, `seconds`, `started?` (RFC 3339), `comment_markdown?` | Persisted `worklog` with ID and seconds |

### Confluence Tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_confluence_spaces` | `refresh?` (bool), `limit?` (1..1000, default 50), `cursor?` (string) | `{ "spaces": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `list_space_pages` | `space_key`, `limit?` (1..1000, default 50), `cursor?` (string), `offline?` (bool) | `{ "space_key": "...", "pages": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `list_child_pages` | `page_id`, `limit?` (1..1000, default 100), `cursor?` (string), `offline?` (bool) | `{ "parent_id": "...", "pages": [...], "returned": ..., "has_more": ..., "next_cursor": ... }` |
| `get_confluence_page` | `page_id`, `offline?` (bool), `compact?` (bool) | `{ "page": {...} }` |
| `create_confluence_page` | `space_id_or_key`, `title`, `body_markdown?`, `parent_id?`, `compact?` (bool, default true) | `{ "ok": true, "site_id": "...", "page": { "id", "title", "space_id", "space_key", "version_number", "web_url" } }` (or full detail when `compact: false`) |
| `update_confluence_page` | `page_id`, `title?`, `body_markdown?`, `append_markdown?`, `replace_text?`, `replace_section?`, `prev_version?`, `version_message?`, `dry_run?` (bool), `compact?` (bool, default true) | Compact updated `page` (or full detail when `compact: false`) or a `preview` |
| `add_confluence_comment` | `page_id`, `body_markdown` | `{ "ok": true, "comment": {...} }` |
| `search_confluence_pages` | `query?`, `cql?`, `remote?` (bool), `limit?` (1..1000) | `{ "remote": ..., "hits"\|"pages": [...] }` |

> **Search queries & pagination**: Plain-text `query` values (including phrases containing `and`, `or`, `=`, or `~` such as `read and write` or `timeout=30`) are escaped into `text ~ "..."` (with `project = "..." AND ...` when `project` is supplied). Standard leading field expressions (e.g. `project = FLS`, `space = ENG`, `status IN (...)`, `cf[10016] >= 5`, or `ORDER BY ...`) are recognized automatically; pass explicit `jql` (e.g. `{"jql": "customField = \"value\""}`) or `cql` (e.g. `{"cql": "customMeta = \"value\""}`) for custom raw expressions. For Jira pagination, use `page_token` on Cloud and `start_at` (from `next_start_at`) on Data Center; passing both cursors together is rejected.

> **Collection paging**: `list_projects`, `list_boards`, `list_confluence_spaces`, `list_sprints`, `get_board_issues`, `list_space_pages`, and `list_child_pages` include `returned`, `has_more` (`true`, `false`, or `null` when upstream gives no completion signal), and `next_cursor` (`string` or `null`). Pass `next_cursor` back as `cursor` with the same arguments to continue. Catalog tools (`list_projects`, `list_boards`, `list_confluence_spaces`) follow all upstream pages on refresh and page locally from SQLite by integer offset; when reading from a cache populated by sync, pass `refresh: true` to load the full catalog. Single-page remote tools fetch one upstream page per call, capped by upstream per-call limits (`get_board_issues`: 100, `list_space_pages` / `list_child_pages`: 250, `list_sprints`: 50). `list_sprints` returns `active` and `future` sprints only. On Confluence page summaries, `has_children` is `true`, `false`, or `null` when unknown from cache without extra requests.
| `delete_confluence_page` | `page_id`, `confirm` (must be literal `true`) | `{ "ok": true, "page_id": "..." }` |
| `upload_confluence_attachment` | `page_id`, `path` (local file path) | `attachments` metadata |
| `move_confluence_page` | `page_id`, `parent_id`, `prev_version?` (required on Data Center) | `page` summary with confirmed parent and version |
| `add_confluence_labels` | `page_id`, `labels` (nonempty array) | `added` names |
| `remove_confluence_label` | `page_id`, `label` | `removed` name |

### Unified Tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `search_all` | `query`, `scope?` (`all\|jira\|confluence`), `remote?` (bool), `limit?` (1..1000) | `{ "query": "...", "scope": "...", "remote": ..., "hits": [...] }` |
| `sync_workspace` | _(none)_ | `{ "ok": true, "summary": "...", "counts": {...} }` |
| `list_sites` | _(none)_ | Configured sites and the default site |
| `download_attachment` | One of `issue_key` or `page_id`, plus `attachment_id?` and `dest_dir?` | Local path and attachment metadata |

## Read and edit bodies

- `get_jira_issue` and `get_confluence_page` default to `compact: true` for live and offline reads. Both return full Markdown, an `omitted` object summarizing left-out collections and field paths (`null` when a subresource failed or was truncated), and a `hint` showing how to request the full response. An issue includes its key, summary, and status, and a page includes its ID, title, space, and version. Set `compact: false` for the full response structure; when any subresource failed or was truncated, the full response includes an `incomplete` array listing those collections.
- Pass one of the body fields when editing. Use `description_markdown` for a full Jira description or `body_markdown` for a full Confluence body. `append_markdown` adds a fragment to the native body. `replace_text` accepts an object with `old_text` and `new_text` and requires one match in a single source text node.
- Pass the current `page.version_number` from a compact read or `page.summary.version_number` from a full read as `prev_version` for any Confluence body edit.
- Use `replace_section: {"heading":"Rollout","markdown":"Verify the canary."}` on a Confluence Cloud ADF page. The heading text needs an exact, unique match among top-level headings and direct headings inside layout columns. The edit keeps that heading and replaces the content up to the next heading at the same or a higher level in the same container (or the end of that container). An empty Markdown string removes that content.
- Set `dry_run: true` on either update tool to compute the pending edit against the live item. The response includes `preview.changed` and `preview.changes`. Confluence also returns the current and proposed version numbers. Body changes include the native `representation`, `before`, and `after`. When present, `start` is a zero-based top-level ADF node index or UTF-8 byte offset into a wiki or storage string. The preview performs zero remote writes and leaves the detail cache untouched.

Preview a section edit on page `40211` at version 7 with `update_confluence_page`.

```json
{
  "page_id": "40211",
  "prev_version": 7,
  "replace_section": {
    "heading": "Rollout",
    "markdown": "Verify the canary."
  },
  "dry_run": true
}
```

## Incremental Jira and Confluence operations

- `set_jira_parent` sets the existing issue's `parent` field in Cloud. In Data Center it assigns a non-subtask issue to an epic through Jira Software. For other Data Center hierarchy levels, use the configured Parent Link field ID where it is editable. Existing Data Center subtask parents can be changed in the Jira UI. [Cloud edit issue](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issues/#api-rest-api-3-issue-issueidorkey-put) · [Data Center epic API](https://developer.atlassian.com/server/jira/platform/rest/v11002/api-group-epic/) · [Data Center subtask REST request](https://jira.atlassian.com/browse/JRASERVER-68763)
- Pass native JSON values to `update_jira_custom_fields`, for example `{"key":"ENG-101","fields":{"customfield_10100":3,"customfield_10101":{"id":"10400"},"customfield_10102":null}}`. Jira determines which fields and shapes are accepted. A Cloud multiline text field expects ADF. The response reports the provider's read-back values, which may expand objects such as options. [Cloud edit issue](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issues/#api-rest-api-3-issue-issueidorkey-put) · [Data Center field updates](https://developer.atlassian.com/server/jira/platform/updating-an-issue-via-the-jira-rest-apis-6848604/)
- `create_jira_issue_link` accepts the configured type name, such as `Blocks`, and the named inward and outward ends. `move_jira_issues_to_sprint` requires Jira Software and an open or active sprint. Cloud OAuth credentials need `write:sprint:jira-software` and `read:issue:jira-software`. Use a Cloud API token for this workflow when using the app's default OAuth scope configuration. [Cloud issue links](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issue-links/) · [Data Center issue links](https://developer.atlassian.com/server/jira/platform/rest/v10006/api-group-issuelink/) · [Cloud sprint API](https://developer.atlassian.com/cloud/jira/software/rest/api-group-sprint/) · [Cloud Agile issue API](https://developer.atlassian.com/cloud/jira/software/rest/api-group-issue/) · [Data Center sprint API](https://developer.atlassian.com/server/jira/platform/rest/v10007/api-group-sprint/)
- `add_jira_worklog` requires time tracking and permission to log work. It keeps the remaining estimate. After a transport or read-back failure, inspect existing worklogs before sending another creation request. [Cloud worklogs](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issue-worklogs/) · [Data Center worklogs](https://docs.atlassian.com/software/jira/docs/api/REST/9.10.2/#api/2/issue-addWorklog)
- `move_confluence_page` uses the Cloud move-to-child operation with an empty request body. An optional Cloud version is checked before the move. Data Center requires the current `prev_version` and a parent in the same space. Its metadata update sends the next version and `ancestors` without a body. [Cloud page move](https://developer.atlassian.com/cloud/confluence/rest/v1/api-group-content---children-and-descendants/) · [Data Center update content](https://developer.atlassian.com/server/confluence/rest/v1001/api-group-content-resource/)
- Labels use the dedicated add and remove endpoints. Add accepts multiple global labels in one request. Remove accepts one label name. Attachment upload uses the existing create-only multipart path shared with `conf attach`. [Cloud labels](https://developer.atlassian.com/cloud/confluence/rest/v1/api-group-content-labels/) · [Data Center labels](https://developer.atlassian.com/server/confluence/rest/v9227/api-group-content-labels/) · [Cloud attachments](https://developer.atlassian.com/cloud/confluence/rest/v1/api-group-content---attachments/) · [Data Center attachments](https://developer.atlassian.com/server/confluence/rest/latest/)

## Safety guards

- Destructive tools (`delete_jira_issue` and `delete_confluence_page`) enforce `confirm: true` as a literal JSON boolean (`Value::Bool(true)`). Omitting `confirm` or passing `false`, `"true"`, `1`, or `null` returns `isError: true`.
- `update_jira_issue` and `update_confluence_page` reject empty patches with no fields to update.
- Confluence reports a version conflict if the page has changed since the supplied base version.
