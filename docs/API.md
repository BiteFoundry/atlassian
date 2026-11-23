# `jira` / `confluence` HTTP JSON API

Local HTTP/JSON server for AI agents, scripts, and integrations. Exposed by `atlassian serve` or `confluence serve` (alias `api`), backed by Atlassian REST APIs (Cloud & Data Center), local SQLite WAL/FTS5 caching, and the background sync engine (`sync::spawn_sync_engine`).

Both binary personas (`jira` and `confluence` / `conf`) mount the complete unified Jira, Jira Agile, Confluence, and cross-product search route surface on the same HTTP server.

## Starting the server

```bash
atlassian serve                      # http://127.0.0.1:8789
confluence serve                # same API server under the confluence persona
atlassian serve --port 9000          # custom port
atlassian serve --bind 127.0.0.1 --port 8789
atlassian serve --token secret123    # require Authorization: Bearer secret123
atlassian serve --cors-origin http://localhost:5173   # let one browser origin call the API
atlassian api                        # alias for `serve`
```

- `--bind` defaults to `127.0.0.1` (loopback only). Binding any other address (for example `0.0.0.0`) requires `--token`; without it `serve` refuses to start.
- `--port` defaults to `8789`.
- `--token` is optional on a loopback bind; when set, every endpoint except `GET /health` requires `Authorization: Bearer <token>`.
- `--cors-origin <ORIGIN>` (repeatable) allows one exact browser origin such as `http://localhost:5173`. The default is none, and `*` is rejected.
- Requires an existing login (`atlassian login` or `confluence login`) for live/remote endpoints; `?offline=true` routes and local `/search` read directly from SQLite.
- On startup, `serve` spawns the background sync engine and requests `SyncCommand::RefreshAll`.

## Conventions

- All responses are JSON (`Content-Type: application/json`).
- Errors use `{"error": "<message>"}` with HTTP `400`, `401`, `403`, `404`, or `500`.
- `limit` defaults to `50` (`30` for `/search`) and is clamped to `1..=1000`. Live board issue queries return at most `100` items per call and live space root page queries return at most `250` items per call.
- Browser access is closed by default. A request with an `Origin` header gets `403` unless that origin was passed with `--cors-origin` (or is the server's own origin); this includes requests that never send a CORS preflight, such as a plain `GET` or a form `POST`. Allowed origins receive CORS headers for `GET`, `POST`, `PUT`, `PATCH`, `DELETE` with `Authorization` and `Content-Type`.
- On a loopback bind the `Host` header must be `127.0.0.1`, `localhost`, or `[::1]` (any port); other names get `403`. This stops DNS rebinding from a web page.
- Requests without an `Origin` header (curl, scripts, agents) are not affected by either check.

---

## Health & Identity

### `GET /health`

Always public, even when `--token` is configured.

```bash
curl http://127.0.0.1:8789/health
```

```json
{
  "ok": true,
  "version": "0.1.0",
  "deployment": "cloud",
  "site_url": "https://acme.atlassian.net",
  "confluence_url": "https://acme.atlassian.net/wiki"
}
```

### `GET /myself`

Returns the currently authenticated Atlassian user profile.

```bash
curl http://127.0.0.1:8789/myself \
  -H "Authorization: Bearer secret123"
```

```json
{
  "user": {
    "id": "557058:...",
    "display_name": "Alice Kim",
    "email": "alice@acme.com",
    "avatar_url": "https://...",
    "active": true
  }
}
```

---

## Jira Projects, Boards & Sprints

### `GET /projects?refresh=false`

Lists Jira projects from local SQLite cache, or refreshes from Jira REST API when `refresh=true` or cache is empty.

```json
{
  "projects": [
    {
      "id": "10000",
      "key": "ENG",
      "name": "Engineering",
      "project_type": "software",
      "lead_name": "Alice Kim",
      "avatar_url": null,
      "pinned": false
    }
  ]
}
```

### `GET /boards?project=&refresh=false`

Lists Jira Agile boards, optionally filtered by `project` key.

```json
{
  "boards": [
    {
      "id": 7,
      "name": "ENG Scrum Board",
      "board_type": "scrum",
      "project_key": "ENG",
      "project_name": "Engineering",
      "columns": []
    }
  ]
}
```

### `GET /boards/{board_id}/sprints?offline=false`

Lists sprints for a Jira Agile board.

```json
{
  "board_id": 7,
  "sprints": [
    {
      "id": 42,
      "board_id": 7,
      "name": "Sprint 12",
      "state": "active",
      "start_date": "2026-11-09T00:00:00Z",
      "end_date": "2026-11-23T00:00:00Z",
      "goal": "Ship unified CLI"
    }
  ]
}
```

### `GET /boards/{board_id}/issues?sprint_id=&limit=50&offline=false`

Lists issues on a Jira Agile board, optionally scoped to `sprint_id`.

```json
{
  "board_id": 7,
  "sprint_id": 42,
  "issues": []
}
```

---

## Jira Issues

### `GET /issues?jql=&project=&limit=50&page_token=&start_at=&offline=false`

Searches Jira issues by explicit `jql`, `project` filter, or `my_open_issues_jql()`, or reads cached issues when `offline=true`. When `offline=true`, `total` is the total number of matching cached rows (not just the returned slice). On Cloud, pass `page_token` (from `next_page_token`); on Data Center, pass non-negative integer `start_at` (from `next_start_at`, default `0`). Supplying both `page_token` and `start_at` returns HTTP 400.

```bash
curl "http://127.0.0.1:8789/issues?project=ENG&limit=20"
# Data Center next page:
curl "http://127.0.0.1:8789/issues?jql=project%20%3D%20ENG&limit=20&start_at=20"
```

```json
{
  "issues": [],
  "next_page_token": null,
  "next_start_at": null,
  "total": 0
}
```

### `POST /issues`

Creates a new Jira issue (`OutgoingIssue`). `project_key` and `summary` are required; `issue_type` defaults to `"Task"`. `assignee_id` accepts an account ID, `"me"`, or a display name/email (resolved when the ID is rejected).

```bash
curl -X POST http://127.0.0.1:8789/issues \
  -H "Content-Type: application/json" \
  -d '{
    "project_key": "ENG",
    "issue_type": "Task",
    "summary": "Add OpenAPI schema",
    "description_markdown": "Document all HTTP endpoints.",
    "priority": "High",
    "labels": ["docs"]
  }'
```

```json
{
  "ok": true,
  "issue": {
    "summary": { "key": "ENG-101", "summary": "Add OpenAPI schema" }
  }
}
```

### `GET /issues/{key}?offline=false&compact=false`

Fetches full issue details (`IssueDetail`: metadata, Markdown description, subtasks, linked issues, linked Confluence pages, attachments, comments). Unlike the CLI (`--json`) and MCP detail tools, HTTP detail endpoints default to full (`compact=false`).

`compact=true` returns `key`, `summary`, `status`, and the full `description_markdown` inside the `issue` envelope (without CLI/MCP `omitted` or `hint` fields). It works for live and offline reads.

### `PATCH /issues/{key}`

Updates at least one issue field. Omit `assignee_id` to keep the current assignee, pass `null` to unassign, pass `"me"` for the current user, or pass an account ID (or display name/email, resolved when the ID is rejected).

Choose one body operation. `description_markdown` replaces the full description, `append_markdown` adds a fragment to the native description, and `replace_text: {"old_text":"…","new_text":"…"}` replaces one unique occurrence inside a single text node. The latter two preserve surrounding native content. Blank appends and unchanged Markdown skip the description write. Missing, ambiguous, and cross-node replacements return HTTP 400.

```json
{
  "summary": "Updated summary",
  "priority": "Medium",
  "assignee_id": null,
  "append_markdown": "Confirm monitoring before rollout."
}
```

### `DELETE /issues/{key}`

Deletes the Jira issue remotely and removes it from the local SQLite cache. Returns `{"ok": true, "key": "ENG-101"}`.

### `GET /issues/{key}/transitions`

Lists available workflow transitions (`IssueTransition[]`).

```json
{
  "key": "ENG-101",
  "transitions": [
    {
      "id": "21",
      "name": "In Progress",
      "to_status": { "id": "3", "name": "In Progress", "category": "inprogress" }
    }
  ]
}
```

### `POST /issues/{key}/transitions`

Transitions an issue to a target transition ID or status/transition name, with an optional Markdown comment.

```json
{
  "transition": "In Progress",
  "comment": "Starting implementation."
}
```

### `PUT /issues/{key}/assignee`

Assigns an issue (`"assignee_id": "me"` resolves to the current user; `null` or `""` unassigns).

```json
{
  "assignee_id": "me"
}
```

### `GET /issues/{key}/comments` & `POST /issues/{key}/comments`

Lists comments on an issue, or adds a Markdown comment (`{"body_markdown": "..."}`).

---

## Confluence Spaces & Pages

### `GET /spaces?refresh=false&limit=50`

Lists Confluence spaces from local SQLite cache or refreshes from Confluence API.

### `GET /spaces/{space}/pages?limit=50&offline=false`

Lists root pages in a Confluence space (`space` may be space key or ID).

### `POST /pages`

Creates a new Confluence page (`OutgoingPage`). `space_id_or_key` and `title` are required.

```json
{
  "space_id_or_key": "ENG",
  "parent_id": "9001",
  "title": "Runbook",
  "body_markdown": "# Overview\n\nDeployment checklist."
}
```

### `GET /pages/{id}?offline=false&compact=false`

Fetches full Confluence page detail (`PageDetail`: summary, ancestors, labels, `body_markdown`, `blocks`, `toc`, `attachments`, `comments`, `linked_issue_keys`). Unlike the CLI (`--json`) and MCP detail tools, HTTP detail endpoints default to full (`compact=false`).

`compact=true` returns `id`, `title`, `space_id`, `space_key`, `version_number`, and the full `body_markdown` inside the `page` envelope (without CLI/MCP `omitted` or `hint` fields). It works for live and offline reads. Detail endpoints reject unsupported query arguments.

### `GET /pages/{id}/children?offline=false`

Lists direct child pages of page `{id}`.

### `PUT /pages/{id}`

Choose one body operation. `body_markdown` replaces the full body, `append_markdown` adds a fragment to the native body, and `replace_text: {"old_text":"…","new_text":"…"}` replaces one unique occurrence inside a single text node.

For body edits, read `GET /pages/{id}?compact=true` and pass the returned `page.version_number` as `prev_version`. A stale version returns HTTP 409. Title and version-message edits preserve the native body. This example uses version 7.

```json
{
  "title": "Runbook (v2)",
  "replace_text": {"old_text": "canary", "new_text": "canary-verified"},
  "prev_version": 7,
  "version_message": "Clarified rollback steps"
}
```

### `DELETE /pages/{id}`

Deletes a Confluence page remotely and from local SQLite cache. Returns `{"ok": true, "id": "9001"}`.

### `GET /pages/{id}/comments` & `POST /pages/{id}/comments`

Lists footer comments on a Confluence page, or adds a Markdown comment (`{"body_markdown": "..."}`).

---

## Unified Search & Workspace Sync

### `GET /search?q=...&scope=all|jira|confluence&remote=false&limit=30`

Searches across Jira issues and Confluence pages.

- `remote=false` (default): searches local SQLite FTS5 (`issues_fts` / `pages_fts` + Korean short-query fallback).
- `remote=true`: executes Jira JQL (`text ~ "..."` for plain text, or compatible leading standard-field JQL such as `project = FLS` / `status IN (...)`) and/or Confluence CQL (`text ~ "..."` for plain text, or compatible leading standard-field CQL such as `space = ENG`). Phrases containing `and`/`or`/`=`/`~` (e.g. `read and write`, `timeout=30`) are treated as plain text. For custom raw JQL expressions, use `GET /issues?jql=customField%20%3D%20%22value%22`.
- `scope`: `all` (default), `jira`, or `confluence`.

```json
{
  "query": "Architecture",
  "scope": "all",
  "remote": false,
  "hits": []
}
```

### `POST /sync`

Runs `sync::bootstrap_workspace` synchronously and returns the summary plus `Storage::cache_counts()`.

```json
{
  "ok": true,
  "summary": "Sync complete",
  "counts": {
    "projects": 3,
    "boards": 2,
    "issues": 48,
    "spaces": 4,
    "pages": 65
  }
}
```
