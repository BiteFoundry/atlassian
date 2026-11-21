<div align="center">

# `@bitefoundry/atlassian`

**Unified Atlassian Jira & Confluence native terminal client & AI agent toolkit — CLI · TUI · HTTP API · MCP.**

[![npm version](https://img.shields.io/npm/v/%40bitefoundry%2Fatlassian?style=flat-square&color=0052CC)](https://www.npmjs.com/package/@bitefoundry/atlassian)
[![platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-24292F?style=flat-square)](docs/NPM.md)
[![Rust & Ratatui](https://img.shields.io/badge/built%20with-Rust%20%2B%20Ratatui-DEA584?style=flat-square&logo=rust)](https://github.com/BiteFoundry/atlassian/blob/master/Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-00875A?style=flat-square)](package.json)

<br />

![Atlassian TUI Demo](https://raw.githubusercontent.com/BiteFoundry/atlassian/master/assets/demo.gif)

</div>

---

## Highlights

| Pillar | Capabilities |
| :--- | :--- |
| **Minimal-Chrome TUI** | Dual-persona binary (`atlassian` / `confluence` / `conf`) with multi-column **Kanban Board**, **Issues & JQL Explorer**, **Confluence Space & Page Tree + TOC**, **Multi-Site Federation (`Ctrl+S`)** across Cloud OAuth 2.0 + Data Center PAT, **Link Extractor (`u`)**, **File Picker (`Ctrl+F`)** with `zoxide` jump, **Command Palette (`Ctrl+P`)**, and **Live Settings (`Ctrl+,`)** (5 palettes, Cozy/Compact density). |
| **High-Res Terminal Images** | Inline attachment previews for Jira issues & Confluence pages via **Kitty Graphics**, **iTerm2**, **Sixel**, and **24-bit Truecolor Half-Block (`▀`)** fallback (`v` HD modal & `--preview-images`). |
| **Local SQLite FTS5 + IPC Daemon** | Embedded SQLite WAL + FTS5 cache (`cache.db`) with instant offline unified search across Jira issues & Confluence pages (including `<3`-char CJK/Korean fallback), plus single-instance background sync IPC daemon (`atlassian daemon start`). |
| **4-in-1 Agent Interface** | Same binary powers the **Interactive TUI**, scriptable **`--json` CLI**, local **HTTP JSON API** (`atlassian serve`, `:8789`, Bearer auth), and **stdio MCP server** (`atlassian mcp`, typed tools discovered through `tools/list` with `confirm: true` delete guards). |

---

## Quick Start

```bash
# 1. Install globally (links `atlassian`, `confluence`, `conf` native binaries)
npm i -g @bitefoundry/atlassian

# 2. Authenticate — Atlassian Cloud (OAuth 2.0 or API Token) or Data Center (PAT)
atlassian login --url https://acme.atlassian.net --email you@acme.com --token "$ATLASSIAN_API_TOKEN"
# Or Data Center / Server:
atlassian login --url https://jira.acme.internal --confluence-url https://wiki.acme.internal --deployment datacenter --pat "$ATLASSIAN_PAT"
# Or Atlassian Cloud OAuth 2.0 (3LO) with multi-site discovery:
atlassian login --oauth --client-id "$ATLASSIAN_OAUTH_CLIENT_ID"

# 3. Launch the TUI (Jira Kanban or Confluence Wiki persona)
atlassian          # Jira Kanban Board & Issue Explorer
confluence    # Confluence Space Tree & Wiki Reader (alias: conf)
```

---

## Keybindings Cheat Sheet

| Key | Action | Context |
| :--- | :--- | :--- |
| `1`–`6` / `Ctrl+1`–`6` | Switch tabs: Board / Issues / My Work / Wiki / Search / Directory | Global |
| `Tab` / `Shift+Tab` | Cycle focus across Nav rail, Main canvas, and Detail/TOC drawer | Global |
| `Ctrl+B` | Toggle left navigation sidebar | Global |
| `h` `j` `k` `l` / Arrows | Navigate Kanban columns, cards, issue lists, and page trees | Board / Lists / Tree |
| `m` / `i` / `a` | Transition issue status / Assign to me / Pick assignee | Jira |
| `c` / `n` / `e` | Add comment / Create issue or child page / Edit (`Ctrl+E` `$EDITOR`) | Jira / Confluence |
| `g` / `Backspace` | Jump across linked `Jira <-> Confluence` refs / navigate back | Global |
| `Space` / `t` | Expand or collapse Confluence tree node / toggle TOC drawer | Confluence |
| `u` / `Ctrl+F` / `v` / `d` | Link Extractor / File Picker (`z` zoxide) / Image Preview / Download | Global |
| `Ctrl+S` | Multi-Site Switcher (`All Sites` federated view, Cloud + DC) | Global |
| `Ctrl+P` / `:` | Command Palette (fuzzy search every action) | Global |
| `Ctrl+,` / `?` / `q` | Live Settings / Keyboard Shortcuts Help / Quit | Global |

---

## CLI, HTTP API & MCP

### CLI (`--json` ready)

```bash
# Multi-site management & diagnostics
atlassian sites list --json
atlassian doctor

# Jira boards, issue search and edits
atlassian boards --project ENG --json
atlassian issues --jql "project = ENG AND sprint in openSprints()" --json
atlassian search "migration runbook" --json
atlassian view ENG-101 --json
atlassian view ENG-101 --json --full
atlassian edit ENG-101 --append "Confirm monitoring before rollout." --dry-run
atlassian transition ENG-101 "In Review" --comment "Canary 50% green"
atlassian attach ENG-101 ./architecture.png

# Confluence spaces, pages and versioned edits
conf spaces --json
conf pages ENG --json
conf view 40211 --json
conf view 40211 --json --full
# This example uses page.version_number 7.
conf edit 40211 --prev-version 7 --replace "canary" "canary-verified"
# Preview replacing the body under a top-level Confluence Cloud heading.
confluence edit 40211 --prev-version 7 --replace-section "Rollout" "Verify the canary." --dry-run

# Jira issue relationships, sprint and time
atlassian parent ENG-101 ENG-9 --json
atlassian custom-fields ENG-101 --fields-file ./fields.json --json
atlassian link ENG-101 ENG-102 --type Blocks --json
atlassian move-to-sprint 42 ENG-101 ENG-102 --json
atlassian log-work ENG-101 --seconds 1800 --json

# Confluence page metadata and files
conf move 40211 40100 --json
conf labels 40211 --add ops --add rollout --json
conf labels 40211 --remove rollout --json
conf attach 40211 ./checklist.txt --json

# Background sync daemon & self-updater
atlassian daemon start
atlassian update --check
```

Add `--dry-run` to print the pending changes as JSON before saving.

If you are upgrading from 0.3.x, change shell commands such as `jira view ENG-101 --json` to `atlassian view ENG-101 --json`. In MCP settings, change `"command": "jira"` to `"command": "atlassian"` and keep the `mcp` argument.

If you are upgrading from 0.5.x, `atlassian view --json` and `conf view --json` now return compact JSON by default (with `omitted` counts and a `hint`). Pass `--full` (for example `atlassian view ENG-101 --json --full`) in scripts that expect the full structure. `projects --json`, `boards --json`, and MCP `list_projects` / `list_boards` default to 50 items with `has_more` and `next_cursor`; pass `--limit 1000` / `limit: 1000` and continue with `cursor` when needed. `has_children` on page summaries can now be `null` when unknown.

For Jira, `fields.json` maps explicit field IDs to JSON values, such as `{"customfield_10100":3}`. The Data Center parent command sets a Jira Software epic on an eligible issue. Confluence Data Center page moves take `--prev-version` from the current page read and require a parent in the same space. [Operation details and supported APIs](docs/MCP.md#incremental-jira-and-confluence-operations)

### Local HTTP JSON API (`atlassian serve`)

```bash
atlassian serve --bind 127.0.0.1 --port 8789 --token "$API_TOKEN"
# Search Jira issues and Confluence pages together.
curl -H "Authorization: Bearer $API_TOKEN" "http://127.0.0.1:8789/search?q=gateway&scope=all"
```

Full endpoint catalog and OpenAPI schema: **[docs/API.md](docs/API.md)** · **[docs/openapi.yaml](docs/openapi.yaml)**.

### MCP Server (`atlassian mcp`)

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

Both CLI `--json` detail views (`atlassian view`, `atlassian wiki view`, `conf view`) and the two MCP detail tools (`get_jira_issue`, `get_confluence_page`) return compact responses with identifying fields, full Markdown, an `omitted` summary, and a `hint` by default. Pass `--json --full` in the CLI or `compact: false` in MCP for native body data and the full structure. In `omitted`, a `null` count means the collection could not be confirmed in full (failed or truncated subresource).

Full tool catalog and argument schemas: **[docs/MCP.md](docs/MCP.md)** · npm packaging details: **[docs/NPM.md](docs/NPM.md)**.

---

## `@bitefoundry` Enterprise Terminal Suite

| Package | Commands | Workspace | Interfaces |
| :--- | :--- | :--- | :--- |
| [`@bitefoundry/teams`](https://www.npmjs.com/package/@bitefoundry/teams) | `teams` | Microsoft Teams (Chats, Channels, Meetings & Threads) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/outlook`](https://www.npmjs.com/package/@bitefoundry/outlook) | `outlook` | Microsoft Outlook (Mail, Calendar & Contacts) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/onenote`](https://www.npmjs.com/package/@bitefoundry/onenote) | `onenote` | Microsoft OneNote (Notebooks, Sections & Pages) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/sharepoint`](https://www.npmjs.com/package/@bitefoundry/sharepoint) | `sharepoint`, `bitefoundry-onedrive` | Microsoft SharePoint & OneDrive (Sites, Libraries & Lists) | TUI · CLI · HTTP · MCP |
| **`@bitefoundry/atlassian`** | `atlassian`, `confluence`, `conf` | Atlassian Jira (Kanban, Sprints, JQL) & Confluence Wiki | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/notion`](https://www.npmjs.com/package/@bitefoundry/notion) | `notion` | Notion (Editorial Pages, Interactive Blocks & Databases) | TUI · CLI · HTTP · MCP |

---

> **Disclaimer:** Unofficial terminal client for Atlassian Jira & Confluence. Not affiliated with, endorsed by, or sponsored by Atlassian.
