# `jira` / `confluence` HTTP JSON API

Local HTTP/JSON server for AI agents, scripts, and integrations. Exposed by `jira serve` or `confluence serve` (alias `api`), backed by Atlassian REST APIs (Cloud & Data Center), local SQLite WAL/FTS5 caching, and the background sync engine (`sync::spawn_sync_engine`).

Both binary personas (`jira` and `confluence` / `conf`) mount the complete unified Jira, Jira Agile, Confluence, and cross-product search route surface on the same HTTP server.

## Starting the server

```bash
jira serve                      # http://127.0.0.1:8789
confluence serve                # same API server under the confluence persona
jira serve --port 9000          # custom port
jira serve --bind 127.0.0.1 --port 8789
jira serve --token secret123    # require Authorization: Bearer secret123
jira api                        # alias for `serve`
```

- `--bind` defaults to `127.0.0.1` (loopback only).
- `--port` defaults to `8789`.
- `--token` is optional; when set, every endpoint except `GET /health` requires `Authorization: Bearer <token>`.
- Requires an existing login (`jira login` or `confluence login`) for live/remote endpoints; `?offline=true` routes and local `/search` read directly from SQLite.
- On startup, `serve` spawns the background sync engine and requests `SyncCommand::RefreshAll`.

## Conventions

- All responses are JSON (`Content-Type: application/json`).
- Errors use `{"error": "<message>"}` with HTTP `400`, `401`, `404`, or `500`.
- `limit` defaults to `50` (`30` for `/search`) and is clamped to `1..=1000`.
- Permissive CORS (`CorsLayer::permissive()`) is enabled for local browser-based agent UIs.

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

### `GET /issues?jql=&project=&limit=50&page_token=&offline=false`

Searches Jira issues by `jql`, `project` filter, or `my_open_issues_jql()`, or reads cached issues when `offline=true`.

```bash
curl "http://127.0.0.1:8789/issues?project=ENG&limit=20"
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

Creates a new Jira issue (`OutgoingIssue`). `project_key` and `summary` are required; `issue_type` defaults to `"Task"`.

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

### `GET /issues/{key}?offline=false`

Fetches full issue details (`IssueDetail`: metadata, Markdown description, subtasks, linked issues, linked Confluence pages, attachments, comments).

### `PATCH /issues/{key}`

Updates fields on an issue (`IssueUpdate`). Requires at least one field (`summary`, `description_markdown`, `priority`, `assignee_id`, `labels`).

```json
{
  "summary": "Updated summary",
  "priority": "Medium",
  "assignee_id": "557058:..."
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

### `GET /pages/{id}?offline=false`

Fetches full Confluence page detail (`PageDetail`: summary, ancestors, labels, `body_markdown`, `blocks`, `toc`, `attachments`, `comments`, `linked_issue_keys`).

### `GET /pages/{id}/children?offline=false`

Lists direct child pages of page `{id}`.

### `PUT /pages/{id}`

Updates a Confluence page. If `prev_version` is `0`/omitted or `title`/`body_markdown` is omitted, the server automatically fetches the current page first to resolve `version_number` and unchanged fields.

```json
{
  "title": "Runbook (v2)",
  "body_markdown": "# Updated",
  "prev_version": 0,
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
- `remote=true`: executes Jira JQL (`text ~ "..."` or raw JQL expression) and/or Confluence CQL (`text ~ "..."` or raw CQL expression).
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
  "summary": "동기화 완료",
  "counts": {
    "projects": 3,
    "boards": 2,
    "issues": 48,
    "spaces": 4,
    "pages": 65
  }
}
```
