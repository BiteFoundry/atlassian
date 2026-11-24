//! Jira projects, boards, sprints, issues, details, filters, and `issues_fts` persistence (multi-site aware).

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::{
    adf,
    models::{
        AttachmentItem, BoardColumn, BoardSummary, IssueComment, IssueDetail, IssuePriority,
        IssueStatus, IssueSummary, IssueType, ListPage, ProjectSummary, SavedFilter, SearchHitKind,
        SprintSummary, StatusCategory, UnifiedSearchHit, UserSummary,
    },
    storage::{
        db::{build_contextual_snippet, build_fts_query, escape_like, DEFAULT_SITE_ID},
        Storage,
    },
};

/// Conditions for an offline issue list. Rows are matched before the limit is applied, so the
/// limit counts matching issues, not scanned rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IssueListFilter<'a> {
    /// Exact project key.
    pub project: Option<&'a str>,
    /// Case-insensitive substring of the status name.
    pub status_contains: Option<&'a str>,
    /// Cloud account ID or Data Center username of the assignee.
    pub assignee_id: Option<&'a str>,
    /// Skip issues whose status category is Done.
    pub open_only: bool,
}

impl Storage {
    pub fn upsert_projects(&self, projects: &[ProjectSummary]) -> Result<()> {
        self.upsert_projects_for_site(DEFAULT_SITE_ID, projects)
    }

    pub fn upsert_projects_for_site(
        &self,
        site_id: &str,
        projects: &[ProjectSummary],
    ) -> Result<()> {
        if projects.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO projects (site_id, key, id, name, project_type, lead_name, avatar_url, pinned)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(site_id, key) DO UPDATE SET
                    id = excluded.id,
                    name = excluded.name,
                    project_type = excluded.project_type,
                    lead_name = COALESCE(excluded.lead_name, projects.lead_name),
                    avatar_url = COALESCE(excluded.avatar_url, projects.avatar_url),
                    pinned = CASE WHEN excluded.pinned != 0 THEN 1 ELSE projects.pinned END
                ",
            )?;
            for p in projects {
                stmt.execute(params![
                    site_id,
                    p.key,
                    p.id,
                    p.name,
                    p.project_type,
                    p.lead_name,
                    p.avatar_url,
                    if p.pinned { 1i32 } else { 0i32 },
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        self.list_projects_for_site(DEFAULT_SITE_ID)
    }

    pub fn list_projects_for_site(&self, site_id: &str) -> Result<Vec<ProjectSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, key, name, project_type, lead_name, avatar_url, pinned
            FROM projects
            WHERE site_id = ?1
            ORDER BY pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([site_id], |row| map_project_row(row, 0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_projects_all_sites(&self) -> Result<Vec<(String, ProjectSummary)>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT site_id, id, key, name, project_type, lead_name, avatar_url, pinned
            FROM projects
            ORDER BY site_id ASC, pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let site_id: String = row.get(0)?;
            let project = map_project_row(row, 1)?;
            Ok((site_id, project))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_project_pinned(&self, key: &str, pinned: bool) -> Result<()> {
        self.set_project_pinned_for_site(DEFAULT_SITE_ID, key, pinned)
    }

    pub fn set_project_pinned_for_site(
        &self,
        site_id: &str,
        key: &str,
        pinned: bool,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE projects SET pinned = ?1 WHERE site_id = ?2 AND key = ?3",
            params![if pinned { 1i32 } else { 0i32 }, site_id, key],
        )?;
        Ok(())
    }

    pub fn upsert_boards(&self, boards: &[BoardSummary]) -> Result<()> {
        self.upsert_boards_for_site(DEFAULT_SITE_ID, boards)
    }

    pub fn upsert_boards_for_site(&self, site_id: &str, boards: &[BoardSummary]) -> Result<()> {
        if boards.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO boards (site_id, id, name, board_type, project_key, project_name, columns_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(site_id, id) DO UPDATE SET
                    name = excluded.name,
                    board_type = excluded.board_type,
                    project_key = COALESCE(excluded.project_key, boards.project_key),
                    project_name = COALESCE(excluded.project_name, boards.project_name),
                    columns_json = CASE
                        WHEN excluded.columns_json != '[]' THEN excluded.columns_json
                        ELSE boards.columns_json
                    END
                ",
            )?;
            for b in boards {
                let columns_json = serde_json::to_string(&b.columns)?;
                stmt.execute(params![
                    site_id,
                    b.id,
                    b.name,
                    b.board_type,
                    b.project_key,
                    b.project_name,
                    columns_json,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_boards(&self, project_key: Option<&str>) -> Result<Vec<BoardSummary>> {
        self.list_boards_for_site(DEFAULT_SITE_ID, project_key)
    }

    pub fn list_boards_for_site(
        &self,
        site_id: &str,
        project_key: Option<&str>,
    ) -> Result<Vec<BoardSummary>> {
        let conn = self.lock_conn()?;
        if let Some(pk) = project_key {
            let mut stmt = conn.prepare(
                "
                SELECT id, name, board_type, project_key, project_name, columns_json
                FROM boards
                WHERE site_id = ?1 AND project_key = ?2
                ORDER BY name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map(params![site_id, pk], |row| map_board_row(row, 0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT id, name, board_type, project_key, project_name, columns_json
                FROM boards
                WHERE site_id = ?1
                ORDER BY name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map([site_id], |row| map_board_row(row, 0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn list_boards_all_sites(
        &self,
        project_key: Option<&str>,
    ) -> Result<Vec<(String, BoardSummary)>> {
        let conn = self.lock_conn()?;
        if let Some(pk) = project_key {
            let mut stmt = conn.prepare(
                "
                SELECT site_id, id, name, board_type, project_key, project_name, columns_json
                FROM boards
                WHERE project_key = ?1
                ORDER BY site_id ASC, name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map([pk], |row| {
                let site_id: String = row.get(0)?;
                let board = map_board_row(row, 1)?;
                Ok((site_id, board))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT site_id, id, name, board_type, project_key, project_name, columns_json
                FROM boards
                ORDER BY site_id ASC, name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map([], |row| {
                let site_id: String = row.get(0)?;
                let board = map_board_row(row, 1)?;
                Ok((site_id, board))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn upsert_sprints(&self, sprints: &[SprintSummary]) -> Result<()> {
        self.upsert_sprints_for_site(DEFAULT_SITE_ID, sprints)
    }

    pub fn upsert_sprints_for_site(&self, site_id: &str, sprints: &[SprintSummary]) -> Result<()> {
        if sprints.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO sprints (site_id, id, board_id, name, state, start_date, end_date, goal)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(site_id, id) DO UPDATE SET
                    board_id = excluded.board_id,
                    name = excluded.name,
                    state = excluded.state,
                    start_date = COALESCE(excluded.start_date, sprints.start_date),
                    end_date = COALESCE(excluded.end_date, sprints.end_date),
                    goal = COALESCE(excluded.goal, sprints.goal)
                ",
            )?;
            let mut link = tx.prepare(
                "INSERT OR IGNORE INTO board_sprints (site_id, board_id, sprint_id) VALUES (?1, ?2, ?3)",
            )?;
            for s in sprints {
                // `sprints.board_id` holds the owning board; `board_sprints` records every board
                // the sprint is listed under, so a shared sprint shows up on each of them.
                let home_board_id = s.origin_board_id.unwrap_or(s.board_id);
                stmt.execute(params![
                    site_id,
                    s.id,
                    home_board_id,
                    s.name,
                    s.state,
                    s.start_date,
                    s.end_date,
                    s.goal,
                ])?;
                link.execute(params![site_id, s.board_id, s.id])?;
                if home_board_id != s.board_id {
                    link.execute(params![site_id, home_board_id, s.id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_sprints(&self, board_id: i64) -> Result<Vec<SprintSummary>> {
        self.list_sprints_for_site(DEFAULT_SITE_ID, board_id)
    }

    /// Lists the sprints linked to `board_id`, including sprints owned by another board. Each
    /// result reports `board_id` as the queried board and `origin_board_id` as the board stored
    /// on the sprint row.
    pub fn list_sprints_for_site(
        &self,
        site_id: &str,
        board_id: i64,
    ) -> Result<Vec<SprintSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT s.id, s.board_id, s.name, s.state, s.start_date, s.end_date, s.goal
            FROM sprints s
            JOIN board_sprints bs ON bs.site_id = s.site_id AND bs.sprint_id = s.id
            WHERE s.site_id = ?1 AND bs.board_id = ?2
            ORDER BY
                CASE s.state WHEN 'active' THEN 0 WHEN 'future' THEN 1 ELSE 2 END,
                s.id DESC
            ",
        )?;
        let rows = stmt.query_map(params![site_id, board_id], |row| {
            Ok(SprintSummary {
                id: row.get(0)?,
                board_id,
                origin_board_id: Some(row.get(1)?),
                name: row.get(2)?,
                state: row.get(3)?,
                start_date: row.get(4)?,
                end_date: row.get(5)?,
                goal: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn upsert_issues(&self, issues: &[IssueSummary]) -> Result<()> {
        self.upsert_issues_for_site(DEFAULT_SITE_ID, issues)
    }

    pub fn upsert_issues_for_site(&self, site_id: &str, issues: &[IssueSummary]) -> Result<()> {
        self.upsert_issues_inner(site_id, None, None, issues)
    }

    /// Upserts issues retrieved from a specific Agile board/sprint context.
    pub fn upsert_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        issues: &[IssueSummary],
    ) -> Result<()> {
        self.upsert_board_issues_for_site(DEFAULT_SITE_ID, board_id, sprint_id, issues)
    }

    pub fn upsert_board_issues_for_site(
        &self,
        site_id: &str,
        board_id: i64,
        sprint_id: Option<i64>,
        issues: &[IssueSummary],
    ) -> Result<()> {
        self.upsert_issues_inner(site_id, Some(board_id), sprint_id, issues)
    }

    fn upsert_issues_inner(
        &self,
        site_id: &str,
        board_id: Option<i64>,
        default_sprint_id: Option<i64>,
        issues: &[IssueSummary],
    ) -> Result<()> {
        if issues.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            for issue in issues {
                let eff_sprint_id = issue.sprint_id.or(default_sprint_id);
                let applied = upsert_issue_row(&tx, site_id, issue, board_id, eff_sprint_id)?;
                if !applied {
                    continue;
                }
                let existing_desc: String = tx
                    .query_row(
                        "SELECT description_markdown FROM issues WHERE site_id = ?1 AND key = ?2",
                        params![site_id, &issue.key],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                refresh_issue_fts(
                    &tx,
                    site_id,
                    &issue.key,
                    &issue.project_key,
                    &issue.summary,
                    &existing_desc,
                    &issue.labels.join(" "),
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_issue_detail(&self, detail: &IssueDetail) -> Result<()> {
        self.upsert_issue_detail_for_site(DEFAULT_SITE_ID, detail)
    }

    pub fn upsert_issue_detail_for_site(&self, site_id: &str, detail: &IssueDetail) -> Result<()> {
        let mut owned = detail.clone();
        self.upsert_issue_detail_for_site_merging(site_id, &mut owned, false)
    }

    /// Upserts a Jira issue detail for `site_id`, enforcing monotonic `updated_at` and optionally
    /// preserving previously cached `linked_pages` when `/remotelink` transiently failed.
    pub fn upsert_issue_detail_for_site_merging(
        &self,
        site_id: &str,
        detail: &mut IssueDetail,
        preserve_linked_pages: bool,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let existing_detail_raw: Option<String> = tx
                .query_row(
                    "SELECT detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                    params![site_id, &detail.summary.key],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();

            // A detail without a timestamp cannot be ordered against the cached row. It takes the
            // cached timestamp so the fetched detail is stored instead of dropped as older.
            if detail.summary.updated_at.trim().is_empty() {
                let existing_updated: Option<String> = tx
                    .query_row(
                        "SELECT updated_at FROM issues WHERE site_id = ?1 AND key = ?2",
                        params![site_id, &detail.summary.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing_updated) = existing_updated.filter(|s| !s.trim().is_empty()) {
                    detail.summary.updated_at = existing_updated;
                }
            }

            let applied =
                upsert_issue_row(&tx, site_id, &detail.summary, None, detail.summary.sprint_id)?;
            if !applied {
                if let Some(raw) = existing_detail_raw.as_deref() {
                    if let Ok(newer) = serde_json::from_str::<IssueDetail>(raw) {
                        *detail = newer;
                    }
                }
                tx.commit()?;
                return Ok(());
            }

            if preserve_linked_pages {
                if let Some(raw) = existing_detail_raw.as_deref() {
                    if let Ok(prev) = serde_json::from_str::<IssueDetail>(raw) {
                        for lp in prev.linked_pages {
                            let exists = detail.linked_pages.iter().any(|cur| {
                                (!lp.page_id.is_empty() && cur.page_id == lp.page_id)
                                    || (!lp.web_url.is_empty() && cur.web_url == lp.web_url)
                            });
                            if !exists {
                                detail.linked_pages.push(lp);
                            }
                        }
                    }
                }
            }

            let issue = &detail.summary;
            let detail_json =
                serde_json::to_string(detail).context("Failed to serialize IssueDetail")?;
            tx.execute(
                "UPDATE issues SET description_markdown = ?1, detail_json = ?2, detail_updated_at = ?3 WHERE site_id = ?4 AND key = ?5",
                params![
                    detail.description_markdown,
                    detail_json,
                    issue.updated_at,
                    site_id,
                    issue.key
                ],
            )?;
            let plain_desc = if !detail.description_blocks.is_empty() {
                adf::blocks_to_plain_text(&detail.description_blocks)
            } else {
                detail.description_markdown.clone()
            };
            refresh_issue_fts(
                &tx,
                site_id,
                &issue.key,
                &issue.project_key,
                &issue.summary,
                &plain_desc,
                &issue.labels.join(" "),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_issue_detail(&self, key: &str) -> Result<Option<IssueDetail>> {
        self.get_issue_detail_for_site(DEFAULT_SITE_ID, key)
    }

    pub fn get_issue_detail_for_site(
        &self,
        site_id: &str,
        key: &str,
    ) -> Result<Option<IssueDetail>> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(IssueSummary, Option<String>)> = conn
            .query_row(
                "
                SELECT
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url, detail_json
                FROM issues
                WHERE site_id = ?1 AND key = ?2
                ",
                params![site_id, key],
                |row| {
                    let summary = map_issue_summary_row(row, 0)?;
                    let detail_json: Option<String> = row.get(20)?;
                    Ok((summary, detail_json))
                },
            )
            .optional()?;
        match row_opt {
            Some((summary, Some(json_str))) if !json_str.trim().is_empty() => {
                let mut detail = serde_json::from_str::<IssueDetail>(&json_str)
                    .context("Failed to deserialize IssueDetail")?;
                // Reconcile summary fields from authoritative table columns so list and offline
                // reader views never split-brain after list/sync updates.
                detail.summary = summary;
                Ok(Some(detail))
            }
            _ => Ok(None),
        }
    }

    /// Returns `true` when cached `detail_json` was captured at an older `updated_at` than the
    /// latest list/sync summary row for `(site_id, key)`.
    pub fn is_issue_detail_stale_for_site(&self, site_id: &str, key: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT updated_at, detail_updated_at, detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((row_updated, detail_updated, detail_json)) = row_opt else {
            return Ok(false);
        };
        let Some(raw_json) = detail_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(false);
        };
        let effective_detail_updated = detail_updated
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                serde_json::from_str::<IssueDetail>(raw_json)
                    .ok()
                    .map(|d| d.summary.updated_at)
                    .filter(|s| !s.trim().is_empty())
            });
        let Some(du) = effective_detail_updated else {
            return Ok(false);
        };
        if row_updated.trim().is_empty() {
            return Ok(false);
        }
        Ok(compare_updated_at(&du, &row_updated) == std::cmp::Ordering::Less)
    }

    /// Returns the `updated_at` timestamp captured when `detail_json` was actually fetched
    /// (independent of any newer list/sync summary row reconciled on read).
    pub fn get_issue_detail_updated_at_for_site(
        &self,
        site_id: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let conn = self.lock_conn()?;
        let row_opt: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT detail_updated_at, detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((detail_updated, detail_json)) = row_opt else {
            return Ok(None);
        };
        let Some(raw_json) = detail_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        Ok(detail_updated
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                serde_json::from_str::<IssueDetail>(raw_json)
                    .ok()
                    .map(|d| d.summary.updated_at)
                    .filter(|s| !s.trim().is_empty())
            }))
    }

    /// Returns the latest summary row `updated_at` stored for `(site_id, key)`.
    pub fn get_issue_row_updated_at_for_site(
        &self,
        site_id: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let conn = self.lock_conn()?;
        let updated: Option<String> = conn
            .query_row(
                "SELECT updated_at FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(updated.filter(|s| !s.trim().is_empty()))
    }

    /// Upserts a lightweight revision `IssueSummary` for `site_id`, preserving previously cached
    /// `comment_count` and `attachment_count` when the revision payload omitted those collections.
    pub fn upsert_issue_revision_for_site(
        &self,
        site_id: &str,
        issue: &IssueSummary,
    ) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let applied = {
            let mut merged = issue.clone();
            if let Some((prev_comments, prev_atts)) = tx
                .query_row(
                    "SELECT comment_count, attachment_count FROM issues WHERE site_id = ?1 AND key = ?2",
                    params![site_id, &issue.key],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
            {
                if merged.comment_count == 0 && prev_comments > 0 {
                    merged.comment_count = prev_comments as usize;
                }
                if merged.attachment_count == 0 && prev_atts > 0 {
                    merged.attachment_count = prev_atts as usize;
                }
            }
            let ok = upsert_issue_row(&tx, site_id, &merged, None, merged.sprint_id)?;
            if ok {
                let existing_desc: String = tx
                    .query_row(
                        "SELECT description_markdown FROM issues WHERE site_id = ?1 AND key = ?2",
                        params![site_id, &merged.key],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                refresh_issue_fts(
                    &tx,
                    site_id,
                    &merged.key,
                    &merged.project_key,
                    &merged.summary,
                    &existing_desc,
                    &merged.labels.join(" "),
                )?;
            }
            ok
        };
        tx.commit()?;
        Ok(applied)
    }

    /// Updates cached `comments` inside `detail_json` and `comment_count` for `(site_id, key)`.
    pub fn update_issue_comments_for_site(
        &self,
        site_id: &str,
        key: &str,
        comments: &[IssueComment],
    ) -> Result<()> {
        self.update_issue_comments_for_site_with_total(site_id, key, comments, None)
    }

    /// Like `update_issue_comments_for_site`, with the comment total the server reported. The
    /// stored `comment_count` is the larger of `total` and `comments.len()`, so a capped or
    /// partial comment list does not lower the count.
    pub fn update_issue_comments_for_site_with_total(
        &self,
        site_id: &str,
        key: &str,
        comments: &[IssueComment],
        total: Option<usize>,
    ) -> Result<()> {
        let comment_count = total.unwrap_or(0).max(comments.len());
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<IssueDetail>(&raw) {
                detail.comments = comments.to_vec();
                detail.summary.comment_count = comment_count;
                let updated_json = serde_json::to_string(&detail)?;
                tx.execute(
                    "UPDATE issues SET detail_json = ?1, comment_count = ?2 WHERE site_id = ?3 AND key = ?4",
                    params![updated_json, comment_count as i64, site_id, key],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Updates cached `attachments` inside `detail_json` and `attachment_count` for `(site_id, key)`.
    pub fn update_issue_attachments_for_site(
        &self,
        site_id: &str,
        key: &str,
        attachments: &[AttachmentItem],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(raw) = tx
            .query_row(
                "SELECT detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            if let Ok(mut detail) = serde_json::from_str::<IssueDetail>(&raw) {
                detail.attachments = attachments.to_vec();
                detail.summary.attachment_count = attachments.len();
                let updated_json = serde_json::to_string(&detail)?;
                tx.execute(
                    "UPDATE issues SET detail_json = ?1, attachment_count = ?2 WHERE site_id = ?3 AND key = ?4",
                    params![updated_json, attachments.len() as i64, site_id, key],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Detaches cached issues of `(site_id, board_id, sprint_id)` that are absent from a complete
    /// board/sprint snapshot, and returns how many were detached. A sprint-scoped prune clears
    /// the issue's sprint; a board-scoped prune (`sprint_id == None`) clears its board. The issue
    /// row, its cached detail and its search entry stay. Never touches other sites or boards.
    pub fn prune_board_issues_for_site(
        &self,
        site_id: &str,
        board_id: i64,
        sprint_id: Option<i64>,
        keep_keys: &[String],
    ) -> Result<usize> {
        let keep: HashSet<&str> = keep_keys.iter().map(String::as_str).collect();
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let existing_keys: Vec<String> = if let Some(sid) = sprint_id {
            let mut stmt = tx.prepare(
                "SELECT key FROM issues WHERE site_id = ?1 AND board_id = ?2 AND sprint_id = ?3",
            )?;
            let rows = stmt.query_map(params![site_id, board_id, sid], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt =
                tx.prepare("SELECT key FROM issues WHERE site_id = ?1 AND board_id = ?2")?;
            let rows = stmt.query_map(params![site_id, board_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut detached = 0usize;
        for key in existing_keys {
            if !keep.contains(key.as_str()) {
                detached += if let Some(sid) = sprint_id {
                    tx.execute(
                        "UPDATE issues SET sprint_id = NULL, sprint_name = NULL WHERE site_id = ?1 AND key = ?2 AND sprint_id = ?3",
                        params![site_id, &key, sid],
                    )?
                } else {
                    tx.execute(
                        "UPDATE issues SET board_id = NULL WHERE site_id = ?1 AND key = ?2 AND board_id = ?3",
                        params![site_id, &key, board_id],
                    )?
                };
            }
        }
        tx.commit()?;
        Ok(detached)
    }

    /// Stores one fetched page. Prunes only when it is a first page the source marked as the last.
    pub fn record_board_issues_page_for_site(
        &self,
        site_id: &str,
        board_id: i64,
        sprint_id: Option<i64>,
        page: &ListPage<IssueSummary>,
        first_page: bool,
    ) -> Result<()> {
        self.upsert_board_issues_for_site(site_id, board_id, sprint_id, &page.items)?;
        if first_page && page.has_more == Some(false) {
            let keep: Vec<String> = page.items.iter().map(|i| i.key.clone()).collect();
            self.prune_board_issues_for_site(site_id, board_id, sprint_id, &keep)?;
        }
        Ok(())
    }

    pub fn list_issues(
        &self,
        project_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        self.list_issues_for_site(DEFAULT_SITE_ID, project_key, limit)
    }

    pub fn list_issues_for_site(
        &self,
        site_id: &str,
        project_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        if let Some(pk) = project_key {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                WHERE site_id = ?1 AND project_key = ?2
                ORDER BY updated_at DESC, key ASC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(params![site_id, pk, limit as i64], |row| {
                map_issue_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                WHERE site_id = ?1
                ORDER BY updated_at DESC, key ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![site_id, limit as i64], |row| {
                map_issue_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn count_issues_for_site(
        &self,
        site_id: &str,
        project_key: Option<&str>,
    ) -> Result<usize> {
        let conn = self.lock_conn()?;
        let count: i64 = if let Some(pk) = project_key {
            conn.query_row(
                "SELECT COUNT(*) FROM issues WHERE site_id = ?1 AND project_key = ?2",
                params![site_id, pk],
                |row| row.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM issues WHERE site_id = ?1",
                [site_id],
                |row| row.get(0),
            )?
        };
        Ok(count.max(0) as usize)
    }

    /// Cached issues of a site that match `filter`, newest first, at most `limit` of them.
    /// Status and assignee live in JSON columns, so they are matched here rather than in SQL.
    pub fn list_issues_filtered_for_site(
        &self,
        site_id: &str,
        filter: &IssueListFilter<'_>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let status_needle = filter
            .status_contains
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let matches = |issue: &IssueSummary| {
            status_needle
                .as_deref()
                .is_none_or(|needle| issue.status.name.to_lowercase().contains(needle))
                && filter.assignee_id.is_none_or(|id| {
                    issue.assignee.as_ref().is_some_and(|user| user.id == id)
                })
                && !(filter.open_only && matches!(issue.status.category, StatusCategory::Done))
        };
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                id, key, project_key, summary, status_json, priority_json,
                issue_type_json, assignee_json, reporter_json, parent_key,
                parent_summary, labels_json, story_points, sprint_id,
                sprint_name, comment_count, attachment_count, created_at,
                updated_at, web_url
            FROM issues
            WHERE site_id = ?1 AND (?2 IS NULL OR project_key = ?2)
            ORDER BY updated_at DESC, key ASC
            ",
        )?;
        let mut rows = stmt.query(params![site_id, filter.project])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let issue = map_issue_summary_row(row, 0)?;
            if matches(&issue) {
                out.push(issue);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Number of cached issues of a site that match `filter`, ignoring any limit.
    pub fn count_issues_filtered_for_site(
        &self,
        site_id: &str,
        filter: &IssueListFilter<'_>,
    ) -> Result<usize> {
        if filter.status_contains.is_none() && filter.assignee_id.is_none() && !filter.open_only {
            return self.count_issues_for_site(site_id, filter.project);
        }
        Ok(self.list_issues_filtered_for_site(site_id, filter, usize::MAX)?.len())
    }

    pub fn list_issues_all_sites(
        &self,
        project_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, IssueSummary)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        if let Some(pk) = project_key {
            let mut stmt = conn.prepare(
                "
                SELECT
                    site_id,
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                WHERE project_key = ?1
                ORDER BY updated_at DESC, site_id ASC, key ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![pk, limit as i64], |row| {
                let site_id: String = row.get(0)?;
                let issue = map_issue_summary_row(row, 1)?;
                Ok((site_id, issue))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT
                    site_id,
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                ORDER BY updated_at DESC, site_id ASC, key ASC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit as i64], |row| {
                let site_id: String = row.get(0)?;
                let issue = map_issue_summary_row(row, 1)?;
                Ok((site_id, issue))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    /// Federated "My Work" feed across all enabled sites: open issues (statusCategory != Done),
    /// ordered most-recently-updated first.
    pub fn list_my_work_all_sites(&self, limit: usize) -> Result<Vec<(String, IssueSummary)>> {
        self.query_my_work(None, None, limit)
    }

    /// Open (non-Done) issues of one site for the My Work seed. With `me_id`
    /// only issues assigned to that account are returned; the assignee filter
    /// runs in SQL so it applies before `LIMIT`.
    pub fn list_my_work_for_site(
        &self,
        site_id: &str,
        me_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        Ok(self
            .query_my_work(Some(site_id), me_id, limit)?
            .into_iter()
            .map(|(_, issue)| issue)
            .collect())
    }

    fn query_my_work(
        &self,
        site_id: Option<&str>,
        me_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, IssueSummary)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let me_id = me_id.map(str::trim).filter(|s| !s.is_empty());
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                i.site_id,
                i.id, i.key, i.project_key, i.summary, i.status_json, i.priority_json,
                i.issue_type_json, i.assignee_json, i.reporter_json, i.parent_key,
                i.parent_summary, i.labels_json, i.story_points, i.sprint_id,
                i.sprint_name, i.comment_count, i.attachment_count, i.created_at,
                i.updated_at, i.web_url
            FROM issues i
            WHERE COALESCE(json_extract(i.status_json, '$.category'), '') != 'done'
              AND NOT EXISTS (
                  SELECT 1 FROM sites s WHERE s.site_id = i.site_id AND s.enabled = 0
              )
              AND (?2 IS NULL OR i.site_id = ?2)
              AND (?3 IS NULL OR json_extract(i.assignee_json, '$.id') = ?3)
            ORDER BY i.updated_at DESC, i.site_id ASC, i.key ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map(params![limit as i64, site_id, me_id], |row| {
            let site_id: String = row.get(0)?;
            let issue = map_issue_summary_row(row, 1)?;
            Ok((site_id, issue))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        self.list_board_issues_for_site(DEFAULT_SITE_ID, board_id, sprint_id, limit)
    }

    pub fn list_board_issues_for_site(
        &self,
        site_id: &str,
        board_id: i64,
        sprint_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        if let Some(sid) = sprint_id {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                WHERE site_id = ?1
                  AND sprint_id = ?2
                  AND (
                      board_id = ?3
                      OR board_id IS NULL
                      OR sprint_id IN (SELECT id FROM sprints WHERE site_id = ?1 AND board_id = ?3)
                  )
                ORDER BY updated_at DESC, key ASC
                LIMIT ?4
                ",
            )?;
            let rows = stmt.query_map(params![site_id, sid, board_id, limit as i64], |row| {
                map_issue_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT
                    id, key, project_key, summary, status_json, priority_json,
                    issue_type_json, assignee_json, reporter_json, parent_key,
                    parent_summary, labels_json, story_points, sprint_id,
                    sprint_name, comment_count, attachment_count, created_at,
                    updated_at, web_url
                FROM issues
                WHERE site_id = ?1
                  AND (
                      board_id = ?2
                      OR sprint_id IN (SELECT id FROM sprints WHERE site_id = ?1 AND board_id = ?2)
                      OR (
                          board_id IS NULL
                          AND project_key = (SELECT project_key FROM boards WHERE site_id = ?1 AND id = ?2)
                      )
                  )
                ORDER BY updated_at DESC, key ASC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(params![site_id, board_id, limit as i64], |row| {
                map_issue_summary_row(row, 0)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn delete_issue(&self, key: &str) -> Result<()> {
        self.delete_issue_for_site(DEFAULT_SITE_ID, key)
    }

    pub fn delete_issue_for_site(&self, site_id: &str, key: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM issues_fts WHERE site_id = ?1 AND issue_key = ?2",
            params![site_id, key],
        )?;
        tx.execute(
            "DELETE FROM issues WHERE site_id = ?1 AND key = ?2",
            params![site_id, key],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn search_issues(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        let scoped = self.search_issues_scoped(query, None, limit)?;
        Ok(scoped.into_iter().map(|(_, hit)| hit).collect())
    }

    pub fn search_issues_scoped(
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
                if let Ok(fts_hits) = run_issue_fts_search(&conn, &fts_q, trimmed, site_id, limit) {
                    for (sid, hit) in fts_hits {
                        if seen.insert((sid.clone(), hit.id_or_key.clone())) {
                            results.push((sid, hit));
                        }
                    }
                }
            }
        }

        if results.len() < limit {
            let like_hits = run_issue_like_search(&conn, trimmed, site_id, limit)?;
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

    pub fn upsert_saved_filters(&self, filters: &[SavedFilter]) -> Result<()> {
        self.upsert_saved_filters_for_site(DEFAULT_SITE_ID, filters)
    }

    pub fn upsert_saved_filters_for_site(
        &self,
        site_id: &str,
        filters: &[SavedFilter],
    ) -> Result<()> {
        if filters.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO saved_filters (site_id, id, name, jql, builtin)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(site_id, id) DO UPDATE SET
                    name = excluded.name,
                    jql = excluded.jql,
                    builtin = CASE WHEN saved_filters.builtin != 0 THEN 1 ELSE excluded.builtin END
                ",
            )?;
            for f in filters {
                stmt.execute(params![
                    site_id,
                    f.id,
                    f.name,
                    f.jql,
                    if f.builtin { 1i32 } else { 0i32 },
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_saved_filters(&self) -> Result<Vec<SavedFilter>> {
        self.list_saved_filters_for_site(DEFAULT_SITE_ID)
    }

    pub fn list_saved_filters_for_site(&self, site_id: &str) -> Result<Vec<SavedFilter>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, name, jql, builtin
            FROM saved_filters
            WHERE site_id = ?1
            ORDER BY builtin DESC, name COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map([site_id], |row| {
            Ok(SavedFilter {
                id: row.get(0)?,
                name: row.get(1)?,
                jql: row.get(2)?,
                builtin: row.get::<_, i32>(3)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn map_project_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<ProjectSummary> {
    Ok(ProjectSummary {
        id: row.get(offset)?,
        key: row.get(offset + 1)?,
        name: row.get(offset + 2)?,
        project_type: row.get(offset + 3)?,
        lead_name: row.get(offset + 4)?,
        avatar_url: row.get(offset + 5)?,
        pinned: row.get::<_, i32>(offset + 6)? != 0,
    })
}

fn map_board_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<BoardSummary> {
    let columns_json: String = row.get(offset + 5)?;
    let columns: Vec<BoardColumn> = serde_json::from_str(&columns_json).unwrap_or_default();
    Ok(BoardSummary {
        id: row.get(offset)?,
        name: row.get(offset + 1)?,
        board_type: row.get(offset + 2)?,
        project_key: row.get(offset + 3)?,
        project_name: row.get(offset + 4)?,
        columns,
    })
}

pub(crate) fn compare_updated_at(incoming: &str, existing: &str) -> std::cmp::Ordering {
    let inc = incoming.trim();
    let ex = existing.trim();
    if ex.is_empty() {
        return std::cmp::Ordering::Greater;
    }
    if inc.is_empty() {
        // No incoming timestamp is no evidence of a newer revision.
        return std::cmp::Ordering::Less;
    }
    let parse_ts = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z"))
            .or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z"))
            .ok()
    };
    if let (Some(a), Some(b)) = (parse_ts(inc), parse_ts(ex)) {
        a.cmp(&b)
    } else {
        inc.cmp(ex)
    }
}

fn upsert_issue_row(
    conn: &Connection,
    site_id: &str,
    issue: &IssueSummary,
    board_id: Option<i64>,
    sprint_id: Option<i64>,
) -> Result<bool> {
    if let Some(existing_updated) = conn
        .query_row(
            "SELECT updated_at FROM issues WHERE site_id = ?1 AND key = ?2",
            params![site_id, &issue.key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        if compare_updated_at(&issue.updated_at, &existing_updated) == std::cmp::Ordering::Less {
            if let Some(bid) = board_id {
                conn.execute(
                    "UPDATE issues SET board_id = COALESCE(board_id, ?1), sprint_id = COALESCE(sprint_id, ?2) WHERE site_id = ?3 AND key = ?4",
                    params![bid, sprint_id, site_id, &issue.key],
                )?;
            }
            return Ok(false);
        }
    }

    let status_json = serde_json::to_string(&issue.status)?;
    let priority_json = issue
        .priority
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let issue_type_json = serde_json::to_string(&issue.issue_type)?;
    let assignee_json = issue
        .assignee
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let reporter_json = issue
        .reporter
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let labels_json = serde_json::to_string(&issue.labels)?;

    let sprint_update_sql = if board_id.is_some() {
        "sprint_id = excluded.sprint_id,\n            sprint_name = excluded.sprint_name,"
    } else {
        "sprint_id = COALESCE(excluded.sprint_id, issues.sprint_id),\n            sprint_name = COALESCE(excluded.sprint_name, issues.sprint_name),"
    };
    let sql = format!(
        "
        INSERT INTO issues (
            site_id, key, id, project_key, board_id, sprint_id, summary,
            status_json, priority_json, issue_type_json, assignee_json, reporter_json,
            parent_key, parent_summary, labels_json, story_points, sprint_name,
            comment_count, attachment_count, created_at, updated_at, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22
        )
        ON CONFLICT(site_id, key) DO UPDATE SET
            id = excluded.id,
            project_key = excluded.project_key,
            board_id = COALESCE(excluded.board_id, issues.board_id),
            {sprint_update_sql}
            summary = excluded.summary,
            status_json = excluded.status_json,
            priority_json = excluded.priority_json,
            issue_type_json = excluded.issue_type_json,
            assignee_json = excluded.assignee_json,
            reporter_json = excluded.reporter_json,
            parent_key = excluded.parent_key,
            parent_summary = excluded.parent_summary,
            labels_json = excluded.labels_json,
            story_points = excluded.story_points,
            comment_count = excluded.comment_count,
            attachment_count = excluded.attachment_count,
            created_at = excluded.created_at,
            updated_at = CASE
                WHEN TRIM(excluded.updated_at) = '' THEN issues.updated_at
                ELSE excluded.updated_at
            END,
            web_url = excluded.web_url
        "
    );
    conn.execute(
        &sql,
        params![
            site_id,
            issue.key,
            issue.id,
            issue.project_key,
            board_id,
            sprint_id,
            issue.summary,
            status_json,
            priority_json,
            issue_type_json,
            assignee_json,
            reporter_json,
            issue.parent_key,
            issue.parent_summary,
            labels_json,
            issue.story_points,
            issue.sprint_name,
            issue.comment_count as i64,
            issue.attachment_count as i64,
            issue.created_at,
            issue.updated_at,
            issue.web_url,
        ],
    )?;
    Ok(true)
}

fn refresh_issue_fts(
    conn: &Connection,
    site_id: &str,
    key: &str,
    project_key: &str,
    summary: &str,
    description_text: &str,
    labels_text: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM issues_fts WHERE site_id = ?1 AND issue_key = ?2",
        params![site_id, key],
    )?;
    conn.execute(
        "
        INSERT INTO issues_fts (site_id, issue_key, project_key, summary, description_text, labels_text)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![site_id, key, project_key, summary, description_text, labels_text],
    )?;
    Ok(())
}

fn map_issue_summary_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<IssueSummary> {
    let status_json: String = row.get(offset + 4)?;
    let priority_json: Option<String> = row.get(offset + 5)?;
    let issue_type_json: String = row.get(offset + 6)?;
    let assignee_json: Option<String> = row.get(offset + 7)?;
    let reporter_json: Option<String> = row.get(offset + 8)?;
    let labels_json: String = row.get(offset + 11)?;

    let status: IssueStatus = serde_json::from_str(&status_json).unwrap_or_default();
    let priority: Option<IssuePriority> = priority_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let issue_type: IssueType = serde_json::from_str(&issue_type_json).unwrap_or_default();
    let assignee: Option<UserSummary> = assignee_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let reporter: Option<UserSummary> = reporter_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let labels: Vec<String> = serde_json::from_str(&labels_json).unwrap_or_default();

    Ok(IssueSummary {
        id: row.get(offset)?,
        key: row.get(offset + 1)?,
        project_key: row.get(offset + 2)?,
        summary: row.get(offset + 3)?,
        status,
        priority,
        issue_type,
        assignee,
        reporter,
        parent_key: row.get(offset + 9)?,
        parent_summary: row.get(offset + 10)?,
        labels,
        story_points: row.get(offset + 12)?,
        sprint_id: row.get(offset + 13)?,
        sprint_name: row.get(offset + 14)?,
        comment_count: row.get::<_, i64>(offset + 15)?.max(0) as usize,
        attachment_count: row.get::<_, i64>(offset + 16)?.max(0) as usize,
        created_at: row.get(offset + 17)?,
        updated_at: row.get(offset + 18)?,
        web_url: row.get(offset + 19)?,
    })
}

fn run_issue_fts_search(
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
                i.site_id,
                i.key,
                i.project_key,
                i.summary,
                i.description_markdown,
                i.labels_json,
                i.status_json,
                i.updated_at,
                i.web_url
            FROM issues_fts f
            JOIN issues i ON i.site_id = f.site_id AND i.key = f.issue_key
            WHERE issues_fts MATCH ?1 AND f.site_id = ?2
            ORDER BY i.updated_at DESC, i.site_id ASC, i.key ASC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, sid, limit as i64], |row| {
            map_issue_hit_row(row, raw_query)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT
                i.site_id,
                i.key,
                i.project_key,
                i.summary,
                i.description_markdown,
                i.labels_json,
                i.status_json,
                i.updated_at,
                i.web_url
            FROM issues_fts f
            JOIN issues i ON i.site_id = f.site_id AND i.key = f.issue_key
            WHERE issues_fts MATCH ?1
            ORDER BY i.updated_at DESC, i.site_id ASC, i.key ASC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_issue_hit_row(row, raw_query)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn run_issue_like_search(
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
            "LOWER(key || ' ' || project_key || ' ' || summary || ' ' || description_markdown || ' ' || labels_json) LIKE ?{p} ESCAPE '\\'"
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
            key,
            project_key,
            summary,
            description_markdown,
            labels_json,
            status_json,
            updated_at,
            web_url
        FROM issues
        WHERE {}
        ORDER BY updated_at DESC, site_id ASC, key ASC
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
        map_issue_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_issue_hit_row(
    row: &Row<'_>,
    raw_query: &str,
) -> rusqlite::Result<(String, UnifiedSearchHit)> {
    let site_id: String = row.get(0)?;
    let key: String = row.get(1)?;
    let project_key: String = row.get(2)?;
    let summary: String = row.get(3)?;
    let desc: String = row.get(4)?;
    let labels_json: String = row.get(5)?;
    let status_json: String = row.get(6)?;
    let updated_at: String = row.get(7)?;
    let web_url: String = row.get(8)?;

    let status_name = serde_json::from_str::<IssueStatus>(&status_json)
        .map(|s| s.name)
        .unwrap_or_default();
    let snippet = build_contextual_snippet(&summary, &desc, &labels_json, raw_query, 120);

    Ok((
        site_id,
        UnifiedSearchHit {
            kind: SearchHitKind::JiraIssue,
            id_or_key: key,
            container: project_key,
            title: summary,
            snippet,
            status_or_version: status_name,
            updated_at,
            web_url,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::StatusCategory;

    #[test]
    fn single_site_my_work_excludes_done_and_other_assignees() {
        let storage = Storage::open_in_memory().unwrap();
        let user = |id: &str| UserSummary {
            id: id.into(),
            display_name: id.to_uppercase(),
            ..Default::default()
        };
        let with = |key: &str, assignee: Option<&str>, done: bool| {
            let mut it = sample_issue(key, "ATL", key, None);
            it.assignee = assignee.map(user);
            it.updated_at = format!("2026-11-2{}T00:00:00Z", key.len() % 10);
            if done {
                it.status = IssueStatus {
                    id: "3".into(),
                    name: "Done".into(),
                    category: StatusCategory::Done,
                };
            }
            it
        };
        storage
            .upsert_issues_for_site(
                "site-a",
                &[
                    with("ATL-1", Some("me-a"), false),
                    with("ATL-2", Some("someone"), false),
                    with("ATL-3", Some("me-a"), true),
                    with("ATL-4", None, false),
                ],
            )
            .unwrap();
        storage
            .upsert_issues_for_site("site-b", &[with("ATL-9", Some("me-a"), false)])
            .unwrap();

        let mine: Vec<String> = storage
            .list_my_work_for_site("site-a", Some("me-a"), 40)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(mine, vec!["ATL-1".to_string()]);

        // Without a known identity: open issues of this site only.
        let mut open: Vec<String> = storage
            .list_my_work_for_site("site-a", None, 40)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        open.sort();
        assert_eq!(open, vec!["ATL-1", "ATL-2", "ATL-4"]);

        // The all-sites listing keeps its previous shape.
        assert_eq!(storage.list_my_work_all_sites(40).unwrap().len(), 4);
    }

    fn sample_issue(
        key: &str,
        project_key: &str,
        summary: &str,
        sprint_id: Option<i64>,
    ) -> IssueSummary {
        IssueSummary {
            id: format!("id-{key}"),
            key: key.to_string(),
            project_key: project_key.to_string(),
            summary: summary.to_string(),
            status: IssueStatus {
                id: "1".into(),
                name: "In Progress".into(),
                category: StatusCategory::InProgress,
            },
            priority: None,
            issue_type: IssueType {
                id: "10".into(),
                name: "Task".into(),
                subtask: false,
                epic: false,
            },
            assignee: None,
            reporter: None,
            parent_key: None,
            parent_summary: None,
            labels: vec!["backend".into(), "인증".into()],
            story_points: Some(3.0),
            sprint_id,
            sprint_name: sprint_id.map(|s| format!("Sprint {s}")),
            comment_count: 1,
            attachment_count: 0,
            created_at: "2026-11-01T00:00:00Z".into(),
            updated_at: "2026-11-15T10:00:00Z".into(),
            web_url: format!("https://acme.atlassian.net/browse/{key}"),
        }
    }

    #[test]
    fn test_projects_pinned_preserved_and_builtin_filters() {
        let storage = Storage::open_in_memory().unwrap();
        let filters = storage.list_saved_filters().unwrap();
        assert_eq!(filters.len(), 4);
        assert!(filters
            .iter()
            .any(|f| f.id == "my_open" && f.name == "Assigned to me (open)"));

        let p = ProjectSummary {
            id: "100".into(),
            key: "PROJ".into(),
            name: "Core Platform".into(),
            project_type: "software".into(),
            lead_name: Some("Alice".into()),
            avatar_url: None,
            pinned: false,
        };
        storage.upsert_projects(&[p.clone()]).unwrap();
        storage.set_project_pinned("PROJ", true).unwrap();
        // Re-upsert with pinned: false should preserve pinned = true
        storage.upsert_projects(&[p]).unwrap();
        let list = storage.list_projects().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].pinned);
    }

    #[test]
    fn test_issue_detail_preserved_on_summary_upsert_and_korean_2char_search() {
        let storage = Storage::open_in_memory().unwrap();
        let summary = sample_issue("PROJ-1", "PROJ", "OAuth2 로그인 개선", Some(42));
        storage.upsert_issues(&[summary.clone()]).unwrap();

        let detail = IssueDetail {
            summary: summary.clone(),
            description_raw: String::new(),
            description_markdown: "무중단 배포 파이프라인 및 장애 복구 절차 정리".into(),
            description_blocks: Vec::new(),
            fix_versions: vec!["v1.0".into()],
            components: vec![],
            subtasks: vec![],
            linked_issues: vec![],
            linked_pages: vec![],
            attachments: vec![],
            comments: vec![],
            incomplete: vec![],
            custom_fields: Default::default(),
        };
        storage.upsert_issue_detail(&detail).unwrap();

        // List-level upsert updates summary without erasing cached detail_json / description_markdown
        let mut updated_summary = summary.clone();
        updated_summary.summary = "OAuth2 로그인 성능 개선".into();
        storage.upsert_issues(&[updated_summary]).unwrap();

        let loaded_detail = storage.get_issue_detail("PROJ-1").unwrap().unwrap();
        assert_eq!(
            loaded_detail.description_markdown,
            "무중단 배포 파이프라인 및 장애 복구 절차 정리"
        );

        // 2-char Korean query ("배포", "장애", "인증") must match
        let hits_deploy = storage.search_issues("배포", 10).unwrap();
        assert_eq!(hits_deploy.len(), 1);
        assert_eq!(hits_deploy[0].id_or_key, "PROJ-1");

        let hits_incident = storage.search_issues("장애", 10).unwrap();
        assert_eq!(hits_incident.len(), 1);

        // Exact key search
        let hits_key = storage.search_issues("PROJ-1", 10).unwrap();
        assert_eq!(hits_key.len(), 1);

        // >=3 char FTS5 trigram search
        let hits_trigram = storage.search_issues("파이프라인", 10).unwrap();
        assert_eq!(hits_trigram.len(), 1);

        // Delete removes from both table and FTS5
        storage.delete_issue("PROJ-1").unwrap();
        assert!(storage.search_issues("파이프라인", 10).unwrap().is_empty());
        assert!(storage.get_issue_detail("PROJ-1").unwrap().is_none());
    }

    #[test]
    fn test_board_sprint_issue_listing() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_boards(&[BoardSummary {
                id: 7,
                name: "Platform Scrum".into(),
                board_type: "scrum".into(),
                project_key: Some("PLAT".into()),
                project_name: Some("Platform".into()),
                columns: vec![],
            }])
            .unwrap();
        storage
            .upsert_sprints(&[SprintSummary {
                id: 77,
                board_id: 7,
                origin_board_id: None,
                name: "Sprint 77".into(),
                state: "active".into(),
                start_date: None,
                end_date: None,
                goal: None,
            }])
            .unwrap();
        let i1 = sample_issue("PLAT-10", "PLAT", "Active sprint work", Some(77));
        let i2 = sample_issue("PLAT-11", "PLAT", "Backlog work", None);
        storage.upsert_board_issues(7, None, &[i1, i2]).unwrap();

        assert_eq!(storage.list_board_issues(7, None, 50).unwrap().len(), 2);
        let sprint_only = storage.list_board_issues(7, Some(77), 50).unwrap();
        assert_eq!(sprint_only.len(), 1);
        assert_eq!(sprint_only[0].key, "PLAT-10");
    }

    #[test]
    fn test_site_scoped_board_issues_do_not_leak_across_sites() {
        let storage = Storage::open_in_memory().unwrap();
        for (site, summary) in [("acme-cloud", "Acme issue"), ("corp-dc", "Corp issue")] {
            storage
                .upsert_boards_for_site(
                    site,
                    &[BoardSummary {
                        id: 1,
                        name: format!("{site} Board"),
                        board_type: "scrum".into(),
                        project_key: Some("ENG".into()),
                        project_name: Some("Engineering".into()),
                        columns: vec![],
                    }],
                )
                .unwrap();
            storage
                .upsert_sprints_for_site(
                    site,
                    &[SprintSummary {
                        id: 10,
                        board_id: 1,
                        origin_board_id: None,
                        name: format!("{site} Sprint"),
                        state: "active".into(),
                        start_date: None,
                        end_date: None,
                        goal: None,
                    }],
                )
                .unwrap();
            storage
                .upsert_board_issues_for_site(
                    site,
                    1,
                    Some(10),
                    &[sample_issue("ENG-1", "ENG", summary, Some(10))],
                )
                .unwrap();
        }

        let acme = storage
            .list_board_issues_for_site("acme-cloud", 1, Some(10), 20)
            .unwrap();
        let corp = storage
            .list_board_issues_for_site("corp-dc", 1, Some(10), 20)
            .unwrap();
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0].summary, "Acme issue");
        assert_eq!(corp.len(), 1);
        assert_eq!(corp[0].summary, "Corp issue");

        // Deleting ENG-1 on acme-cloud must leave corp-dc ENG-1 intact
        storage.delete_issue_for_site("acme-cloud", "ENG-1").unwrap();
        assert!(storage
            .list_board_issues_for_site("acme-cloud", 1, Some(10), 20)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .list_board_issues_for_site("corp-dc", 1, Some(10), 20)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_monotonic_updated_at_split_brain_remotelink_preserve_and_board_prune() {
        use crate::models::LinkedConfluencePage;

        let storage = Storage::open_in_memory().unwrap();
        let mut v1 = sample_issue("PROJ-10", "PROJ", "v1 summary", Some(10));
        v1.updated_at = "2026-11-16T10:00:00.000+0000".into();

        let detail_v1 = IssueDetail {
            summary: v1.clone(),
            description_markdown: "v1 description".into(),
            linked_pages: vec![LinkedConfluencePage {
                page_id: "2001".into(),
                title: "Design Doc".into(),
                space_key: Some("ENG".into()),
                web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/2001".into(),
            }],
            ..Default::default()
        };
        storage
            .upsert_issue_detail_for_site("site-a", &detail_v1)
            .unwrap();
        assert!(!storage
            .is_issue_detail_stale_for_site("site-a", "PROJ-10")
            .unwrap());

        // Newer list summary updates authoritative fields and marks cached detail stale
        let mut v2 = v1.clone();
        v2.summary = "v2 remote summary".into();
        v2.status = IssueStatus {
            id: "13".into(),
            name: "In Review".into(),
            category: StatusCategory::InProgress,
        };
        v2.sprint_id = None;
        v2.sprint_name = None;
        v2.updated_at = "2026-11-17T10:00:00.000+0000".into();
        storage
            .upsert_board_issues_for_site("site-a", 11, None, &[v2.clone()])
            .unwrap();

        assert!(storage
            .is_issue_detail_stale_for_site("site-a", "PROJ-10")
            .unwrap());
        let reconciled = storage
            .get_issue_detail_for_site("site-a", "PROJ-10")
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.summary.summary, "v2 remote summary");
        assert_eq!(reconciled.summary.status.name, "In Review");
        // Non-sticky sprint cleared when remote returned None
        assert!(reconciled.summary.sprint_id.is_none());
        assert!(reconciled.summary.sprint_name.is_none());

        // Older v1 upsert must NOT regress v2
        storage.upsert_issues_for_site("site-a", &[v1]).unwrap();
        let after_old = storage
            .list_issues_for_site("site-a", None, 10)
            .unwrap();
        assert_eq!(after_old[0].summary, "v2 remote summary");

        // Transient /remotelink failure preserves cached linked_pages when merging
        let mut v2_detail_503 = IssueDetail {
            summary: v2.clone(),
            description_markdown: "v2 description".into(),
            linked_pages: vec![],
            ..Default::default()
        };
        storage
            .upsert_issue_detail_for_site_merging("site-a", &mut v2_detail_503, true)
            .unwrap();
        assert_eq!(v2_detail_503.linked_pages.len(), 1);
        assert_eq!(v2_detail_503.linked_pages[0].page_id, "2001");
        assert!(!storage
            .is_issue_detail_stale_for_site("site-a", "PROJ-10")
            .unwrap());

        // Pruning board 11 on site-a detaches PROJ-10 (row and detail stay) while preserving site-b
        storage
            .upsert_board_issues_for_site("site-b", 11, None, &[v2])
            .unwrap();
        let removed = storage
            .prune_board_issues_for_site("site-a", 11, None, &[])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(storage
            .get_issue_detail_for_site("site-a", "PROJ-10")
            .unwrap()
            .is_some());
        assert!(storage
            .list_board_issues_for_site("site-a", 11, None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .list_board_issues_for_site("site-b", 11, None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage.list_issues_for_site("site-b", None, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn record_board_issues_prunes_only_on_complete_first_page() {
        let storage = Storage::open_in_memory().unwrap();
        let issues: Vec<IssueSummary> = (1..=120)
            .map(|i| sample_issue(&format!("ENG-{i}"), "ENG", &format!("Issue {i}"), None))
            .collect();
        storage
            .upsert_board_issues_for_site("default", 7, None, &issues)
            .unwrap();

        let partial = ListPage {
            items: issues[0..100].to_vec(),
            has_more: Some(true),
            next_cursor: Some("100".into()),
        };
        storage
            .record_board_issues_page_for_site("default", 7, None, &partial, true)
            .unwrap();
        assert_eq!(
            storage
                .list_board_issues_for_site("default", 7, None, 200)
                .unwrap()
                .len(),
            120
        );

        // Second page with has_more=false does not prune earlier pages either
        let second = ListPage {
            items: issues[100..120].to_vec(),
            has_more: Some(false),
            next_cursor: None,
        };
        storage
            .record_board_issues_page_for_site("default", 7, None, &second, false)
            .unwrap();
        assert_eq!(
            storage
                .list_board_issues_for_site("default", 7, None, 200)
                .unwrap()
                .len(),
            120
        );

        // Complete first page detaches missing issues from the board; their rows stay cached
        let complete_first = ListPage {
            items: issues[0..10].to_vec(),
            has_more: Some(false),
            next_cursor: None,
        };
        storage
            .record_board_issues_page_for_site("default", 7, None, &complete_first, true)
            .unwrap();
        assert_eq!(
            storage
                .list_board_issues_for_site("default", 7, None, 200)
                .unwrap()
                .len(),
            10
        );
        assert_eq!(
            storage.count_issues_for_site("default", Some("ENG")).unwrap(),
            120
        );
    }

    #[test]
    fn count_issues_matches_list_filter() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_issues_for_site(
                "site-a",
                &[
                    sample_issue("ENG-1", "ENG", "A1", None),
                    sample_issue("ENG-2", "ENG", "A2", None),
                    sample_issue("OPS-1", "OPS", "A3", None),
                ],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "site-b",
                &[
                    sample_issue("ENG-1", "ENG", "B1", None),
                    sample_issue("OPS-1", "OPS", "B2", None),
                ],
            )
            .unwrap();

        for (site, proj) in [
            ("site-a", None),
            ("site-a", Some("ENG")),
            ("site-a", Some("OPS")),
            ("site-b", None),
            ("site-b", Some("ENG")),
            ("site-b", Some("NONE")),
        ] {
            let expected = storage.list_issues_for_site(site, proj, 1000).unwrap().len();
            assert_eq!(storage.count_issues_for_site(site, proj).unwrap(), expected);
        }
    }

    #[test]
    fn list_issues_filtered_applies_status_before_limit() {
        let storage = Storage::open_in_memory().unwrap();
        // ENG-11..=ENG-60 are the 50 newest rows and Done; ENG-1..=ENG-10 are older and open.
        let issues: Vec<IssueSummary> = (1..=60)
            .map(|i| {
                let mut issue = sample_issue(&format!("ENG-{i}"), "ENG", &format!("Issue {i}"), None);
                issue.updated_at = format!("2026-11-20T10:{:02}:{:02}Z", i / 60, i % 60);
                if i > 10 {
                    issue.status = IssueStatus {
                        id: "3".into(),
                        name: "Done".into(),
                        category: StatusCategory::Done,
                    };
                }
                if i <= 3 {
                    issue.assignee = Some(UserSummary {
                        id: "acct-me".into(),
                        display_name: "Me".into(),
                        ..Default::default()
                    });
                }
                issue
            })
            .collect();
        storage.upsert_issues_for_site("site-a", &issues).unwrap();

        // Filtering after the limit is what used to return nothing.
        let newest = storage.list_issues_for_site("site-a", None, 50).unwrap();
        assert!(newest.iter().all(|i| i.status.name == "Done"));

        let in_progress = IssueListFilter {
            status_contains: Some(" progress "),
            ..Default::default()
        };
        let open = storage
            .list_issues_filtered_for_site("site-a", &in_progress, 50)
            .unwrap();
        assert_eq!(open.len(), 10);
        assert_eq!(storage.count_issues_filtered_for_site("site-a", &in_progress).unwrap(), 10);
        assert_eq!(
            storage.count_issues_filtered_for_site("site-a", &IssueListFilter::default()).unwrap(),
            60
        );
        assert!(open.iter().all(|i| i.status.name == "In Progress"));
        let keys = |rows: &[IssueSummary]| rows.iter().map(|i| i.key.clone()).collect::<Vec<_>>();
        assert_eq!(
            keys(&storage.list_issues_filtered_for_site("site-a", &in_progress, 3).unwrap()),
            ["ENG-10", "ENG-9", "ENG-8"],
            "newest matches first, limit counts matches"
        );

        let open_only = IssueListFilter { open_only: true, ..Default::default() };
        assert_eq!(storage.list_issues_filtered_for_site("site-a", &open_only, 50).unwrap().len(), 10);

        let mine = IssueListFilter {
            project: Some("ENG"),
            assignee_id: Some("acct-me"),
            ..Default::default()
        };
        assert_eq!(
            keys(&storage.list_issues_filtered_for_site("site-a", &mine, 50).unwrap()),
            ["ENG-3", "ENG-2", "ENG-1"]
        );
        let other_project = IssueListFilter { project: Some("OPS"), ..Default::default() };
        assert!(storage.list_issues_filtered_for_site("site-a", &other_project, 50).unwrap().is_empty());
        assert!(storage.list_issues_filtered_for_site("site-b", &open_only, 50).unwrap().is_empty());
    }

    /// `(board_id, sprint_id, sprint_name)` of a cached issue row, or `None` when the row is gone.
    fn issue_row_refs(
        storage: &Storage,
        site_id: &str,
        key: &str,
    ) -> Option<(Option<i64>, Option<i64>, Option<String>)> {
        let conn = storage.lock_conn().unwrap();
        conn.query_row(
            "SELECT board_id, sprint_id, sprint_name FROM issues WHERE site_id = ?1 AND key = ?2",
            params![site_id, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .unwrap()
    }

    fn board_sprint_links(storage: &Storage, site_id: &str) -> Vec<(i64, i64)> {
        let conn = storage.lock_conn().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT board_id, sprint_id FROM board_sprints WHERE site_id = ?1 ORDER BY board_id, sprint_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([site_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn prune_board_issues_detaches_instead_of_deleting() {
        let storage = Storage::open_in_memory().unwrap();
        let kept = sample_issue("ENG-1", "ENG", "Kept sprint work", Some(42));
        let gone = sample_issue("ENG-2", "ENG", "Carried over migration task", Some(42));
        storage
            .upsert_board_issues_for_site("site-a", 7, Some(42), &[kept, gone.clone()])
            .unwrap();
        storage
            .upsert_issue_detail_for_site(
                "site-a",
                &IssueDetail {
                    summary: gone,
                    description_markdown: "Rollback checklist".into(),
                    ..Default::default()
                },
            )
            .unwrap();

        let detached = storage
            .prune_board_issues_for_site("site-a", 7, Some(42), &["ENG-1".to_string()])
            .unwrap();
        assert_eq!(detached, 1);

        // The row survives with its sprint cleared and its board kept.
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-2"),
            Some((Some(7), None, None))
        );
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-1"),
            Some((Some(7), Some(42), Some("Sprint 42".to_string())))
        );
        // Cached detail and the search entry survive too.
        let detail = storage
            .get_issue_detail_for_site("site-a", "ENG-2")
            .unwrap()
            .unwrap();
        assert_eq!(detail.description_markdown, "Rollback checklist");
        let hits = storage
            .search_issues_scoped("migration", Some("site-a"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.id_or_key, "ENG-2");
        assert_eq!(
            storage
                .list_issues_for_site("site-a", Some("ENG"), 10)
                .unwrap()
                .len(),
            2
        );
        // It is no longer part of the sprint view.
        let in_sprint = storage
            .list_board_issues_for_site("site-a", 7, Some(42), 50)
            .unwrap();
        assert_eq!(in_sprint.len(), 1);
        assert_eq!(in_sprint[0].key, "ENG-1");
    }

    #[test]
    fn prune_board_scope_clears_board_id_only() {
        let storage = Storage::open_in_memory().unwrap();
        let on_board = [
            sample_issue("ENG-1", "ENG", "Still on the board", Some(42)),
            sample_issue("ENG-2", "ENG", "Left the board", Some(42)),
        ];
        storage
            .upsert_board_issues_for_site("site-a", 7, None, &on_board)
            .unwrap();
        storage
            .upsert_board_issues_for_site("site-b", 7, None, &on_board)
            .unwrap();
        storage
            .upsert_board_issues_for_site(
                "site-a",
                8,
                None,
                &[sample_issue("ENG-3", "ENG", "Other board", None)],
            )
            .unwrap();

        let detached = storage
            .prune_board_issues_for_site("site-a", 7, None, &["ENG-1".to_string()])
            .unwrap();
        assert_eq!(detached, 1);

        // Only the board link is cleared; the sprint stays.
        let sprint = Some("Sprint 42".to_string());
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-2"),
            Some((None, Some(42), sprint.clone()))
        );
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-1"),
            Some((Some(7), Some(42), sprint.clone()))
        );
        // Other boards and other sites are untouched.
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-3"),
            Some((Some(8), None, None))
        );
        assert_eq!(
            issue_row_refs(&storage, "site-b", "ENG-2"),
            Some((Some(7), Some(42), sprint))
        );
        // No board or sprint row is cached here, so the board listing has no project or sprint
        // fallback to bring ENG-2 back.
        let listed = storage
            .list_board_issues_for_site("site-a", 7, None, 50)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "ENG-1");
    }

    #[test]
    fn shared_sprint_is_listed_under_both_boards() {
        let storage = Storage::open_in_memory().unwrap();
        let sprint = |id: i64, board_id: i64| SprintSummary {
            id,
            board_id,
            origin_board_id: Some(10),
            name: format!("Sprint {id}"),
            state: "active".into(),
            ..Default::default()
        };
        storage
            .upsert_sprints_for_site("site-a", &[sprint(7, 10)])
            .unwrap();
        storage
            .upsert_sprints_for_site("site-a", &[sprint(7, 20)])
            .unwrap();

        for board in [10, 20] {
            let listed = storage.list_sprints_for_site("site-a", board).unwrap();
            assert_eq!(listed.len(), 1, "board {board}");
            assert_eq!(listed[0].id, 7);
            assert_eq!(listed[0].board_id, board);
            assert_eq!(listed[0].origin_board_id, Some(10));
        }
        assert!(storage.list_sprints_for_site("site-a", 30).unwrap().is_empty());
        assert!(storage.list_sprints_for_site("site-b", 10).unwrap().is_empty());
        assert_eq!(board_sprint_links(&storage, "site-a"), vec![(10, 7), (20, 7)]);

        // A sprint seen only through a borrowing board is linked to its owning board as well.
        storage
            .upsert_sprints_for_site("site-a", &[sprint(8, 20)])
            .unwrap();
        let owner_ids: Vec<i64> = storage
            .list_sprints_for_site("site-a", 10)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(owner_ids, vec![8, 7]);
    }

    #[test]
    fn board_sprints_backfilled_from_existing_sprints() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache_without_board_sprints.db");
        {
            // A schema v2 cache written before `board_sprints` existed.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE sprints (
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
                INSERT INTO sprints (site_id, id, board_id, name, state)
                VALUES ('default', 77, 7, 'Sprint 77', 'active'),
                       ('site-a', 78, 9, 'Sprint 78', 'future');
                PRAGMA user_version = 2;
                ",
            )
            .unwrap();
        }

        let storage = Storage::open(&db_path).unwrap();
        let default_sprints = storage.list_sprints_for_site(DEFAULT_SITE_ID, 7).unwrap();
        assert_eq!(default_sprints.len(), 1);
        assert_eq!(default_sprints[0].id, 77);
        assert_eq!(default_sprints[0].board_id, 7);
        let site_a_sprints = storage.list_sprints_for_site("site-a", 9).unwrap();
        assert_eq!(site_a_sprints.len(), 1);
        assert_eq!(site_a_sprints[0].id, 78);
        {
            let conn = storage.lock_conn().unwrap();
            let version: i32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2);
        }

        // Re-opening repeats the backfill without duplicating links.
        drop(storage);
        let reopened = Storage::open(&db_path).unwrap();
        assert_eq!(board_sprint_links(&reopened, DEFAULT_SITE_ID), vec![(7, 77)]);
        assert_eq!(board_sprint_links(&reopened, "site-a"), vec![(9, 78)]);
    }

    #[test]
    fn compare_updated_at_empty_incoming_is_older() {
        use std::cmp::Ordering;

        let ts = "2026-11-15T10:00:00Z";
        assert_eq!(compare_updated_at("", ts), Ordering::Less);
        assert_eq!(compare_updated_at("   ", ts), Ordering::Less);
        // Unchanged orderings.
        assert_eq!(compare_updated_at("", ""), Ordering::Greater);
        assert_eq!(compare_updated_at(ts, ""), Ordering::Greater);
        assert_eq!(compare_updated_at(ts, ts), Ordering::Equal);
        assert_eq!(compare_updated_at("2026-11-14T10:00:00Z", ts), Ordering::Less);
        assert_eq!(compare_updated_at("2026-11-16T10:00:00Z", ts), Ordering::Greater);
    }

    #[test]
    fn upsert_summary_with_empty_updated_at_keeps_existing_timestamp() {
        let storage = Storage::open_in_memory().unwrap();
        let first = sample_issue("ENG-1", "ENG", "Original summary", None);
        storage
            .upsert_issues_for_site("site-a", &[first.clone()])
            .unwrap();

        let mut untimed = first.clone();
        untimed.summary = "Untimed copy".into();
        untimed.updated_at = String::new();
        storage
            .upsert_issues_for_site("site-a", &[untimed.clone()])
            .unwrap();
        assert_eq!(
            storage
                .get_issue_row_updated_at_for_site("site-a", "ENG-1")
                .unwrap()
                .as_deref(),
            Some(first.updated_at.as_str())
        );

        // Listed on a board without a timestamp: only the board/sprint link is filled in.
        storage
            .upsert_board_issues_for_site("site-a", 7, Some(42), &[untimed])
            .unwrap();
        assert_eq!(
            issue_row_refs(&storage, "site-a", "ENG-1"),
            Some((Some(7), Some(42), None))
        );
        let rows = storage.list_issues_for_site("site-a", None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "Original summary");
        assert_eq!(rows[0].updated_at, first.updated_at);
    }

    #[test]
    fn upsert_detail_with_empty_updated_at_is_stored_and_keeps_timestamp() {
        let storage = Storage::open_in_memory().unwrap();
        let first = sample_issue("ENG-1", "ENG", "Original summary", None);
        storage
            .upsert_issues_for_site("site-a", &[first.clone()])
            .unwrap();

        let mut untimed = first.clone();
        untimed.updated_at = String::new();
        storage
            .upsert_issue_detail_for_site(
                "site-a",
                &IssueDetail {
                    summary: untimed,
                    description_markdown: "Fetched without a timestamp".into(),
                    ..Default::default()
                },
            )
            .unwrap();

        let detail = storage
            .get_issue_detail_for_site("site-a", "ENG-1")
            .unwrap()
            .expect("detail without a timestamp must still be stored");
        assert_eq!(detail.description_markdown, "Fetched without a timestamp");
        assert_eq!(detail.summary.updated_at, first.updated_at);
        assert_eq!(
            storage
                .get_issue_detail_updated_at_for_site("site-a", "ENG-1")
                .unwrap()
                .as_deref(),
            Some(first.updated_at.as_str())
        );
        assert!(!storage
            .is_issue_detail_stale_for_site("site-a", "ENG-1")
            .unwrap());
    }

    #[test]
    fn update_issue_comments_keeps_server_total() {
        let storage = Storage::open_in_memory().unwrap();
        let issue = sample_issue("ENG-1", "ENG", "Busy thread", None);
        storage
            .upsert_issue_detail_for_site(
                "site-a",
                &IssueDetail {
                    summary: issue,
                    ..Default::default()
                },
            )
            .unwrap();
        let comments: Vec<IssueComment> = (1..=3)
            .map(|i| IssueComment {
                id: i.to_string(),
                issue_key: "ENG-1".into(),
                body_markdown: format!("Comment {i}"),
                ..Default::default()
            })
            .collect();
        let cached = |storage: &Storage| {
            let detail = storage
                .get_issue_detail_for_site("site-a", "ENG-1")
                .unwrap()
                .unwrap();
            let row_count = storage.list_issues_for_site("site-a", None, 10).unwrap()[0].comment_count;
            assert_eq!(detail.summary.comment_count, row_count);
            (detail.comments.len(), row_count)
        };

        // The server reported 130 comments; only 3 were loaded.
        storage
            .update_issue_comments_for_site_with_total("site-a", "ENG-1", &comments, Some(130))
            .unwrap();
        assert_eq!(cached(&storage), (3, 130));

        // A total below what was loaded never undercounts.
        storage
            .update_issue_comments_for_site_with_total("site-a", "ENG-1", &comments, Some(2))
            .unwrap();
        assert_eq!(cached(&storage), (3, 3));

        // Without a total the count is the number of comments, as before.
        storage
            .update_issue_comments_for_site_with_total("site-a", "ENG-1", &comments, Some(130))
            .unwrap();
        storage
            .update_issue_comments_for_site("site-a", "ENG-1", &comments[..2])
            .unwrap();
        assert_eq!(cached(&storage), (2, 2));
    }
}
