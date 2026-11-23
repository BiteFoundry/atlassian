//! SQLite WAL initialization, `PRAGMA user_version` v1 -> v2 migrations, and FTS5 trigram + Korean `<3` char `LIKE` helpers.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::Storage;

pub const DEFAULT_SITE_ID: &str = "default";
pub const SCHEMA_VERSION: i32 = 2;

const V2_SITES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS sites (
    site_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    site_url TEXT NOT NULL,
    deployment TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_synced_at TEXT,
    last_error TEXT
);
";

const V2_PROJECTS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS projects (
    site_id TEXT NOT NULL DEFAULT 'default',
    key TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    project_type TEXT NOT NULL,
    lead_name TEXT,
    avatar_url TEXT,
    pinned INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (site_id, key)
);
";

const V2_BOARDS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS boards (
    site_id TEXT NOT NULL DEFAULT 'default',
    id INTEGER NOT NULL,
    name TEXT NOT NULL,
    board_type TEXT NOT NULL,
    project_key TEXT,
    project_name TEXT,
    columns_json TEXT NOT NULL,
    PRIMARY KEY (site_id, id)
);
";

const V2_SPRINTS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS sprints (
    site_id TEXT NOT NULL DEFAULT 'default',
    id INTEGER NOT NULL,
    board_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL,
    start_date TEXT,
    end_date TEXT,
    goal TEXT,
    PRIMARY KEY (site_id, id)
);
";

/// Which boards list which sprints. A sprint shared across boards has one `sprints` row and one
/// link per board. Additive to schema v2: binaries that predate it ignore the table, so it is
/// created and backfilled on open without changing `PRAGMA user_version`.
const BOARD_SPRINTS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS board_sprints (
    site_id   TEXT    NOT NULL DEFAULT 'default',
    board_id  INTEGER NOT NULL,
    sprint_id INTEGER NOT NULL,
    PRIMARY KEY (site_id, board_id, sprint_id)
);
";

/// The signed-in Jira user of each site, saved by `whoami` so `issues --offline --mine` can
/// match assignees without a request. Additive to schema v2 like `board_sprints`.
const SITE_IDENTITIES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS site_identities (
    site_id    TEXT PRIMARY KEY,
    user_json  TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

const V2_ISSUES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS issues (
    site_id TEXT NOT NULL DEFAULT 'default',
    key TEXT NOT NULL,
    id TEXT NOT NULL,
    project_key TEXT NOT NULL,
    board_id INTEGER,
    sprint_id INTEGER,
    summary TEXT NOT NULL,
    status_json TEXT NOT NULL,
    priority_json TEXT,
    issue_type_json TEXT NOT NULL,
    assignee_json TEXT,
    reporter_json TEXT,
    parent_key TEXT,
    parent_summary TEXT,
    labels_json TEXT NOT NULL,
    story_points REAL,
    sprint_name TEXT,
    comment_count INTEGER NOT NULL DEFAULT 0,
    attachment_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    web_url TEXT NOT NULL,
    description_markdown TEXT NOT NULL DEFAULT '',
    detail_json TEXT,
    PRIMARY KEY (site_id, key)
);
";

const V2_SAVED_FILTERS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS saved_filters (
    site_id TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    jql TEXT NOT NULL,
    builtin INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (site_id, id)
);
";

const V2_SPACES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS spaces (
    site_id TEXT NOT NULL DEFAULT 'default',
    key TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    space_type TEXT NOT NULL,
    homepage_id TEXT,
    description TEXT,
    web_url TEXT NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (site_id, key)
);
";

const V2_PAGES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS pages (
    site_id TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL,
    space_id TEXT NOT NULL,
    space_key TEXT NOT NULL,
    parent_id TEXT,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    version_number INTEGER NOT NULL DEFAULT 1,
    author_name TEXT,
    updated_at TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    has_children INTEGER NOT NULL DEFAULT 0,
    web_url TEXT NOT NULL,
    body_markdown TEXT NOT NULL DEFAULT '',
    detail_json TEXT,
    PRIMARY KEY (site_id, id)
);
";

const V2_INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_sprints_board
    ON sprints(site_id, board_id, id DESC);

CREATE INDEX IF NOT EXISTS idx_issues_project_updated
    ON issues(site_id, project_key, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_issues_board_sprint
    ON issues(site_id, board_id, sprint_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_issues_updated
    ON issues(updated_at DESC, site_id ASC, key ASC);

CREATE INDEX IF NOT EXISTS idx_pages_space_parent_pos
    ON pages(site_id, space_key, parent_id, position ASC, title ASC);

CREATE INDEX IF NOT EXISTS idx_pages_updated
    ON pages(updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_pages_site_updated
    ON pages(site_id, updated_at DESC);
";

const V2_ISSUES_FTS_TRIGRAM_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS issues_fts USING fts5(
    site_id UNINDEXED,
    issue_key UNINDEXED,
    project_key,
    summary,
    description_text,
    labels_text,
    tokenize='trigram'
);
";

const V2_ISSUES_FTS_UNICODE61_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS issues_fts USING fts5(
    site_id UNINDEXED,
    issue_key UNINDEXED,
    project_key,
    summary,
    description_text,
    labels_text,
    tokenize='unicode61'
);
";

const V2_PAGES_FTS_TRIGRAM_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    site_id UNINDEXED,
    page_id UNINDEXED,
    space_key,
    title,
    body_text,
    tokenize='trigram'
);
";

const V2_PAGES_FTS_UNICODE61_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    site_id UNINDEXED,
    page_id UNINDEXED,
    space_key,
    title,
    body_text,
    tokenize='unicode61'
);
";

pub fn open_file(path: &Path) -> Result<Storage> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open SQLite database: {}", path.display()))?;
    init_connection(&conn, false)?;
    Ok(Storage {
        conn: Arc::new(Mutex::new(conn)),
        path: Some(path.to_path_buf()),
    })
}

pub fn open_memory() -> Result<Storage> {
    let conn =
        Connection::open_in_memory().context("Failed to open in-memory SQLite database")?;
    init_connection(&conn, true)?;
    Ok(Storage {
        conn: Arc::new(Mutex::new(conn)),
        path: None,
    })
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    init_connection(conn, false)
}

fn init_connection(conn: &Connection, is_memory: bool) -> Result<()> {
    if !is_memory {
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        ",
    )
    .context("Failed to configure SQLite PRAGMAs")?;

    let user_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("Failed to read SQLite PRAGMA user_version")?;

    if user_version < SCHEMA_VERSION && has_legacy_v1_schema(conn)? {
        migrate_v1_to_v2(conn).context("Failed to migrate SQLite schema from v1 to v2")?;
    } else {
        create_v2_schema(conn)?;
        if user_version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .context("Failed to update SQLite PRAGMA user_version to v2")?;
        }
    }
    ensure_detail_freshness_columns(conn)?;
    ensure_board_sprints(conn)?;
    conn.execute_batch(SITE_IDENTITIES_TABLE_SQL)
        .context("Failed to create site_identities table")?;

    Ok(())
}

/// Creates `board_sprints` and links every cached sprint to the board stored on its row. Runs on
/// every open and is idempotent, so sprints written by a binary without the table (first upgrade,
/// or a downgrade and re-upgrade) get their link.
fn ensure_board_sprints(conn: &Connection) -> Result<()> {
    conn.execute_batch(BOARD_SPRINTS_TABLE_SQL)
        .context("Failed to create board_sprints table")?;
    if table_exists(conn, "sprints")? {
        conn.execute(
            "INSERT OR IGNORE INTO board_sprints (site_id, board_id, sprint_id)
             SELECT site_id, board_id, id FROM sprints",
            [],
        )
        .context("Failed to backfill board_sprints from sprints")?;
    }
    Ok(())
}

fn ensure_detail_freshness_columns(conn: &Connection) -> Result<()> {
    if table_exists(conn, "issues")? && !table_has_column(conn, "issues", "detail_updated_at")? {
        conn.execute("ALTER TABLE issues ADD COLUMN detail_updated_at TEXT", [])
            .context("Failed to add detail_updated_at column to issues")?;
    }
    if table_exists(conn, "pages")? && !table_has_column(conn, "pages", "detail_version")? {
        conn.execute("ALTER TABLE pages ADD COLUMN detail_version INTEGER", [])
            .context("Failed to add detail_version column to pages")?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table_name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn table_has_column(conn: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_legacy_v1_schema(conn: &Connection) -> Result<bool> {
    const LEGACY_TABLES: &[&str] = &[
        "projects",
        "boards",
        "sprints",
        "issues",
        "saved_filters",
        "spaces",
        "pages",
        "issues_fts",
        "pages_fts",
    ];
    for tbl in LEGACY_TABLES {
        if table_exists(conn, tbl)? && !table_has_column(conn, tbl, "site_id")? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn create_v2_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(V2_SITES_TABLE_SQL)
        .context("Failed to create sites table")?;
    conn.execute_batch(V2_PROJECTS_TABLE_SQL)
        .context("Failed to create projects table")?;
    conn.execute_batch(V2_BOARDS_TABLE_SQL)
        .context("Failed to create boards table")?;
    conn.execute_batch(V2_SPRINTS_TABLE_SQL)
        .context("Failed to create sprints table")?;
    conn.execute_batch(V2_ISSUES_TABLE_SQL)
        .context("Failed to create issues table")?;
    conn.execute_batch(V2_SAVED_FILTERS_TABLE_SQL)
        .context("Failed to create saved_filters table")?;
    conn.execute_batch(V2_SPACES_TABLE_SQL)
        .context("Failed to create spaces table")?;
    conn.execute_batch(V2_PAGES_TABLE_SQL)
        .context("Failed to create pages table")?;
    conn.execute_batch(V2_INDEXES_SQL)
        .context("Failed to create SQLite v2 indexes")?;

    ensure_fts5_table(
        conn,
        "issues_fts",
        V2_ISSUES_FTS_TRIGRAM_SQL,
        V2_ISSUES_FTS_UNICODE61_SQL,
    )?;

    ensure_fts5_table(
        conn,
        "pages_fts",
        V2_PAGES_FTS_TRIGRAM_SQL,
        V2_PAGES_FTS_UNICODE61_SQL,
    )?;

    seed_builtin_filters(conn)?;
    Ok(())
}

fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("Failed to begin v1 -> v2 schema migration transaction")?;

    tx.execute_batch(V2_SITES_TABLE_SQL)
        .context("Failed to create sites table during migration")?;

    migrate_table_if_v1(
        &tx,
        "projects",
        V2_PROJECTS_TABLE_SQL,
        "key, id, name, project_type, lead_name, avatar_url, pinned",
    )?;
    migrate_table_if_v1(
        &tx,
        "boards",
        V2_BOARDS_TABLE_SQL,
        "id, name, board_type, project_key, project_name, columns_json",
    )?;
    migrate_table_if_v1(
        &tx,
        "sprints",
        V2_SPRINTS_TABLE_SQL,
        "id, board_id, name, state, start_date, end_date, goal",
    )?;
    migrate_table_if_v1(
        &tx,
        "issues",
        V2_ISSUES_TABLE_SQL,
        "key, id, project_key, board_id, sprint_id, summary, status_json, priority_json, \
         issue_type_json, assignee_json, reporter_json, parent_key, parent_summary, labels_json, \
         story_points, sprint_name, comment_count, attachment_count, created_at, updated_at, \
         web_url, description_markdown, detail_json",
    )?;
    migrate_table_if_v1(
        &tx,
        "saved_filters",
        V2_SAVED_FILTERS_TABLE_SQL,
        "id, name, jql, builtin",
    )?;
    migrate_table_if_v1(
        &tx,
        "spaces",
        V2_SPACES_TABLE_SQL,
        "key, id, name, space_type, homepage_id, description, web_url, pinned",
    )?;
    migrate_table_if_v1(
        &tx,
        "pages",
        V2_PAGES_TABLE_SQL,
        "id, space_id, space_key, parent_id, title, status, version_number, author_name, \
         updated_at, position, has_children, web_url, body_markdown, detail_json",
    )?;

    tx.execute_batch(V2_INDEXES_SQL)
        .context("Failed to create v2 indexes during migration")?;

    migrate_issues_fts_if_v1(&tx)?;
    migrate_pages_fts_if_v1(&tx)?;

    seed_builtin_filters(&tx)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("Failed to set SQLite PRAGMA user_version = 2 during migration")?;
    tx.commit()
        .context("Failed to commit v1 -> v2 schema migration transaction")?;
    Ok(())
}

fn migrate_table_if_v1(
    conn: &Connection,
    table_name: &str,
    v2_create_sql: &str,
    v1_columns: &str,
) -> Result<()> {
    if table_exists(conn, table_name)? && !table_has_column(conn, table_name, "site_id")? {
        let old_name = format!("_v1_migrating_{table_name}");
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS {old_name}; ALTER TABLE {table_name} RENAME TO {old_name};"
        ))
        .with_context(|| format!("Failed to rename legacy v1 table `{table_name}`"))?;
        conn.execute_batch(v2_create_sql)
            .with_context(|| format!("Failed to create v2 table `{table_name}`"))?;
        let copy_sql = format!(
            "INSERT INTO {table_name} (site_id, {v1_columns}) \
             SELECT ?1, {v1_columns} FROM {old_name}"
        );
        conn.execute(&copy_sql, [DEFAULT_SITE_ID])
            .with_context(|| format!("Failed to copy v1 rows into v2 table `{table_name}`"))?;
        conn.execute_batch(&format!("DROP TABLE {old_name};"))
            .with_context(|| format!("Failed to drop temporary table `{old_name}`"))?;
    } else {
        conn.execute_batch(v2_create_sql)
            .with_context(|| format!("Failed to ensure v2 table `{table_name}`"))?;
    }
    Ok(())
}

fn migrate_issues_fts_if_v1(conn: &Connection) -> Result<()> {
    let exists = table_exists(conn, "issues_fts")?;
    let needs_rebuild = !exists || !table_has_column(conn, "issues_fts", "site_id")?;
    if !needs_rebuild {
        return Ok(());
    }

    let mut preserved: HashMap<String, (String, String)> = HashMap::new();
    if exists {
        if let Ok(mut stmt) =
            conn.prepare("SELECT issue_key, description_text, labels_text FROM issues_fts")
        {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            }) {
                for row in rows.flatten() {
                    preserved.insert(row.0, (row.1, row.2));
                }
            }
        }
        conn.execute_batch("DROP TABLE IF EXISTS issues_fts;")
            .context("Failed to drop legacy v1 issues_fts table")?;
    }

    ensure_fts5_table(
        conn,
        "issues_fts",
        V2_ISSUES_FTS_TRIGRAM_SQL,
        V2_ISSUES_FTS_UNICODE61_SQL,
    )?;

    let mut select_stmt = conn.prepare(
        "SELECT site_id, key, project_key, summary, description_markdown, labels_json FROM issues",
    )?;
    let issues_rows: Vec<(String, String, String, String, String, String)> = select_stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut insert_stmt = conn.prepare(
        "
        INSERT INTO issues_fts (site_id, issue_key, project_key, summary, description_text, labels_text)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
    )?;

    for (site_id, key, project_key, summary, desc_md, labels_json) in issues_rows {
        let fallback_labels = serde_json::from_str::<Vec<String>>(&labels_json)
            .map(|v| v.join(" "))
            .unwrap_or_else(|_| labels_json.clone());
        let (desc_text, labels_text) = if site_id == DEFAULT_SITE_ID {
            if let Some((old_desc, old_labels)) = preserved.get(&key) {
                let d = if !old_desc.is_empty() {
                    old_desc.clone()
                } else {
                    desc_md
                };
                let l = if !old_labels.is_empty() {
                    old_labels.clone()
                } else {
                    fallback_labels
                };
                (d, l)
            } else {
                (desc_md, fallback_labels)
            }
        } else {
            (desc_md, fallback_labels)
        };
        insert_stmt.execute(params![
            site_id,
            key,
            project_key,
            summary,
            desc_text,
            labels_text,
        ])?;
    }

    Ok(())
}

fn migrate_pages_fts_if_v1(conn: &Connection) -> Result<()> {
    let exists = table_exists(conn, "pages_fts")?;
    let needs_rebuild = !exists || !table_has_column(conn, "pages_fts", "site_id")?;
    if !needs_rebuild {
        return Ok(());
    }

    let mut preserved: HashMap<String, String> = HashMap::new();
    if exists {
        if let Ok(mut stmt) = conn.prepare("SELECT page_id, body_text FROM pages_fts") {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            }) {
                for row in rows.flatten() {
                    preserved.insert(row.0, row.1);
                }
            }
        }
        conn.execute_batch("DROP TABLE IF EXISTS pages_fts;")
            .context("Failed to drop legacy v1 pages_fts table")?;
    }

    ensure_fts5_table(
        conn,
        "pages_fts",
        V2_PAGES_FTS_TRIGRAM_SQL,
        V2_PAGES_FTS_UNICODE61_SQL,
    )?;

    let mut select_stmt =
        conn.prepare("SELECT site_id, id, space_key, title, body_markdown FROM pages")?;
    let pages_rows: Vec<(String, String, String, String, String)> = select_stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut insert_stmt = conn.prepare(
        "
        INSERT INTO pages_fts (site_id, page_id, space_key, title, body_text)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
    )?;

    for (site_id, page_id, space_key, title, body_md) in pages_rows {
        let body_text = if site_id == DEFAULT_SITE_ID {
            preserved
                .get(&page_id)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or(body_md)
        } else {
            body_md
        };
        insert_stmt.execute(params![site_id, page_id, space_key, title, body_text])?;
    }

    Ok(())
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql).with_context(|| {
            format!("Failed to create FTS5 virtual table ({table_name}, unicode61)")
        })?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    Ok(sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true))
}

fn seed_builtin_filters(conn: &Connection) -> Result<()> {
    let builtins = [
        (
            "my_open",
            "Assigned to me (open)",
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC",
        ),
        (
            "reported",
            "Reported by me",
            "reporter = currentUser() ORDER BY updated DESC",
        ),
        (
            "recent",
            "Recently updated (7d)",
            "updated >= -7d ORDER BY updated DESC",
        ),
        (
            "all_open",
            "All open",
            "statusCategory != Done ORDER BY updated DESC",
        ),
    ];

    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO saved_filters (site_id, id, name, jql, builtin) VALUES (?1, ?2, ?3, ?4, 1)",
    )?;
    for (id, name, jql) in builtins {
        stmt.execute(params![DEFAULT_SITE_ID, id, name, jql])?;
    }
    Ok(())
}

pub(crate) fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn build_fts_query(trimmed: &str) -> Option<String> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 && tokens.iter().all(|t| t.chars().count() >= 3) {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else if trimmed.chars().count() >= 3 && tokens.len() == 1 {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    } else {
        None
    }
}

pub(crate) fn build_contextual_snippet(
    title: &str,
    body: &str,
    extra: &str,
    query: &str,
    max_chars: usize,
) -> String {
    let clean_body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let source = if !clean_body.is_empty() {
        clean_body
    } else if !extra.trim().is_empty() {
        extra.trim().to_string()
    } else {
        title.trim().to_string()
    };

    let chars: Vec<char> = source.chars().collect();
    if chars.len() <= max_chars {
        return source;
    }

    let query_lower = query.trim().to_lowercase();
    let match_char_idx = find_case_insensitive_char_pos(&source, &query_lower)
        .or_else(|| {
            query_lower
                .split_whitespace()
                .filter_map(|tok| find_case_insensitive_char_pos(&source, tok))
                .min()
        })
        .unwrap_or(0);

    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(chars.len());
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let slice: String = chars[start..end].iter().collect();
    out.push_str(slice.trim());
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn find_case_insensitive_char_pos(haystack: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let haystack_lower = haystack.to_lowercase();
    let byte_idx = haystack_lower.find(needle_lower)?;
    Some(haystack_lower[..byte_idx].chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_legacy_v1_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE projects (
                key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                name TEXT NOT NULL,
                project_type TEXT NOT NULL,
                lead_name TEXT,
                avatar_url TEXT,
                pinned INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE boards (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                board_type TEXT NOT NULL,
                project_key TEXT,
                project_name TEXT,
                columns_json TEXT NOT NULL
            );
            CREATE TABLE sprints (
                id INTEGER PRIMARY KEY,
                board_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                state TEXT NOT NULL,
                start_date TEXT,
                end_date TEXT,
                goal TEXT
            );
            CREATE TABLE issues (
                key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                project_key TEXT NOT NULL,
                board_id INTEGER,
                sprint_id INTEGER,
                summary TEXT NOT NULL,
                status_json TEXT NOT NULL,
                priority_json TEXT,
                issue_type_json TEXT NOT NULL,
                assignee_json TEXT,
                reporter_json TEXT,
                parent_key TEXT,
                parent_summary TEXT,
                labels_json TEXT NOT NULL,
                story_points REAL,
                sprint_name TEXT,
                comment_count INTEGER NOT NULL DEFAULT 0,
                attachment_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                web_url TEXT NOT NULL,
                description_markdown TEXT NOT NULL DEFAULT '',
                detail_json TEXT
            );
            CREATE TABLE saved_filters (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                jql TEXT NOT NULL,
                builtin INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE spaces (
                key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                name TEXT NOT NULL,
                space_type TEXT NOT NULL,
                homepage_id TEXT,
                description TEXT,
                web_url TEXT NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE pages (
                id TEXT PRIMARY KEY,
                space_id TEXT NOT NULL,
                space_key TEXT NOT NULL,
                parent_id TEXT,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                version_number INTEGER NOT NULL DEFAULT 1,
                author_name TEXT,
                updated_at TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0,
                has_children INTEGER NOT NULL DEFAULT 0,
                web_url TEXT NOT NULL,
                body_markdown TEXT NOT NULL DEFAULT '',
                detail_json TEXT
            );
            CREATE VIRTUAL TABLE issues_fts USING fts5(
                issue_key UNINDEXED,
                project_key,
                summary,
                description_text,
                labels_text,
                tokenize='trigram'
            );
            CREATE VIRTUAL TABLE pages_fts USING fts5(
                page_id UNINDEXED,
                space_key,
                title,
                body_text,
                tokenize='trigram'
            );
            ",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO projects VALUES ('ENG', 'p1', 'Engineering', 'software', 'Alice', NULL, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO boards VALUES (1, 'Eng Scrum', 'scrum', 'ENG', 'Engineering', '[]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sprints VALUES (10, 1, 'Sprint 10', 'active', '2026-11-01', '2026-11-14', 'Ship v2')",
            [],
        )
        .unwrap();
        let status_json = r#"{"id":"1","name":"In Progress","category":"inprogress"}"#;
        let type_json = r#"{"id":"10","name":"Task","subtask":false,"epic":false}"#;
        let issue_detail = crate::models::IssueDetail {
            summary: crate::models::IssueSummary {
                id: "i1".into(),
                key: "ENG-1".into(),
                project_key: "ENG".into(),
                summary: "Legacy v1 issue".into(),
                status: crate::models::IssueStatus {
                    id: "1".into(),
                    name: "In Progress".into(),
                    category: crate::models::StatusCategory::InProgress,
                },
                issue_type: crate::models::IssueType {
                    id: "10".into(),
                    name: "Task".into(),
                    subtask: false,
                    epic: false,
                },
                labels: vec!["legacy".into(), "배포".into()],
                story_points: Some(5.0),
                sprint_id: Some(10),
                sprint_name: Some("Sprint 10".into()),
                comment_count: 2,
                attachment_count: 1,
                created_at: "2026-11-01T00:00:00Z".into(),
                updated_at: "2026-11-15T12:00:00Z".into(),
                web_url: "https://legacy.atlassian.net/browse/ENG-1".into(),
                ..Default::default()
            },
            description_markdown: "무중단 배포 체크리스트".into(),
            ..Default::default()
        };
        let issue_detail_json = serde_json::to_string(&issue_detail).unwrap();
        conn.execute(
            "INSERT INTO issues VALUES (
                'ENG-1', 'i1', 'ENG', 1, 10, 'Legacy v1 issue', ?1, NULL, ?2,
                NULL, NULL, NULL, NULL, '[\"legacy\",\"배포\"]', 5.0, 'Sprint 10',
                2, 1, '2026-11-01T00:00:00Z', '2026-11-15T12:00:00Z',
                'https://legacy.atlassian.net/browse/ENG-1', '무중단 배포 체크리스트', ?3
            )",
            params![status_json, type_json, issue_detail_json],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues_fts VALUES ('ENG-1', 'ENG', 'Legacy v1 issue', '무중단 배포 체크리스트', 'legacy 배포')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO saved_filters VALUES ('custom_1', 'My Custom', 'project = ENG', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO spaces VALUES ('DOCS', 's1', 'Documentation', 'global', '100', 'Legacy docs', 'https://legacy.atlassian.net/wiki/spaces/DOCS', 1)",
            [],
        )
        .unwrap();
        let page_detail = crate::models::PageDetail {
            summary: crate::models::PageSummary {
                id: "100".into(),
                space_id: "s1".into(),
                space_key: "DOCS".into(),
                parent_id: None,
                title: "아키텍처 가이드".into(),
                status: "current".into(),
                version_number: 4,
                author_name: Some("Bob".into()),
                updated_at: "2026-11-15T13:00:00Z".into(),
                position: 1,
                has_children: None,
                web_url: "https://legacy.atlassian.net/wiki/spaces/DOCS/pages/100".into(),
            },
            body_markdown: "배포 절차 안내".into(),
            ..Default::default()
        };
        let page_detail_json = serde_json::to_string(&page_detail).unwrap();
        conn.execute(
            "INSERT INTO pages VALUES (
                '100', 's1', 'DOCS', NULL, '아키텍처 가이드', 'current', 4, 'Bob',
                '2026-11-15T13:00:00Z', 1, 0,
                'https://legacy.atlassian.net/wiki/spaces/DOCS/pages/100', '배포 절차 안내', ?1
            )",
            params![page_detail_json],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages_fts VALUES ('100', 'DOCS', '아키텍처 가이드', '배포 절차 안내')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    #[test]
    fn test_v1_to_v2_lossless_migration_on_file_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("legacy_v1_cache.db");
        create_legacy_v1_db(&db_path);

        // Open through Storage::open -> triggers automatic v1 -> v2 migration.
        let storage = Storage::open(&db_path).unwrap();

        // Verify PRAGMA user_version == 2
        {
            let conn = storage.lock_conn().unwrap();
            let version: i32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            assert!(table_exists(&conn, "sites").unwrap());
            assert!(table_has_column(&conn, "issues_fts", "site_id").unwrap());
            assert!(table_has_column(&conn, "pages_fts", "site_id").unwrap());
        }

        // Verify zero data loss and all rows preserved under site_id = 'default'
        let counts = storage.cache_counts_for_site(DEFAULT_SITE_ID).unwrap();
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.boards, 1);
        assert_eq!(counts.issues, 1);
        assert_eq!(counts.spaces, 1);
        assert_eq!(counts.pages, 1);

        let projects = storage.list_projects_for_site(DEFAULT_SITE_ID).unwrap();
        assert_eq!(projects[0].key, "ENG");
        assert!(projects[0].pinned);

        let boards = storage.list_boards_for_site(DEFAULT_SITE_ID, Some("ENG")).unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].id, 1);

        let sprints = storage.list_sprints_for_site(DEFAULT_SITE_ID, 1).unwrap();
        assert_eq!(sprints.len(), 1);
        assert_eq!(sprints[0].id, 10);

        let issues = storage
            .list_issues_for_site(DEFAULT_SITE_ID, Some("ENG"), 10)
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].key, "ENG-1");
        assert_eq!(issues[0].summary, "Legacy v1 issue");

        let filters = storage.list_saved_filters_for_site(DEFAULT_SITE_ID).unwrap();
        assert!(filters.iter().any(|f| f.id == "custom_1" && !f.builtin));
        assert!(filters.iter().any(|f| f.id == "my_open" && f.builtin));

        let spaces = storage.list_spaces_for_site(DEFAULT_SITE_ID).unwrap();
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].key, "DOCS");
        assert!(spaces[0].pinned);

        let pages = storage
            .list_pages_for_site(DEFAULT_SITE_ID, Some("DOCS"), 10)
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "100");

        // Verify cached detail_json survived migration intact
        let migrated_issue_detail = storage
            .get_issue_detail_for_site(DEFAULT_SITE_ID, "ENG-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated_issue_detail.description_markdown,
            "무중단 배포 체크리스트"
        );
        let migrated_page_detail = storage
            .get_page_detail_for_site(DEFAULT_SITE_ID, "100")
            .unwrap()
            .unwrap();
        assert_eq!(migrated_page_detail.body_markdown, "배포 절차 안내");

        // Verify migrated FTS5 & Korean 2-char LIKE search both return site_id = 'default'
        let fts_hits = storage
            .unified_search_scoped("체크리스트", Some(DEFAULT_SITE_ID), 10)
            .unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].0, DEFAULT_SITE_ID);
        assert_eq!(fts_hits[0].1.id_or_key, "ENG-1");

        let ko_hits = storage
            .unified_search_scoped("배포", Some(DEFAULT_SITE_ID), 10)
            .unwrap();
        assert_eq!(ko_hits.len(), 2);
        assert!(ko_hits.iter().all(|(sid, _)| sid == DEFAULT_SITE_ID));

        // Re-opening migrated DB must be idempotent
        drop(storage);
        let reopened = Storage::open(&db_path).unwrap();
        assert_eq!(reopened.cache_counts().unwrap(), counts);
    }
}
