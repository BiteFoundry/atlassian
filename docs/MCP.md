# `jira` / `confluence` MCP Server

`jira mcp` or `confluence mcp` (alias `mcp-server`) runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over **stdio** using newline-delimited JSON-RPC 2.0. It exposes **26 typed tools** covering Atlassian Jira, Jira Agile, Confluence, and unified search/sync to MCP-compatible clients such as Claude Code, Claude Desktop, and Codex.

## Requirements

1. Build or install the `jira` binary (and optional `confluence` / `conf` copy/symlink for persona auto-detection):
   ```bash
   cargo build --release
   ```
2. Authenticate once (the MCP server never prompts interactively on stdin):
   ```bash
   jira login --url https://acme.atlassian.net --email you@acme.com
   # or for Data Center with PAT:
   jira login --url https://jira.company.com --deployment datacenter --pat <PAT>
   ```
3. Ensure `jira` (e.g. `target/release/jira.exe` on Windows or on `PATH`) is accessible to your MCP host.

## Starting manually

```bash
jira mcp            # stdio: one JSON-RPC 2.0 object per line
confluence mcp      # identical 26-tool MCP server under the confluence persona
jira mcp-server     # alias
```

`stdout` strictly carries single-line JSON-RPC 2.0 messages; diagnostics and startup warnings are written to `stderr` only.

## Client configuration

### Claude Code

```bash
claude mcp add atlassian -- jira mcp
# or using the confluence persona / explicit Windows binary path:
claude mcp add atlassian -- C:\Users\me\source\atlassian\target\release\jira.exe mcp
```

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "atlassian": {
      "command": "jira",
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
      "command": "C:\\Users\\me\\source\\atlassian\\target\\release\\jira.exe",
      "args": ["mcp"]
    }
  }
}
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.atlassian]
command = "jira"
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
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with an error message on validation or API failures.

## Tools (26)

### Jira Tools (15)

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_myself` | _(none)_ | `{ "user": {...} }` |
| `list_projects` | `refresh?` (bool) | `{ "projects": [...] }` |
| `list_boards` | `project?` (string), `refresh?` (bool) | `{ "boards": [...] }` |
| `list_sprints` | `board_id` (int), `offline?` (bool) | `{ "board_id": ..., "sprints": [...] }` |
| `get_board_issues` | `board_id` (int), `sprint_id?` (int), `limit?` (1..1000), `offline?` (bool) | `{ "board_id": ..., "sprint_id": ..., "issues": [...] }` |
| `search_jira_issues` | `jql?`, `query?`, `project?`, `limit?` (1..1000), `page_token?`, `offline?` (bool) | `{ "jql": "...", "issues": [...], ... }` or offline `{ "offline": true, "hits"\|"issues": [...] }` |
| `get_jira_issue` | `key` (string), `offline?` (bool) | `{ "issue": {...} }` |
| `create_jira_issue` | `project_key`, `summary`, `issue_type?`, `description_markdown?`, `priority?`, `assignee_id?`, `labels?`, `parent_key?` | `{ "ok": true, "issue": {...} }` |
| `update_jira_issue` | `key`, `summary?`, `description_markdown?`, `priority?`, `assignee_id?` (`string\|null`), `labels?` | `{ "ok": true, "issue": {...} }` |
| `list_jira_transitions` | `key` (string) | `{ "key": "...", "transitions": [...] }` |
| `transition_jira_issue` | `key`, `transition`, `comment_markdown?` | `{ "ok": true, "key": "...", "transition": "..." }` |
| `assign_jira_issue` | `key`, `assignee_id?` (`string\|null`, `"me"` resolves current user) | `{ "ok": true, "key": "...", "assignee_id": ... }` |
| `add_jira_comment` | `key`, `body_markdown` | `{ "ok": true, "comment": {...} }` |
| `upload_jira_attachment` | `key`, `path` (local file path) | `{ "ok": true, "key": "...", "attachments": [...] }` |
| `delete_jira_issue` | `key`, `confirm` (must be literal `true`) | `{ "ok": true, "key": "..." }` |

### Confluence Tools (9)

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_confluence_spaces` | `refresh?` (bool), `limit?` (1..1000) | `{ "spaces": [...] }` |
| `list_space_pages` | `space_key`, `limit?` (1..1000), `offline?` (bool) | `{ "space_key": "...", "pages": [...] }` |
| `list_child_pages` | `page_id`, `offline?` (bool) | `{ "parent_id": "...", "pages": [...] }` |
| `get_confluence_page` | `page_id`, `offline?` (bool) | `{ "page": {...} }` |
| `create_confluence_page` | `space_id_or_key`, `title`, `body_markdown?`, `parent_id?` | `{ "ok": true, "page": {...} }` |
| `update_confluence_page` | `page_id`, `title?`, `body_markdown?`, `prev_version?`, `version_message?` | `{ "ok": true, "page": {...} }` |
| `add_confluence_comment` | `page_id`, `body_markdown` | `{ "ok": true, "comment": {...} }` |
| `search_confluence_pages` | `query?`, `cql?`, `remote?` (bool), `limit?` (1..1000) | `{ "remote": ..., "hits"\|"pages": [...] }` |
| `delete_confluence_page` | `page_id`, `confirm` (must be literal `true`) | `{ "ok": true, "page_id": "..." }` |

### Unified Tools (2)

| Tool | Arguments | Returns |
| --- | --- | --- |
| `search_all` | `query`, `scope?` (`all\|jira\|confluence`), `remote?` (bool), `limit?` (1..1000) | `{ "query": "...", "scope": "...", "remote": ..., "hits": [...] }` |
| `sync_workspace` | _(none)_ | `{ "ok": true, "summary": "...", "counts": {...} }` |

## Safety guards

- Destructive tools (`delete_jira_issue` and `delete_confluence_page`) enforce `confirm: true` as a literal JSON boolean (`Value::Bool(true)`). Omitting `confirm` or passing `false`, `"true"`, `1`, or `null` returns `isError: true`.
- `update_jira_issue` and `update_confluence_page` reject empty patches with no fields to update.
- `update_confluence_page` automatically fetches the current page when `prev_version` is omitted so version-conflict checks (`prev_version + 1`) stay safe.
