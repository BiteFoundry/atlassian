//! Confluence spaces, hierarchical page tree, page details, and `pages_fts` persistence.

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::{
    adf,
    models::{PageDetail, PageSummary, SearchHitKind, SpaceSummary, UnifiedSearchHit},
    storage::{
        db::{build_contextual_snippet, build_fts_query, escape_like},
        Storage,
    },
};

impl Storage {
    pub fn upsert_spaces(&self, spaces: &[SpaceSummary]) -> Result<()> {
        if spaces.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO spaces (key, id, name, space_type, homepage_id, description, web_url, pinned)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(key) DO UPDATE SET
                    id = excluded.id,
                    name = excluded.name,
                    space_type = excluded.space_type,
                    homepage_id = COALESCE(excluded.homepage_id, spaces.homepage_id),
                    description = COALESCE(excluded.description, spaces.description),
                    web_url = excluded.web_url,
                    pinned = CASE WHEN excluded.pinned != 0 THEN 1 ELSE spaces.pinned END
                ",
            )?;
            for s in spaces {
                stmt.execute(params![
                    s.key,
                    s.id,
                    s.name,
                    s.space_type,
                    s.homepage_id,
                    s.description,
                    s.web_url,
                    if s.pinned { 1i32 } else { 0i32 },
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_spaces(&self) -> Result<Vec<SpaceSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, key, name, space_type, homepage_id, description, web_url, pinned
            FROM spaces
            ORDER BY pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SpaceSummary {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                space_type: row.get(3)?,
                homepage_id: row.get(4)?,
                description: row.get(5)?,
                web_url: row.get(6)?,
                pinned: row.get::<_, i32>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_space_pinned(&self, key: &str, pinned: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE spaces SET pinned = ?1 WHERE key = ?2",
            params![if pinned { 1i32 } else { 0i32 }, key],
        )?;
        Ok(())
    }

    pub fn upsert_pages(&self, pages: &[PageSummary]) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            for page in pages {
                upsert_page_row(&tx, page)?;
                let existing_body: String = tx
                    .query_row(
                        "SELECT body_markdown FROM pages WHERE id = ?1",
                        [&page.id],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                refresh_page_fts(&tx, &page.id, &page.space_key, &page.title, &existing_body)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_page_detail(&self, detail: &PageDetail) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let page = &detail.summary;
            upsert_page_row(&tx, page)?;
            let detail_json = serde_json::to_string(detail).context("PageDetail 직렬화 실패")?;
            tx.execute(
                "UPDATE pages SET body_markdown = ?1, detail_json = ?2 WHERE id = ?3",
                params![detail.body_markdown, detail_json, page.id],
            )?;
            let plain_body = if !detail.blocks.is_empty() {
                adf::blocks_to_plain_text(&detail.blocks)
            } else {
                detail.body_markdown.clone()
            };
            refresh_page_fts(&tx, &page.id, &page.space_key, &page.title, &plain_body)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_page_detail(&self, page_id: &str) -> Result<Option<PageDetail>> {
        let conn = self.lock_conn()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT detail_json FROM pages WHERE id = ?1",
                [page_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match raw {
            Some(json_str) if !json_str.trim().is_empty() => {
                let detail = serde_json::from_str::<PageDetail>(&json_str)
                    .context("PageDetail 역직렬화 실패")?;
                Ok(Some(detail))
            }
            _ => Ok(None),
        }
    }

    pub fn list_space_root_pages(&self, space_key: &str) -> Result<Vec<PageSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, space_id, space_key, parent_id, title, status,
                version_number, author_name, updated_at, position, has_children, web_url
            FROM pages
            WHERE space_key = ?1 AND (parent_id IS NULL OR parent_id = '')
            ORDER BY position ASC, title COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map([space_key], map_page_summary_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_child_pages(&self, parent_id: &str) -> Result<Vec<PageSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, space_id, space_key, parent_id, title, status,
                version_number, author_name, updated_at, position, has_children, web_url
            FROM pages
            WHERE parent_id = ?1
            ORDER BY position ASC, title COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map([parent_id], map_page_summary_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_recent_pages(&self, limit: usize) -> Result<Vec<PageSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, space_id, space_key, parent_id, title, status,
                version_number, author_name, updated_at, position, has_children, web_url
            FROM pages
            ORDER BY updated_at DESC, title COLLATE NOCASE ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map([limit as i64], map_page_summary_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_page(&self, page_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM pages_fts WHERE page_id = ?1", [page_id])?;
        tx.execute("DELETE FROM pages WHERE id = ?1", [page_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn search_pages(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        let mut results: Vec<UnifiedSearchHit> = Vec::new();
        let mut seen = HashSet::<String>::new();

        if trimmed.chars().count() >= 3 {
            if let Some(fts_q) = build_fts_query(trimmed) {
                if let Ok(fts_hits) = run_page_fts_search(&conn, &fts_q, trimmed, limit) {
                    for hit in fts_hits {
                        if seen.insert(hit.id_or_key.clone()) {
                            results.push(hit);
                        }
                    }
                }
            }
        }

        if results.len() < limit {
            let like_hits = run_page_like_search(&conn, trimmed, limit)?;
            for hit in like_hits {
                if seen.insert(hit.id_or_key.clone()) {
                    results.push(hit);
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }

        results.truncate(limit);
        Ok(results)
    }
}

fn upsert_page_row(conn: &Connection, page: &PageSummary) -> Result<()> {
    conn.execute(
        "
        INSERT INTO pages (
            id, space_id, space_key, parent_id, title, status,
            version_number, author_name, updated_at, position, has_children, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11, ?12
        )
        ON CONFLICT(id) DO UPDATE SET
            space_id = excluded.space_id,
            space_key = excluded.space_key,
            parent_id = excluded.parent_id,
            title = excluded.title,
            status = excluded.status,
            version_number = excluded.version_number,
            author_name = COALESCE(excluded.author_name, pages.author_name),
            updated_at = excluded.updated_at,
            position = excluded.position,
            has_children = excluded.has_children,
            web_url = excluded.web_url
        ",
        params![
            page.id,
            page.space_id,
            page.space_key,
            page.parent_id,
            page.title,
            page.status,
            page.version_number,
            page.author_name,
            page.updated_at,
            page.position,
            if page.has_children { 1i32 } else { 0i32 },
            page.web_url,
        ],
    )?;
    Ok(())
}

fn refresh_page_fts(
    conn: &Connection,
    page_id: &str,
    space_key: &str,
    title: &str,
    body_text: &str,
) -> Result<()> {
    conn.execute("DELETE FROM pages_fts WHERE page_id = ?1", [page_id])?;
    conn.execute(
        "
        INSERT INTO pages_fts (page_id, space_key, title, body_text)
        VALUES (?1, ?2, ?3, ?4)
        ",
        params![page_id, space_key, title, body_text],
    )?;
    Ok(())
}

fn map_page_summary_row(row: &Row<'_>) -> rusqlite::Result<PageSummary> {
    Ok(PageSummary {
        id: row.get(0)?,
        space_id: row.get(1)?,
        space_key: row.get(2)?,
        parent_id: row.get(3)?,
        title: row.get(4)?,
        status: row.get(5)?,
        version_number: row.get(6)?,
        author_name: row.get(7)?,
        updated_at: row.get(8)?,
        position: row.get(9)?,
        has_children: row.get::<_, i32>(10)? != 0,
        web_url: row.get(11)?,
    })
}

fn run_page_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    limit: usize,
) -> Result<Vec<UnifiedSearchHit>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            p.id,
            p.space_key,
            p.title,
            p.body_markdown,
            p.version_number,
            p.updated_at,
            p.web_url
        FROM pages_fts f
        JOIN pages p ON p.id = f.page_id
        WHERE pages_fts MATCH ?1
        ORDER BY p.updated_at DESC, p.title COLLATE NOCASE ASC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
        map_page_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn run_page_like_search(
    conn: &Connection,
    raw_query: &str,
    limit: usize,
) -> Result<Vec<UnifiedSearchHit>> {
    let tokens: Vec<String> = raw_query
        .split_whitespace()
        .map(|t| format!("%{}%", escape_like(&t.to_lowercase())))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut clauses = Vec::with_capacity(tokens.len());
    for idx in 0..tokens.len() {
        let p = idx + 1;
        clauses.push(format!(
            "LOWER(id || ' ' || space_key || ' ' || title || ' ' || body_markdown) LIKE ?{p} ESCAPE '\\'"
        ));
    }
    let limit_idx = tokens.len() + 1;
    let sql = format!(
        "
        SELECT
            id,
            space_key,
            title,
            body_markdown,
            version_number,
            updated_at,
            web_url
        FROM pages
        WHERE {}
        ORDER BY updated_at DESC, title COLLATE NOCASE ASC
        LIMIT ?{limit_idx}
        ",
        clauses.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let limit_val = limit as i64;
    let mut dyn_params: Vec<&dyn ToSql> = Vec::with_capacity(tokens.len() + 1);
    for tok in &tokens {
        dyn_params.push(tok);
    }
    dyn_params.push(&limit_val);

    let rows = stmt.query_map(dyn_params.as_slice(), |row| {
        map_page_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_page_hit_row(row: &Row<'_>, raw_query: &str) -> rusqlite::Result<UnifiedSearchHit> {
    let id: String = row.get(0)?;
    let space_key: String = row.get(1)?;
    let title: String = row.get(2)?;
    let body: String = row.get(3)?;
    let version: i32 = row.get(4)?;
    let updated_at: String = row.get(5)?;
    let web_url: String = row.get(6)?;

    let snippet = build_contextual_snippet(&title, &body, &space_key, raw_query, 120);

    Ok(UnifiedSearchHit {
        kind: SearchHitKind::ConfluencePage,
        id_or_key: id,
        container: space_key,
        title,
        snippet,
        status_or_version: format!("v{version}"),
        updated_at,
        web_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page(id: &str, parent_id: Option<&str>, title: &str, pos: i32) -> PageSummary {
        PageSummary {
            id: id.to_string(),
            space_id: "sp-1".into(),
            space_key: "ENG".into(),
            parent_id: parent_id.map(str::to_string),
            title: title.to_string(),
            status: "current".into(),
            version_number: 2,
            author_name: Some("Bob".into()),
            updated_at: "2026-11-15T11:00:00Z".into(),
            position: pos,
            has_children: parent_id.is_none(),
            web_url: format!("https://acme.atlassian.net/wiki/spaces/ENG/pages/{id}"),
        }
    }

    #[test]
    fn test_spaces_pinned_and_page_tree_and_korean_search() {
        let storage = Storage::open_in_memory().unwrap();
        let sp = SpaceSummary {
            id: "sp-1".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            space_type: "global".into(),
            homepage_id: Some("100".into()),
            description: Some("Docs".into()),
            web_url: "https://acme.atlassian.net/wiki/spaces/ENG".into(),
            pinned: false,
        };
        storage.upsert_spaces(&[sp.clone()]).unwrap();
        storage.set_space_pinned("ENG", true).unwrap();
        storage.upsert_spaces(&[sp]).unwrap();
        assert!(storage.list_spaces().unwrap()[0].pinned);

        let root = sample_page("100", None, "아키텍처 가이드", 1);
        let child = sample_page("101", Some("100"), "인증 설계 문서", 2);
        storage.upsert_pages(&[root.clone(), child.clone()]).unwrap();

        let roots = storage.list_space_root_pages("ENG").unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, "100");
        let children = storage.list_child_pages("100").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "101");

        let detail = PageDetail {
            summary: child.clone(),
            ancestors: vec![],
            labels: vec!["security".into()],
            body_raw: String::new(),
            body_representation: "markdown".into(),
            body_markdown: "JWT 토큰 및 배포 체크리스트".into(),
            blocks: vec![],
            toc: vec![],
            attachments: vec![],
            comments: vec![],
            linked_issue_keys: vec![],
        };
        storage.upsert_page_detail(&detail).unwrap();

        // Re-upsert page summary preserves detail_json & body_markdown
        let mut updated_child = child;
        updated_child.version_number = 3;
        storage.upsert_pages(&[updated_child]).unwrap();
        let reloaded = storage.get_page_detail("101").unwrap().unwrap();
        assert_eq!(reloaded.body_markdown, "JWT 토큰 및 배포 체크리스트");

        // 2-char Korean queries ("설계", "배포") & unified search
        assert_eq!(storage.search_pages("설계", 10).unwrap().len(), 1);
        assert_eq!(storage.search_pages("배포", 10).unwrap().len(), 1);
        assert_eq!(storage.search_unified("체크리스트", 10).unwrap().len(), 1);
        assert_eq!(storage.cache_counts().unwrap().pages, 2);

        storage.delete_page("101").unwrap();
        assert!(storage.search_pages("체크리스트", 10).unwrap().is_empty());
    }
}
