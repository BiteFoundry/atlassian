//! Confluence spaces, hierarchical page tree, page details, and `pages_fts` persistence (multi-site aware).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::{
    adf,
    models::{
        AttachmentItem, ConfluenceComment, ListPage, PageDetail, PageSummary, SearchHitKind,
        SpaceSummary, UnifiedSearchHit,
    },
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
        self.upsert_pages_for_site_with_mode(
            site_id,
            pages,
            ParentUpdateMode::PreserveExistingOnNone,
        )
    }

    /// Upserts summaries whose `parent_id` is authoritative (e.g., space root listings or full
    /// revision probes), allowing `None` to clear an outdated `parent_id`.
    pub fn upsert_authoritative_pages_for_site(
        &self,
        site_id: &str,
        pages: &[PageSummary],
    ) -> Result<()> {
        self.upsert_pages_for_site_with_mode(site_id, pages, ParentUpdateMode::Authoritative)
    }

    fn upsert_pages_for_site_with_mode(
        &self,
        site_id: &str,
        pages: &[PageSummary],
        parent_mode: ParentUpdateMode,
    ) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            for page in pages {
                let outcome = upsert_page_row(&tx, site_id, page, parent_mode)?;
                if !outcome.applied && !outcome.fts_needs_refresh {
                    continue;
                }
                let (eff_space_key, eff_title, existing_body): (String, String, String) = tx
                    .query_row(
                        "SELECT space_key, title, body_markdown FROM pages WHERE site_id = ?1 AND id = ?2",
                        params![site_id, &page.id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap_or_else(|_| (page.space_key.clone(), page.title.clone(), String::new()));
                refresh_page_fts(
                    &tx,
                    site_id,
                    &page.id,
                    &eff_space_key,
                    &eff_title,
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
        self.upsert_page_detail_for_site_merging_inner(
            site_id,
            detail,
            preserve_labels,
            preserve_attachments,
            preserve_comments,
            false,
        )
    }

    /// Upserts a Confluence page detail using [`PageSubresourceStatus`] flags to preserve
    /// previously cached labels, attachments, comments, or ancestors when any subresource failed.
    pub fn upsert_page_detail_for_site_with_meta(
        &self,
        site_id: &str,
        detail: &mut PageDetail,
        meta: crate::atlassian::confluence::PageSubresourceStatus,
    ) -> Result<()> {
        self.upsert_page_detail_for_site_merging_inner(
            site_id,
            detail,
            !meta.labels_ok,
            !meta.attachments_ok,
            !meta.comments_ok,
            !meta.ancestors_ok,
        )
    }

    fn upsert_page_detail_for_site_merging_inner(
        &self,
        site_id: &str,
        detail: &mut PageDetail,
        preserve_labels: bool,
        preserve_attachments: bool,
        preserve_comments: bool,
        preserve_ancestors: bool,
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

            let outcome = upsert_page_row(
                &tx,
                site_id,
                &detail.summary,
                ParentUpdateMode::Authoritative,
            )?;
            if !outcome.applied {
                if let Some(raw) = existing_detail_raw.as_deref() {
                    if let Ok(mut newer) = serde_json::from_str::<PageDetail>(raw) {
                        if let Ok(reconciled_summary) = tx.query_row(
                            "
                            SELECT
                                id, space_id, space_key, parent_id, title, status,
                                version_number, author_name, updated_at, position, has_children, web_url
                            FROM pages
                            WHERE site_id = ?1 AND id = ?2
                            ",
                            params![site_id, &detail.summary.id],
                            |row| map_page_summary_row(row, 0),
                        ) {
                            newer.summary = reconciled_summary;
                        }
                        *detail = newer;
                    }
                }
                if outcome.fts_needs_refresh {
                    if let Ok((eff_space_key, eff_title, existing_body)) = tx.query_row(
                        "SELECT space_key, title, body_markdown FROM pages WHERE site_id = ?1 AND id = ?2",
                        params![site_id, &detail.summary.id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                    ) {
                        refresh_page_fts(
                            &tx,
                            site_id,
                            &detail.summary.id,
                            &eff_space_key,
                            &eff_title,
                            &existing_body,
                        )?;
                    }
                }
                tx.commit()?;
                return Ok(());
            }

            if let Ok(reconciled_summary) = tx.query_row(
                "
                SELECT
                    id, space_id, space_key, parent_id, title, status,
                    version_number, author_name, updated_at, position, has_children, web_url
                FROM pages
                WHERE site_id = ?1 AND id = ?2
                ",
                params![site_id, &detail.summary.id],
                |row| map_page_summary_row(row, 0),
            ) {
                detail.summary = reconciled_summary;
            }

            if preserve_labels || preserve_attachments || preserve_comments || preserve_ancestors {
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
                        if preserve_ancestors && detail.ancestors.is_empty() {
                            detail.ancestors = prev.ancestors;
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

    /// Looks up cached non-empty page titles for `(site_id, ids)`.
    pub fn get_page_titles_for_site(
        &self,
        site_id: &str,
        ids: &[String],
    ) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT title FROM pages WHERE site_id = ?1 AND id = ?2",
        )?;
        let mut titles = HashMap::new();
        for id in ids {
            if let Some(title) = stmt
                .query_row(params![site_id, id], |row| row.get::<_, String>(0))
                .optional()?
            {
                let trimmed = title.trim();
                if !trimmed.is_empty() {
                    titles.insert(id.clone(), trimmed.to_string());
                }
            }
        }
        Ok(titles)
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
        let Some(raw_json) = detail_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(false);
        };
        let effective_detail_ver = detail_ver.or_else(|| {
            serde_json::from_str::<PageDetail>(raw_json)
                .ok()
                .map(|d| d.summary.version_number)
        });
        Ok(effective_detail_ver.is_some_and(|dv| dv < row_ver))
    }

    /// Returns the `version_number` captured when `detail_json` was actually fetched
    /// (independent of any newer list/sync summary row reconciled on read).
    pub fn get_page_detail_version_for_site(
        &self,
        site_id: &str,
        page_id: &str,
    ) -> Result<Option<i32>> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(Option<i32>, Option<String>)> = conn
            .query_row(
                "SELECT detail_version, detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((detail_ver, detail_json)) = row_opt else {
            return Ok(None);
        };
        let Some(raw_json) = detail_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        Ok(detail_ver.or_else(|| {
            serde_json::from_str::<PageDetail>(raw_json)
                .ok()
                .map(|d| d.summary.version_number)
        }))
    }

    /// Returns the latest summary row `version_number` stored for `(site_id, page_id)`.
    pub fn get_page_row_version_for_site(
        &self,
        site_id: &str,
        page_id: &str,
    ) -> Result<Option<i32>> {
        let conn = self.lock_conn()?;
        let ver: Option<i32> = conn
            .query_row(
                "SELECT version_number FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ver)
    }

    /// Updates cached `comments` inside `detail_json` for `(site_id, page_id)`.
    pub fn update_page_comments_for_site(
        &self,
        site_id: &str,
        page_id: &str,
        comments: &[ConfluenceComment],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<PageDetail>(&raw) {
                detail.comments = comments.to_vec();
                let updated_json = serde_json::to_string(&detail)?;
                tx.execute(
                    "UPDATE pages SET detail_json = ?1 WHERE site_id = ?2 AND id = ?3",
                    params![updated_json, site_id, page_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Updates cached `attachments` inside `detail_json` for `(site_id, page_id)`.
    pub fn update_page_attachments_for_site(
        &self,
        site_id: &str,
        page_id: &str,
        attachments: &[AttachmentItem],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<PageDetail>(&raw) {
                detail.attachments = attachments.to_vec();
                let updated_json = serde_json::to_string(&detail)?;
                tx.execute(
                    "UPDATE pages SET detail_json = ?1 WHERE site_id = ?2 AND id = ?3",
                    params![updated_json, site_id, page_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Appends a newly posted Confluence comment into cached `detail_json` (if present).
    pub fn append_page_comment_for_site(
        &self,
        site_id: &str,
        page_id: &str,
        comment: &ConfluenceComment,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<PageDetail>(&raw) {
                let exists = !comment.id.is_empty()
                    && detail.comments.iter().any(|existing| existing.id == comment.id);
                if !exists {
                    detail.comments.push(comment.clone());
                    let updated_json = serde_json::to_string(&detail)?;
                    tx.execute(
                        "UPDATE pages SET detail_json = ?1 WHERE site_id = ?2 AND id = ?3",
                        params![updated_json, site_id, page_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Appends newly uploaded Confluence attachments into cached `detail_json` (if present).
    pub fn append_page_attachments_for_site(
        &self,
        site_id: &str,
        page_id: &str,
        new_atts: &[AttachmentItem],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM pages WHERE site_id = ?1 AND id = ?2",
                params![site_id, page_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<PageDetail>(&raw) {
                let mut changed = false;
                for att in new_atts {
                    let exists = !att.id.is_empty()
                        && detail.attachments.iter().any(|existing| existing.id == att.id);
                    if !exists {
                        detail.attachments.push(att.clone());
                        changed = true;
                    }
                }
                if changed {
                    let updated_json = serde_json::to_string(&detail)?;
                    tx.execute(
                        "UPDATE pages SET detail_json = ?1 WHERE site_id = ?2 AND id = ?3",
                        params![updated_json, site_id, page_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes cached root pages in `(site_id, space_key)` that are absent from a complete
    /// root-page listing snapshot, together with every page cached below them, and returns the
    /// number of page rows removed. The space key is matched case-insensitively. Pages in
    /// `keep_ids` are never removed. Never touches other spaces/sites.
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
                "SELECT id FROM pages WHERE site_id = ?1 AND space_key = ?2 COLLATE NOCASE AND (parent_id IS NULL OR parent_id = '')",
            )?;
            let rows = stmt.query_map(params![site_id, space_key], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let doomed: Vec<String> = existing_ids
            .into_iter()
            .filter(|pid| !keep.contains(pid.as_str()))
            .collect();
        let removed = delete_pages_with_descendants(&tx, site_id, doomed, &keep)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Removes cached child pages under `(site_id, parent_id)` absent from a complete child
    /// listing, together with every page cached below them, and returns the number of page rows
    /// removed. Pages in `keep_ids` and `parent_id` itself are never removed.
    pub fn prune_child_pages_for_site(
        &self,
        site_id: &str,
        parent_id: &str,
        keep_ids: &[String],
    ) -> Result<usize> {
        let mut keep: HashSet<&str> = keep_ids.iter().map(String::as_str).collect();
        keep.insert(parent_id);
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let existing_ids: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT id FROM pages WHERE site_id = ?1 AND parent_id = ?2")?;
            let rows = stmt.query_map(params![site_id, parent_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let doomed: Vec<String> = existing_ids
            .into_iter()
            .filter(|pid| !keep.contains(pid.as_str()))
            .collect();
        let removed = delete_pages_with_descendants(&tx, site_id, doomed, &keep)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Stores one fetched page of space root pages. Prunes only on a complete first page.
    pub fn record_root_pages_page_for_site(
        &self,
        site_id: &str,
        space_key: &str,
        page: &ListPage<PageSummary>,
        first_page: bool,
    ) -> Result<()> {
        self.upsert_authoritative_pages_for_site(site_id, &page.items)?;
        if first_page && page.has_more == Some(false) {
            let keep: Vec<String> = page.items.iter().map(|p| p.id.clone()).collect();
            self.prune_space_root_pages_for_site(site_id, space_key, &keep)?;
        }
        Ok(())
    }

    /// Stores one fetched page of child pages. Prunes and records childless state only on a complete first page.
    pub fn record_child_pages_page_for_site(
        &self,
        site_id: &str,
        parent_id: &str,
        page: &ListPage<PageSummary>,
        first_page: bool,
    ) -> Result<()> {
        self.upsert_pages_for_site(site_id, &page.items)?;
        if first_page && page.has_more == Some(false) {
            let keep: Vec<String> = page.items.iter().map(|p| p.id.clone()).collect();
            self.prune_child_pages_for_site(site_id, parent_id, &keep)?;
            if page.items.is_empty() {
                self.set_page_has_no_children_for_site(site_id, parent_id)?;
            }
        }
        Ok(())
    }

    /// Marks a page as confirmed childless after a complete, empty child listing.
    pub fn set_page_has_no_children_for_site(&self, site_id: &str, page_id: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE pages SET has_children = 2 WHERE site_id = ?1 AND id = ?2",
            params![site_id, page_id],
        )?;
        Ok(())
    }

    /// Replaces `has_children` on freshly fetched summaries with what the cache knows.
    pub fn overlay_children_state_for_site(
        &self,
        site_id: &str,
        pages: &mut [PageSummary],
    ) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT has_children FROM pages WHERE site_id = ?1 AND id = ?2")?;
        for page in pages {
            if let Some(raw) = stmt
                .query_row(params![site_id, &page.id], |row| row.get::<_, i32>(0))
                .optional()?
            {
                page.has_children = match raw {
                    1 => Some(true),
                    2 => Some(false),
                    _ => None,
                };
            }
        }
        Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentUpdateMode {
    PreserveExistingOnNone,
    Authoritative,
}

struct UpsertPageRowOutcome {
    applied: bool,
    fts_needs_refresh: bool,
}

fn upsert_page_row(
    conn: &Connection,
    site_id: &str,
    page: &PageSummary,
    parent_mode: ParentUpdateMode,
) -> Result<UpsertPageRowOutcome> {
    let incoming_parent = page
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let incoming_space_id = page.space_id.trim();
    let mut resolved_space_key = page.space_key.trim().to_string();

    let existing: Option<(i32, String, Option<String>, String, String, String)> = conn
        .query_row(
            "SELECT version_number, updated_at, parent_id, space_id, space_key, web_url FROM pages WHERE site_id = ?1 AND id = ?2",
            params![site_id, &page.id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;

    if resolved_space_key.is_empty() {
        if let Some((_, _, _, _, ref ex_sk, _)) = existing {
            if !ex_sk.trim().is_empty() {
                resolved_space_key = ex_sk.clone();
            }
        }
    }
    if resolved_space_key.is_empty() && !incoming_space_id.is_empty() {
        if let Some(sk) = conn
            .query_row(
                "SELECT key FROM spaces WHERE site_id = ?1 AND id = ?2",
                params![site_id, incoming_space_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if !sk.trim().is_empty() {
                resolved_space_key = sk;
            }
        }
    }
    if resolved_space_key.is_empty() {
        if let Some(pid) = incoming_parent {
            if let Some(sk) = conn
                .query_row(
                    "SELECT space_key FROM pages WHERE site_id = ?1 AND id = ?2",
                    params![site_id, pid],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                if !sk.trim().is_empty() {
                    resolved_space_key = sk;
                }
            }
        }
    }

    if let Some((
        existing_ver,
        existing_updated,
        existing_parent,
        existing_space_id,
        existing_space_key,
        existing_web_url,
    )) = existing
    {
        let existing_parent_nonempty = existing_parent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let version_missing = page.version_number <= 0 && existing_ver > 0;
        let version_older = page.version_number > 0 && page.version_number < existing_ver;
        let same_version_older_time = page.version_number == existing_ver
            && !existing_updated.trim().is_empty()
            && !page.updated_at.trim().is_empty()
            && crate::storage::jira::compare_updated_at(&page.updated_at, &existing_updated)
                == std::cmp::Ordering::Less;

        if version_missing || version_older || same_version_older_time {
            // Keep content/version monotonic, while enriching hierarchy and space metadata separately.
            let next_parent: Option<&str> = match parent_mode {
                ParentUpdateMode::Authoritative if version_missing => incoming_parent,
                _ if version_missing && incoming_parent.is_some() => incoming_parent,
                _ if existing_parent_nonempty.is_none() => incoming_parent,
                _ => existing_parent_nonempty,
            };
            let next_space_id =
                if existing_space_id.trim().is_empty() && !incoming_space_id.is_empty() {
                    incoming_space_id
                } else {
                    existing_space_id.as_str()
                };
            let next_space_key =
                if existing_space_key.trim().is_empty() && !resolved_space_key.is_empty() {
                    resolved_space_key.as_str()
                } else {
                    existing_space_key.as_str()
                };
            let next_web_url =
                if existing_web_url.trim().is_empty() && !page.web_url.trim().is_empty() {
                    page.web_url.trim()
                } else {
                    existing_web_url.as_str()
                };

            let space_key_changed = next_space_key != existing_space_key;
            let hierarchy_changed = next_parent != existing_parent_nonempty
                || next_space_id != existing_space_id
                || space_key_changed
                || next_web_url != existing_web_url;
            if hierarchy_changed {
                conn.execute(
                    "UPDATE pages SET parent_id = ?1, space_id = ?2, space_key = ?3, web_url = ?4 WHERE site_id = ?5 AND id = ?6",
                    params![
                        next_parent,
                        next_space_id,
                        next_space_key,
                        next_web_url,
                        site_id,
                        &page.id,
                    ],
                )?;
            }
            if let Some(pid) = next_parent {
                conn.execute(
                    "UPDATE pages SET has_children = 1 WHERE site_id = ?1 AND id = ?2",
                    params![site_id, pid],
                )?;
            }
            return Ok(UpsertPageRowOutcome {
                applied: false,
                fts_needs_refresh: space_key_changed,
            });
        }
    }

    let authoritative_flag = if parent_mode == ParentUpdateMode::Authoritative {
        1i32
    } else {
        0i32
    };

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
            space_id = CASE WHEN excluded.space_id != '' THEN excluded.space_id ELSE pages.space_id END,
            space_key = CASE WHEN excluded.space_key != '' THEN excluded.space_key ELSE pages.space_key END,
            parent_id = CASE
                WHEN ?14 != 0 THEN excluded.parent_id
                WHEN excluded.parent_id IS NOT NULL AND excluded.parent_id != '' THEN excluded.parent_id
                ELSE pages.parent_id
            END,
            title = CASE WHEN excluded.title != '' THEN excluded.title ELSE pages.title END,
            status = CASE WHEN excluded.status != '' THEN excluded.status ELSE pages.status END,
            version_number = CASE WHEN excluded.version_number > 0 THEN excluded.version_number ELSE pages.version_number END,
            author_name = COALESCE(excluded.author_name, pages.author_name),
            updated_at = CASE WHEN excluded.updated_at != '' THEN excluded.updated_at ELSE pages.updated_at END,
            position = CASE WHEN ?14 != 0 OR excluded.position != 0 THEN excluded.position ELSE pages.position END,
            has_children = CASE WHEN excluded.has_children != 0 THEN excluded.has_children ELSE pages.has_children END,
            web_url = CASE WHEN excluded.web_url != '' THEN excluded.web_url ELSE pages.web_url END
        ",
        params![
            site_id,
            page.id,
            incoming_space_id,
            resolved_space_key,
            incoming_parent,
            page.title,
            page.status,
            page.version_number,
            page.author_name,
            page.updated_at,
            page.position,
            match page.has_children {
                Some(true) => 1i32,
                Some(false) => 2i32,
                None => 0i32,
            },
            page.web_url,
            authoritative_flag,
        ],
    )?;
    if let Some(pid) = incoming_parent {
        conn.execute(
            "UPDATE pages SET has_children = 1 WHERE site_id = ?1 AND id = ?2",
            params![site_id, pid],
        )?;
    }
    Ok(UpsertPageRowOutcome {
        applied: true,
        fts_needs_refresh: true,
    })
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
        has_children: match row.get::<_, i32>(offset + 10)? {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        },
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

/// Deletes `doomed` pages of `site_id` and every page cached below them (following `parent_id`)
/// from `pages` and `pages_fts`, so a removed page leaves no orphaned subtree behind. Pages in
/// `keep` are never deleted and nothing below them is visited. Returns the page rows deleted.
fn delete_pages_with_descendants(
    conn: &Connection,
    site_id: &str,
    doomed: Vec<String>,
    keep: &HashSet<&str>,
) -> Result<usize> {
    let mut seen: HashSet<String> = doomed.iter().cloned().collect();
    let mut queue = doomed;
    {
        let mut children =
            conn.prepare("SELECT id FROM pages WHERE site_id = ?1 AND parent_id = ?2")?;
        let mut next = 0;
        while next < queue.len() {
            let pid = queue[next].clone();
            next += 1;
            if pid.is_empty() {
                // An empty `parent_id` marks a root page, not a child of a page with an empty id.
                continue;
            }
            let rows = children.query_map(params![site_id, &pid], |r| r.get::<_, String>(0))?;
            for child in rows.collect::<rusqlite::Result<Vec<_>>>()? {
                if !keep.contains(child.as_str()) && seen.insert(child.clone()) {
                    queue.push(child);
                }
            }
        }
    }

    let mut removed = 0usize;
    for pid in &queue {
        conn.execute(
            "DELETE FROM pages_fts WHERE site_id = ?1 AND page_id = ?2",
            params![site_id, pid],
        )?;
        removed += conn.execute(
            "DELETE FROM pages WHERE site_id = ?1 AND id = ?2",
            params![site_id, pid],
        )?;
    }
    Ok(removed)
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
            has_children: Some(parent_id.is_none()),
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
        storage.upsert_spaces(std::slice::from_ref(&sp)).unwrap();
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
            incomplete: vec![],
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

    #[test]
    fn regression_atlassian_bug_02_storage_hierarchy_enrichment_separate_from_version_guard() {
        let storage = Storage::open_in_memory().unwrap();
        // v3 detail cached without parent_id yet
        let mut p_v3 = sample_page("301", None, "V3 Page", 1);
        p_v3.version_number = 3;
        p_v3.updated_at = "2026-11-18T12:00:00Z".into();
        let d_v3 = PageDetail {
            summary: p_v3.clone(),
            body_markdown: "Authoritative v3 body".into(),
            ..Default::default()
        };
        storage.upsert_page_detail_for_site("cloud", &d_v3).unwrap();

        // Live child listing without version/space_key enriches parent_id without downgrading v3 body/version
        let child_listing = PageSummary {
            id: "301".into(),
            space_id: "5001".into(),
            space_key: String::new(),
            parent_id: Some("100".into()),
            title: "Stale Child Title".into(),
            status: "current".into(),
            version_number: 0,
            author_name: None,
            updated_at: String::new(),
            position: 2,
            has_children: None,
            web_url: String::new(),
        };
        storage
            .upsert_pages_for_site("cloud", &[child_listing])
            .unwrap();
        let detail = storage
            .get_page_detail_for_site("cloud", "301")
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.version_number, 3);
        assert_eq!(detail.summary.title, "V3 Page");
        assert_eq!(detail.summary.parent_id.as_deref(), Some("100"));
        assert_eq!(detail.summary.space_key, "ENG");
        assert_eq!(detail.body_markdown, "Authoritative v3 body");
        assert!(!storage.is_page_detail_stale_for_site("cloud", "301").unwrap());

        // CQL search with missing parent_id and empty space_id preserves parent_id="100" and space_id="sp-1"
        let search_hit = PageSummary {
            id: "301".into(),
            space_id: String::new(),
            space_key: "ENG".into(),
            parent_id: None,
            title: "V3 Page".into(),
            status: "current".into(),
            version_number: 3,
            author_name: None,
            updated_at: "2026-11-18T12:00:00Z".into(),
            position: 0,
            has_children: None,
            web_url: String::new(),
        };
        storage.upsert_pages_for_site("cloud", &[search_hit]).unwrap();
        let detail_after_search = storage
            .get_page_detail_for_site("cloud", "301")
            .unwrap()
            .unwrap();
        assert_eq!(detail_after_search.summary.parent_id.as_deref(), Some("100"));
        assert_eq!(detail_after_search.summary.space_id, "sp-1");

        // Authoritative root listing clears parent_id when page is moved to root
        let moved_root = PageSummary {
            parent_id: None,
            ..detail_after_search.summary.clone()
        };
        storage
            .upsert_authoritative_pages_for_site("cloud", &[moved_root])
            .unwrap();
        let detail_root = storage
            .get_page_detail_for_site("cloud", "301")
            .unwrap()
            .unwrap();
        assert_eq!(detail_root.summary.parent_id, None);
    }

    #[test]
    fn has_children_is_unknown_then_true_then_false() {
        let storage = Storage::open_in_memory().unwrap();
        let make = |id: &str, parent_id: Option<&str>| PageSummary {
            id: id.into(),
            space_id: "sp-1".into(),
            space_key: "ENG".into(),
            parent_id: parent_id.map(str::to_string),
            title: format!("Page {id}"),
            status: "current".into(),
            version_number: 1,
            updated_at: "2026-11-20T00:00:00Z".into(),
            has_children: None,
            ..Default::default()
        };

        // (a) Upsert 700 with None -> reads as None
        storage
            .upsert_pages_for_site("default", &[make("700", None)])
            .unwrap();
        let roots = storage.list_space_root_pages_for_site("default", "ENG").unwrap();
        assert_eq!(roots[0].has_children, None);

        // (b) Upsert child 701 with parent 700 -> 700 becomes Some(true)
        storage
            .upsert_pages_for_site("default", &[make("701", Some("700"))])
            .unwrap();
        let roots = storage.list_space_root_pages_for_site("default", "ENG").unwrap();
        assert_eq!(roots[0].has_children, Some(true));

        // (c) Record empty, complete first child page for 701 -> 701 becomes Some(false)
        let empty_complete = ListPage {
            items: Vec::new(),
            has_more: Some(false),
            next_cursor: None,
        };
        storage
            .record_child_pages_page_for_site("default", "701", &empty_complete, true)
            .unwrap();
        let kids = storage.list_child_pages_for_site("default", "700").unwrap();
        assert_eq!(kids[0].id, "701");
        assert_eq!(kids[0].has_children, Some(false));

        // (d) Empty page with has_more: None for 702 -> stays None
        storage
            .upsert_pages_for_site("default", &[make("702", None)])
            .unwrap();
        let empty_unknown = ListPage {
            items: Vec::new(),
            has_more: None,
            next_cursor: None,
        };
        storage
            .record_child_pages_page_for_site("default", "702", &empty_unknown, true)
            .unwrap();
        let roots = storage.list_space_root_pages_for_site("default", "ENG").unwrap();
        let p702 = roots.iter().find(|p| p.id == "702").unwrap();
        assert_eq!(p702.has_children, None);

        // (e) Legacy row with has_children = 0 reads as None
        {
            let conn = storage.lock_conn().unwrap();
            conn.execute(
                "INSERT INTO pages (site_id, id, space_id, space_key, parent_id, title, status, version_number, updated_at, position, has_children, web_url) VALUES ('default', '703', 'sp-1', 'ENG', NULL, 'Legacy', 'current', 1, '2026-11-20T00:00:00Z', 0, 0, '')",
                [],
            )
            .unwrap();
        }
        let roots = storage.list_space_root_pages_for_site("default", "ENG").unwrap();
        let p703 = roots.iter().find(|p| p.id == "703").unwrap();
        assert_eq!(p703.has_children, None);
    }

    #[test]
    fn record_pages_prune_only_on_complete_first_page() {
        let storage = Storage::open_in_memory().unwrap();
        let a = sample_page("A", Some("P"), "Child A", 1);
        let b = sample_page("B", Some("P"), "Child B", 2);
        storage
            .upsert_pages_for_site("default", &[a.clone(), b.clone()])
            .unwrap();

        // (a) has_more = Some(true), first_page = true -> B kept
        storage
            .record_child_pages_page_for_site(
                "default",
                "P",
                &ListPage {
                    items: vec![a.clone()],
                    has_more: Some(true),
                    next_cursor: Some("1".into()),
                },
                true,
            )
            .unwrap();
        assert_eq!(
            storage.list_child_pages_for_site("default", "P").unwrap().len(),
            2
        );

        // (b) has_more = Some(false), first_page = false -> B kept
        storage
            .record_child_pages_page_for_site(
                "default",
                "P",
                &ListPage {
                    items: vec![a.clone()],
                    has_more: Some(false),
                    next_cursor: None,
                },
                false,
            )
            .unwrap();
        assert_eq!(
            storage.list_child_pages_for_site("default", "P").unwrap().len(),
            2
        );

        // (c) has_more = Some(false), first_page = true -> B pruned
        storage
            .record_child_pages_page_for_site(
                "default",
                "P",
                &ListPage {
                    items: vec![a],
                    has_more: Some(false),
                    next_cursor: None,
                },
                true,
            )
            .unwrap();
        let remaining = storage.list_child_pages_for_site("default", "P").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "A");
    }

    fn cached_page_ids(storage: &Storage, site_id: &str) -> Vec<String> {
        let conn = storage.lock_conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM pages WHERE site_id = ?1 ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([site_id], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    fn cached_page_fts_ids(storage: &Storage, site_id: &str) -> Vec<String> {
        let conn = storage.lock_conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT page_id FROM pages_fts WHERE site_id = ?1 ORDER BY page_id")
            .unwrap();
        let rows = stmt.query_map([site_id], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn prune_root_pages_matches_space_key_case_insensitively() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_pages_for_site(
                "default",
                &[
                    sample_page("R1", None, "Root one", 1),
                    sample_page("R2", None, "Root two", 2),
                ],
            )
            .unwrap();

        // Cached rows carry "ENG"; the caller spells the same space "eng".
        let removed = storage
            .prune_space_root_pages_for_site("default", "eng", &["R1".to_string()])
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(cached_page_ids(&storage, "default"), vec!["R1".to_string()]);
    }

    #[test]
    fn prune_root_page_removes_cached_descendants() {
        let storage = Storage::open_in_memory().unwrap();
        let tree = [
            sample_page("KEEP", None, "Kept root", 1),
            sample_page("K1", Some("KEEP"), "Child of kept root", 1),
            sample_page("GONE", None, "Removed root", 2),
            sample_page("C1", Some("GONE"), "Child of removed root", 1),
            sample_page("G1", Some("C1"), "Grandchild of removed root", 1),
        ];
        storage.upsert_pages_for_site("default", &tree).unwrap();
        storage.upsert_pages_for_site("site-b", &tree).unwrap();
        assert_eq!(cached_page_fts_ids(&storage, "default").len(), 5);

        let removed = storage
            .prune_space_root_pages_for_site("default", "ENG", &["KEEP".to_string()])
            .unwrap();

        // The root and everything cached below it are gone, from the table and from FTS.
        let expected = vec!["K1".to_string(), "KEEP".to_string()];
        assert_eq!(cached_page_ids(&storage, "default"), expected);
        assert_eq!(cached_page_fts_ids(&storage, "default"), expected);
        assert_eq!(removed, 3);
        assert!(storage
            .list_child_pages_for_site("default", "GONE")
            .unwrap()
            .is_empty());
        // Same ids on another site are untouched.
        assert_eq!(cached_page_ids(&storage, "site-b").len(), 5);
        assert_eq!(cached_page_fts_ids(&storage, "site-b").len(), 5);
    }

    #[test]
    fn prune_child_page_removes_cached_descendants() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_pages_for_site(
                "default",
                &[
                    sample_page("P", None, "Parent", 1),
                    sample_page("A", Some("P"), "Kept child", 1),
                    sample_page("A1", Some("A"), "Child of kept child", 1),
                    sample_page("B", Some("P"), "Removed child", 2),
                    sample_page("B1", Some("B"), "Grandchild", 1),
                    sample_page("B2", Some("B1"), "Great-grandchild", 1),
                ],
            )
            .unwrap();

        let removed = storage
            .prune_child_pages_for_site("default", "P", &["A".to_string()])
            .unwrap();

        let expected = vec!["A".to_string(), "A1".to_string(), "P".to_string()];
        assert_eq!(cached_page_ids(&storage, "default"), expected);
        assert_eq!(cached_page_fts_ids(&storage, "default"), expected);
        assert_eq!(removed, 3);
    }
}
