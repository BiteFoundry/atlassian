//! SQLite WAL initialization, schema migrations, and FTS5 trigram + Korean `<3` char `LIKE` helpers.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::Storage;

pub fn open_file(path: &Path) -> Result<Storage> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("SQLite 데이터베이스 열기 실패: {}", path.display()))?;
    init_connection(&conn, false)?;
    Ok(Storage {
        conn: Arc::new(Mutex::new(conn)),
        path: Some(path.to_path_buf()),
    })
}

pub fn open_memory() -> Result<Storage> {
    let conn = Connection::open_in_memory().context("인메모리 SQLite 열기 실패")?;
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
    .context("SQLite PRAGMA 설정 실패")?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS projects (
            key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            name TEXT NOT NULL,
            project_type TEXT NOT NULL,
            lead_name TEXT,
            avatar_url TEXT,
            pinned INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS boards (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            board_type TEXT NOT NULL,
            project_key TEXT,
            project_name TEXT,
            columns_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sprints (
            id INTEGER PRIMARY KEY,
            board_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            state TEXT NOT NULL,
            start_date TEXT,
            end_date TEXT,
            goal TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_sprints_board
            ON sprints(board_id, id DESC);

        CREATE TABLE IF NOT EXISTS issues (
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

        CREATE INDEX IF NOT EXISTS idx_issues_project_updated
            ON issues(project_key, updated_at DESC);

        CREATE INDEX IF NOT EXISTS idx_issues_board_sprint
            ON issues(board_id, sprint_id, updated_at DESC);

        CREATE TABLE IF NOT EXISTS saved_filters (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            jql TEXT NOT NULL,
            builtin INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS spaces (
            key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            name TEXT NOT NULL,
            space_type TEXT NOT NULL,
            homepage_id TEXT,
            description TEXT,
            web_url TEXT NOT NULL,
            pinned INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS pages (
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

        CREATE INDEX IF NOT EXISTS idx_pages_space_parent_pos
            ON pages(space_key, parent_id, position ASC, title ASC);

        CREATE INDEX IF NOT EXISTS idx_pages_updated
            ON pages(updated_at DESC);
        ",
    )
    .context("SQLite 기본 스키마 생성 실패")?;

    ensure_fts5_table(
        conn,
        "issues_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS issues_fts USING fts5(
            issue_key UNINDEXED,
            project_key,
            summary,
            description_text,
            labels_text,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS issues_fts USING fts5(
            issue_key UNINDEXED,
            project_key,
            summary,
            description_text,
            labels_text,
            tokenize='unicode61'
        );
        ",
    )?;

    ensure_fts5_table(
        conn,
        "pages_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            page_id UNINDEXED,
            space_key,
            title,
            body_text,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            page_id UNINDEXED,
            space_key,
            title,
            body_text,
            tokenize='unicode61'
        );
        ",
    )?;

    seed_builtin_filters(conn)?;
    Ok(())
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql)
            .with_context(|| format!("FTS5 가상 테이블 생성 실패 ({table_name}, unicode61)"))?;
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
            "내게 할당된 미해결",
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC",
        ),
        (
            "reported",
            "내가 보고한 이슈",
            "reporter = currentUser() ORDER BY updated DESC",
        ),
        (
            "recent",
            "최근 업데이트 (7일)",
            "updated >= -7d ORDER BY updated DESC",
        ),
        (
            "all_open",
            "미해결 전체",
            "statusCategory != Done ORDER BY updated DESC",
        ),
    ];

    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO saved_filters (id, name, jql, builtin) VALUES (?1, ?2, ?3, 1)",
    )?;
    for (id, name, jql) in builtins {
        stmt.execute(params![id, name, jql])?;
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
