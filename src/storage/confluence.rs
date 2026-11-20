//! Confluence spaces, hierarchical page tree, page details, and `pages_fts` persistence (multi-site aware).

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::{
    adf,
    models::{PageDetail, PageSummary, SearchHitKind, SpaceSummary, UnifiedSearchHit},
    storage::{
        db::{build_contextual_snippet, build_fts_query, escape_like, DEFAULT_SITE_ID},
        Storage,
    },
};

impl Storage {
    pub fn upsert_spaces(&self, spaces: &[SpaceSummary]) -> Result<()> {
        self.upsert_spaces_for_site(DEFAULT_SITE_ID, spaces)
    }

    pub fn upsert_spaces_for_site(&self, site_id: &str, spaces: &[SpaceSummary]) -> Result<()> {
        if spaces.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO spaces (site_id, key, id, name, space_type, homepage_id, description, web_url, pinned)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(site_id, key) DO UPDATE SET
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
                    site_id,
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
        self.list_spaces_for_site(DEFAULT_SITE_ID)
    }

    pub fn list_spaces_for_site(&self, site_id: &str) -> Result<Vec<SpaceSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, key, name, space_type, homepage_id, description, web_url, pinned
            FROM spaces
            WHERE site_id = ?1
            ORDER BY pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([site_id], |row| map_space_row(row, 0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_spaces_all_sites(&self) -> Result<Vec<(String, SpaceSummary)>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT site_id, id, key, name, space_type, homepage_id, description, web_url, pinned
            FROM spaces
            ORDER BY site_id ASC, pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let site_id: String = row.get(0)?;
            let space = map_space_row(row, 1)?;
            Ok((site_id, space))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_space_pinned(&self, key: &str, pinned: bool) -> Result<()> {
        self.set_space_pinned_for_site(DEFAULT_SITE_ID, key, pinned)
    }

    pub fn set_space_pinned_for_site(
        &self,
        site_id: &str,
        key: &str,
        pinned: bool,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE spaces SET pinned = ?1 WHERE site_id = ?2 AND key = ?3",
            params![if pinned { 1i32 } else { 0i32 }, site_id, key],
        )?;
        Ok(())
    }

    pub fn upsert_pages(&self, pages: &[PageSummary]) -> Result<()> {
        self.upsert_pages_for_site(DEFAULT_SITE_ID, pages)
    }

    pub fn upsert_pages_for_site(&self, site_id: &str, pages: &[PageSummary]) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            for page in pages {
                let applied = upsert_page_row(&tx, site_id, page)?;
                if !applied {
                    continue;
                }
                let existing_body: String = tx
                    .query_row(
                        "SELECT body_markdown FROM pages WHERE site_id = ?1 AND id = ?2",
                        params![site_id, &page.id],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                refresh_page_fts(
                    &tx,
                    site_id,
                    &page.id,
                    &page.space_key,
                    &page.title,
                    &existing_body,
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_page_detail(&self, detail: &PageDetail) -> Result<()> {
        self.upsert_page_detail_for_site(DEFAULT_SITE_ID, detail)
    }

    pub fn upsert_page_detail_for_site(&self, site_id: &str, detail: &PageDetail) -> Result<()> {
        let mut owned = detail.clone();
        self.upsert_page_detail_for_site_merging(site_id, &mut owned, false, false, false)
    }

    /// Upserts a Confluence page detail for `site_id`, enforcing monotonic `version_number` and
    /// optionally preserving previously cached labels, attachments, or comments when those
    /// subresource calls fail transiently.
    pub fn upsert_page_detail_for_site_merging(
        &self,
        site_id: &str,
        detail: &mut PageDetail,
        preserve_labels: bool,
        preserve_attachments: bool,
        preserve_comments: bool,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let existing_detail_raw: Option<String> = tx
                .query_row(
                    "SELECT detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                    params![site_id, &detail.summary.id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();

            let applied = upsert_page_row(&tx, site_id, &detail.summary)?;
            if !applied {
                if let Some(raw) = existing_detail_raw.as_deref() {
                    if let Ok(newer) = serde_json::from_str::<PageDetail>(raw) {
                        *detail = newer;
                    }
                }
                tx.commit()?;
                return Ok(());
            }

            if preserve_labels || preserve_attachments || preserve_comments {
                if let Some(raw) = existing_detail_raw.as_deref() {
                    if let Ok(prev) = serde_json::from_str::<PageDetail>(raw) {
                        if preserve_labels && detail.labels.is_empty() {
                            detail.labels = prev.labels;
                        }
                        if preserve_attachments && detail.attachments.is_empty() {
                            detail.attachments = prev.attachments;
                        }
                        if preserve_comments && detail.comments.is_empty() {
                            detail.comments = prev.comments;
                        }
                    }
                }
            }

            let page = &detail.summary;
            let detail_json =
                serde_json::to_string(detail).context("Failed to serialize PageDetail")?;
            tx.execute(
                "UPDATE pages SET body_markdown = ?1, detail_json = ?2, detail_version = ?3 WHERE site_id = ?4 AND id = ?5",
                params![
                    detail.body_markdown,
                    detail_json,
                    page.version_number,
                    site_id,
                    page.id
                ],
            )?;
            let plain_body = if !detail.blocks.is_empty() {
                adf::blocks_to_plain_text(&detail.blocks)
            } else {
                detail.body_markdown.clone()
            };
            refresh_page_fts(
                &tx,
                site_id,
                &page.id,
                &page.space_key,
                &page.title,
                &plain_body,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_page_detail(&self, page_id: &str) -> Result<Option<PageDetail>> {
        self.get_page_detail_for_site(DEFAULT_SITE_ID, page_id)
    }

    pub fn get_page_detail_for_site(
        &self,
        site_id: &str,
        page_id: &str,
    ) -> Result<Option<PageDetail>> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(PageSummary, Option<String>)> = conn
            .query_row(
                "
                SELECT
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url,
                    detail_json
                FROM pages
                WHERE site_id = ?1 AND id = ?2
                ",
                params![site_id, page_id],
                |row| {
                    let summary = map_page_summary_row(row, 0)?;
                    let detail_json: Option<String> = row.get(12)?;
                    Ok((summary, detail_json))
                },
            )
            .optional()?;
        match row_opt {
            Some((summary, Some(json_str))) if !json_str.trim().is_empty() => {
                let mut detail = serde_json::from_str::<PageDetail>(&json_str)
                    .context("Failed to deserialize PageDetail")?;
                // Reconcile summary metadata with authoritative table columns so list and
                // offline reader views never split-brain after list/sync updates.
                detail.summary = summary;
                Ok(Some(detail))
            }
            _ => Ok(None),
        }
    }

    /// Returns `true` when cached `detail_json` was captured at an older `version_number` than
    /// the latest list/sync summary row for `(site_id, page_id)`.
    pub fn is_page_detail_stale_for_site(&self, site_id: &str, page_id: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(i32, Option<i32>, Option<String>)> = conn
            .query_row(
                "SELECT version_number, detail_version, detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((row_ver, detail_ver, detail_json)) = row_opt else {
            return Ok(false);
        };
        if detail_json.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Ok(false);
        }
        Ok(detail_ver.is_some_and(|dv| dv < row_ver))
    }

    /// Removes cached root pages in `(site_id, space_key)` that are absent from a complete
    /// root-page listing snapshot. Never touches child pages or other spaces/sites.
    pub fn prune_space_root_pages_for_site(
        &self,
        site_id: &str,
        space_key: &str,
        keep_ids: &[String],
    ) -> Result<usize> {
        let keep: HashSet<&str> = keep_ids.iter().map(String::as_str).collect();
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let existing_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM pages WHERE site_id = ?1 AND space_key = ?2 AND (parent_id IS NULL OR parent_id = '')",
            )?;
            let rows = stmt.query_map(params![site_id, space_key], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut removed = 0usize;
        for pid in existing_ids {
            if !keep.contains(pid.as_str()) {
                tx.execute(
                    "DELETE FROM pages_fts WHERE site_id = ?1 AND page_id = ?2",
                    params![site_id, &pid],
                )?;
                tx.execute(
                    "DELETE FROM pages WHERE site_id = ?1 AND id = ?2",
                    params![site_id, &pid],
                )?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Removes cached child pages under `(site_id, parent_id)` absent from a complete child listing.
    pub fn prune_child_pages_for_site(
        &self,
        site_id: &str,
        parent_id: &str,
        keep_ids: &[String],
    ) -> Result<usize> {
        let keep: HashSet<&str> = keep_ids.iter().map(String::as_str).collect();
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let existing_ids: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT id FROM pages WHERE site_id = ?1 AND parent_id = ?2")?;
            let rows = stmt.query_map(params![site_id, parent_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut removed = 0usize;
        for pid in existing_ids {
            if !keep.contains(pid.as_str()) {
                tx.execute(
                    "DELETE FROM pages_fts WHERE site_id = ?1 AND page_id = ?2",
                    params![site_id, &pid],
                )?;
                tx.execute(
                    "DELETE FROM pages WHERE site_id = ?1 AND id = ?2",
                    params![site_id, &pid],
                )?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn list_pages_for_site(
        &self,
        site_id: &str,
        space_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        if let Some(sk) = space_key {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url
                FROM pages
                WHERE site_id = ?1 AND space_key = ?2
                ORDER BY updated_at DESC, title COLLATE NOCASE ASC, id ASC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(params![site_id, sk, limit as i64], |row| {
                map_page_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url
                FROM pages
                WHERE site_id = ?1
                ORDER BY updated_at DESC, title COLLATE NOCASE ASC, id ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![site_id, limit as i64], |row| {
                map_page_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn list_pages_all_sites(
        &self,
        space_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, PageSummary)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        if let Some(sk) = space_key {
            let mut stmt = conn.prepare(
                "
                SELECT
                    site_id,
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url
                FROM pages
                WHERE space_key = ?1
                ORDER BY updated_at DESC, site_id ASC, title COLLATE NOCASE ASC, id ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![sk, limit as i64], |row| {
                let site_id: String = row.get(0)?;
                let page = map_page_summary_row(row, 1)?;
                Ok((site_id, page))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT
                    site_id,
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url
                FROM pages
                ORDER BY updated_at DESC, site_id ASC, title COLLATE NOCASE ASC, id ASC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit as i64], |row| {
                let site_id: String = row.get(0)?;
                let page = map_page_summary_row(row, 1)?;
                Ok((site_id, page))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn list_space_root_pages(&self, space_key: &str) -> Result<Vec<PageSummary>> {
        self.list_space_root_pages_for_site(DEFAULT_SITE_ID, space_key)
    }

    pub fn list_space_root_pages_for_site(
        &self,
        site_id: &str,
        space_key: &str,
    ) -> Result<Vec<PageSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, space_id, space_key, parent_id, title, status,
                version_number, author_name, updated_at, position, has_children, web_url
            FROM pages
            WHERE site_id = ?1 AND space_key = ?2 AND (parent_id IS NULL OR parent_id = '')
            ORDER BY position ASC, title COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map(params![site_id, space_key], |row| {
            map_page_summary_row(row, 0)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_child_pages(&self, parent_id: &str) -> Result<Vec<PageSummary>> {
        self.list_child_pages_for_site(DEFAULT_SITE_ID, parent_id)
    }

    pub fn list_child_pages_for_site(
        &self,
        site_id: &str,
        parent_id: &str,
    ) -> Result<Vec<PageSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, space_id, space_key, parent_id, title, status,
                version_number, author_name, updated_at, position, has_children, web_url
            FROM pages
            WHERE site_id = ?1 AND parent_id = ?2
            ORDER BY position ASC, title COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map(params![site_id, parent_id], |row| {
            map_page_summary_row(row, 0)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_recent_pages(&self, limit: usize) -> Result<Vec<PageSummary>> {
        self.list_recent_pages_for_site(DEFAULT_SITE_ID, limit)
    }

    pub fn list_recent_pages_for_site(
        &self,
        site_id: &str,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        self.list_pages_for_site(site_id, None, limit)
    }

    pub fn list_recent_pages_all_sites(&self, limit: usize) -> Result<Vec<(String, PageSummary)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                p.site_id,
                p.id, p.space_id, p.space_key, p.parent_id, p.title, p.status,
                p.version_number, p.author_name, p.updated_at, p.position, p.has_children, p.web_url
            FROM pages p
            WHERE NOT EXISTS (
                SELECT 1 FROM sites s WHERE s.site_id = p.site_id AND s.enabled = 0
            )
            ORDER BY p.updated_at DESC, p.site_id ASC, p.title COLLATE NOCASE ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let site_id: String = row.get(0)?;
            let page = map_page_summary_row(row, 1)?;
            Ok((site_id, page))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_page(&self, page_id: &str) -> Result<()> {
        self.delete_page_for_site(DEFAULT_SITE_ID, page_id)
    }

    pub fn delete_page_for_site(&self, site_id: &str, page_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM pages_fts WHERE site_id = ?1 AND page_id = ?2",
            params![site_id, page_id],
        )?;
        tx.execute(
            "DELETE FROM pages WHERE site_id = ?1 AND id = ?2",
            params![site_id, page_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn search_pages(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        let scoped = self.search_pages_scoped(query, None, limit)?;
        Ok(scoped.into_iter().map(|(_, hit)| hit).collect())
    }

    pub fn search_pages_scoped(
        &self,
        query: &str,
        site_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, UnifiedSearchHit)>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        let mut results: Vec<(String, UnifiedSearchHit)> = Vec::new();
        let mut seen = HashSet::<(String, String)>::new();

        if trimmed.chars().count() >= 3 {
            if let Some(fts_q) = build_fts_query(trimmed) {
                if let Ok(fts_hits) = run_page_fts_search(&conn, &fts_q, trimmed, site_id, limit) {
                    for (sid, hit) in fts_hits {
                        if seen.insert((sid.clone(), hit.id_or_key.clone())) {
                            results.push((sid, hit));
                        }
                    }
                }
            }
        }

        if results.len() < limit {
            let like_hits = run_page_like_search(&conn, trimmed, site_id, limit)?;
            for (sid, hit) in like_hits {
                if seen.insert((sid.clone(), hit.id_or_key.clone())) {
                    results.push((sid, hit));
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

fn map_space_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<SpaceSummary> {
    Ok(SpaceSummary {
        id: row.get(offset)?,
        key: row.get(offset + 1)?,
        name: row.get(offset + 2)?,
        space_type: row.get(offset + 3)?,
        homepage_id: row.get(offset + 4)?,
        description: row.get(offset + 5)?,
        web_url: row.get(offset + 6)?,
        pinned: row.get::<_, i32>(offset + 7)? != 0,
    })
}

fn upsert_page_row(conn: &Connection, site_id: &str, page: &PageSummary) -> Result<bool> {
    if let Some((existing_ver, existing_updated)) = conn
        .query_row(
            "SELECT version_number, updated_at FROM pages WHERE site_id = ?1 AND id = ?2",
            params![site_id, &page.id],
            |row| Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if page.version_number < existing_ver {
            return Ok(false);
        }
        if page.version_number == existing_ver
            && !existing_updated.trim().is_empty()
            && !page.updated_at.trim().is_empty()
            && crate::storage::jira::compare_updated_at(&page.updated_at, &existing_updated)
                == std::cmp::Ordering::Less
        {
            return Ok(false);
        }
    }

    conn.execute(
        "
        INSERT INTO pages (
            site_id, id, space_id, space_key, parent_id, title, status,
            version_number, author_name, updated_at, position, has_children, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12, ?13
        )
        ON CONFLICT(site_id, id) DO UPDATE SET
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
            site_id,
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
    Ok(true)
}

fn refresh_page_fts(
    conn: &Connection,
    site_id: &str,
    page_id: &str,
    space_key: &str,
    title: &str,
    body_text: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM pages_fts WHERE site_id = ?1 AND page_id = ?2",
        params![site_id, page_id],
    )?;
    conn.execute(
        "
        INSERT INTO pages_fts (site_id, page_id, space_key, title, body_text)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
        params![site_id, page_id, space_key, title, body_text],
    )?;
    Ok(())
}

fn map_page_summary_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<PageSummary> {
    Ok(PageSummary {
        id: row.get(offset)?,
        space_id: row.get(offset + 1)?,
        space_key: row.get(offset + 2)?,
        parent_id: row.get(offset + 3)?,
        title: row.get(offset + 4)?,
        status: row.get(offset + 5)?,
        version_number: row.get(offset + 6)?,
        author_name: row.get(offset + 7)?,
        updated_at: row.get(offset + 8)?,
        position: row.get(offset + 9)?,
        has_children: row.get::<_, i32>(offset + 10)? != 0,
        web_url: row.get(offset + 11)?,
    })
}

fn run_page_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    site_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, UnifiedSearchHit)>> {
    if let Some(sid) = site_id {
        let mut stmt = conn.prepare(
            "
            SELECT
                p.site_id,
                p.id,
                p.space_key,
                p.title,
                p.body_markdown,
                p.version_number,
                p.updated_at,
                p.web_url
            FROM pages_fts f
            JOIN pages p ON p.site_id = f.site_id AND p.id = f.page_id
            WHERE pages_fts MATCH ?1 AND f.site_id = ?2
            ORDER BY p.updated_at DESC, p.site_id ASC, p.title COLLATE NOCASE ASC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, sid, limit as i64], |row| {
            map_page_hit_row(row, raw_query)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT
                p.site_id,
                p.id,
                p.space_key,
                p.title,
                p.body_markdown,
                p.version_number,
                p.updated_at,
                p.web_url
            FROM pages_fts f
            JOIN pages p ON p.site_id = f.site_id AND p.id = f.page_id
            WHERE pages_fts MATCH ?1
            ORDER BY p.updated_at DESC, p.site_id ASC, p.title COLLATE NOCASE ASC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_page_hit_row(row, raw_query)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn run_page_like_search(
    conn: &Connection,
    raw_query: &str,
    site_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, UnifiedSearchHit)>> {
    let tokens: Vec<String> = raw_query
        .split_whitespace()
        .map(|t| format!("%{}%", escape_like(&t.to_lowercase())))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut clauses = Vec::with_capacity(tokens.len() + 1);
    for idx in 0..tokens.len() {
        let p = idx + 1;
        clauses.push(format!(
            "LOWER(id || ' ' || space_key || ' ' || title || ' ' || body_markdown) LIKE ?{p} ESCAPE '\\'"
        ));
    }

    let mut next_param = tokens.len() + 1;
    if site_id.is_some() {
        clauses.push(format!("site_id = ?{next_param}"));
        next_param += 1;
    }
    let limit_idx = next_param;

    let sql = format!(
        "
        SELECT
            site_id,
            id,
            space_key,
            title,
            body_markdown,
            version_number,
            updated_at,
            web_url
        FROM pages
        WHERE {}
        ORDER BY updated_at DESC, site_id ASC, title COLLATE NOCASE ASC
        LIMIT ?{limit_idx}
        ",
        clauses.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let limit_val = limit as i64;
    let mut dyn_params: Vec<&dyn ToSql> = Vec::with_capacity(tokens.len() + 2);
    for tok in &tokens {
        dyn_params.push(tok);
    }
    if let Some(ref sid) = site_id {
        dyn_params.push(sid);
    }
    dyn_params.push(&limit_val);

    let rows = stmt.query_map(dyn_params.as_slice(), |row| {
        map_page_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_page_hit_row(
    row: &Row<'_>,
    raw_query: &str,
) -> rusqlite::Result<(String, UnifiedSearchHit)> {
    let site_id: String = row.get(0)?;
    let id: String = row.get(1)?;
    let space_key: String = row.get(2)?;
    let title: String = row.get(3)?;
    let body: String = row.get(4)?;
    let version: i32 = row.get(5)?;
    let updated_at: String = row.get(6)?;
    let web_url: String = row.get(7)?;

    let snippet = build_contextual_snippet(&title, &body, &space_key, raw_query, 120);

    Ok((
        site_id,
        UnifiedSearchHit {
            kind: SearchHitKind::ConfluencePage,
            id_or_key: id,
            container: space_key,
            title,
            snippet,
            status_or_version: format!("v{version}"),
            updated_at,
            web_url,
        },
    ))
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

    #[test]
    fn test_monotonic_version_split_brain_subresource_preserve_and_prune() {
        let storage = Storage::open_in_memory().unwrap();
        let mut p1 = sample_page("2001", None, "Runbook v1", 1);
        p1.version_number = 1;
        p1.updated_at = "2026-11-15T10:00:00Z".into();

        let d1 = PageDetail {
            summary: p1.clone(),
            labels: vec!["runbook".into()],
            body_markdown: "Runbook body v1".into(),
            ..Default::default()
        };
        storage.upsert_page_detail_for_site("site-a", &d1).unwrap();
        assert!(!storage
            .is_page_detail_stale_for_site("site-a", "2001")
            .unwrap());

        // Newer v2 summary updates title/version and marks detail stale
        let mut p2 = p1.clone();
        p2.title = "Runbook v2".into();
        p2.version_number = 2;
        p2.updated_at = "2026-11-17T10:00:00Z".into();
        storage.upsert_pages_for_site("site-a", &[p2.clone()]).unwrap();

        assert!(storage
            .is_page_detail_stale_for_site("site-a", "2001")
            .unwrap());
        let reconciled = storage
            .get_page_detail_for_site("site-a", "2001")
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.summary.title, "Runbook v2");
        assert_eq!(reconciled.summary.version_number, 2);

        // Stale v1 upsert must NOT regress v2
        storage.upsert_pages_for_site("site-a", &[p1]).unwrap();
        let list = storage
            .list_space_root_pages_for_site("site-a", "ENG")
            .unwrap();
        assert_eq!(list[0].version_number, 2);
        assert_eq!(list[0].title, "Runbook v2");

        // Subresource failure preserves cached labels
        let mut d2 = PageDetail {
            summary: p2.clone(),
            labels: vec![],
            body_markdown: "Runbook body v2".into(),
            ..Default::default()
        };
        storage
            .upsert_page_detail_for_site_merging("site-a", &mut d2, true, true, true)
            .unwrap();
        assert_eq!(d2.labels, vec!["runbook".to_string()]);
        assert!(!storage
            .is_page_detail_stale_for_site("site-a", "2001")
            .unwrap());

        // Pruning root pages on site-a removes 2001 while preserving site-b
        storage.upsert_pages_for_site("site-b", &[p2]).unwrap();
        let removed = storage
            .prune_space_root_pages_for_site("site-a", "ENG", &[])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(storage
            .get_page_detail_for_site("site-a", "2001")
            .unwrap()
            .is_none());
        assert_eq!(
            storage
                .list_space_root_pages_for_site("site-b", "ENG")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_site_scoped_page_tree_and_delete_isolation() {
        let storage = Storage::open_in_memory().unwrap();
        let acme_root = sample_page("100", None, "Acme Root", 1);
        let acme_child = sample_page("101", Some("100"), "Acme Child", 2);
        let corp_root = sample_page("100", None, "Corp Root", 1);
        let corp_child = sample_page("101", Some("100"), "Corp Child", 2);

        storage
            .upsert_pages_for_site("acme-cloud", &[acme_root, acme_child])
            .unwrap();
        storage
            .upsert_pages_for_site("corp-dc", &[corp_root, corp_child])
            .unwrap();

        assert_eq!(
            storage
                .list_space_root_pages_for_site("acme-cloud", "ENG")
                .unwrap()[0]
                .title,
            "Acme Root"
        );
        assert_eq!(
            storage
                .list_child_pages_for_site("corp-dc", "100")
                .unwrap()[0]
                .title,
            "Corp Child"
        );
        assert_eq!(
            storage
                .list_pages_for_site("acme-cloud", Some("ENG"), 10)
                .unwrap()
                .len(),
            2
        );

        // Delete page 101 only on acme-cloud
        storage.delete_page_for_site("acme-cloud", "101").unwrap();
        assert!(storage
            .list_child_pages_for_site("acme-cloud", "100")
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .list_child_pages_for_site("corp-dc", "100")
                .unwrap()
                .len(),
            1
        );
    }
}
