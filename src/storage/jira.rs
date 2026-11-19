//! Jira projects, boards, sprints, issues, details, filters, and `issues_fts` persistence (multi-site aware).

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};

use crate::{
    adf,
    models::{
        BoardColumn, BoardSummary, IssueDetail, IssuePriority, IssueStatus, IssueSummary,
        IssueType, ProjectSummary, SavedFilter, SearchHitKind, SprintSummary, UnifiedSearchHit,
        UserSummary,
    },
    storage::{
        db::{build_contextual_snippet, build_fts_query, escape_like, DEFAULT_SITE_ID},
        Storage,
    },
};

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
            for s in sprints {
                stmt.execute(params![
                    site_id,
                    s.id,
                    s.board_id,
                    s.name,
                    s.state,
                    s.start_date,
                    s.end_date,
                    s.goal,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_sprints(&self, board_id: i64) -> Result<Vec<SprintSummary>> {
        self.list_sprints_for_site(DEFAULT_SITE_ID, board_id)
    }

    pub fn list_sprints_for_site(
        &self,
        site_id: &str,
        board_id: i64,
    ) -> Result<Vec<SprintSummary>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, board_id, name, state, start_date, end_date, goal
            FROM sprints
            WHERE site_id = ?1 AND board_id = ?2
            ORDER BY
                CASE state WHEN 'active' THEN 0 WHEN 'future' THEN 1 ELSE 2 END,
                id DESC
            ",
        )?;
        let rows = stmt.query_map(params![site_id, board_id], |row| {
            Ok(SprintSummary {
                id: row.get(0)?,
                board_id: row.get(1)?,
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
                upsert_issue_row(&tx, site_id, issue, board_id, eff_sprint_id)?;
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
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let issue = &detail.summary;
            upsert_issue_row(&tx, site_id, issue, None, issue.sprint_id)?;
            let detail_json =
                serde_json::to_string(detail).context("Failed to serialize IssueDetail")?;
            tx.execute(
                "UPDATE issues SET description_markdown = ?1, detail_json = ?2 WHERE site_id = ?3 AND key = ?4",
                params![detail.description_markdown, detail_json, site_id, issue.key],
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
        let raw: Option<String> = conn
            .query_row(
                "SELECT detail_json FROM issues WHERE site_id = ?1 AND key = ?2",
                params![site_id, key],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match raw {
            Some(json_str) if !json_str.trim().is_empty() => {
                let detail = serde_json::from_str::<IssueDetail>(&json_str)
                    .context("Failed to deserialize IssueDetail")?;
                Ok(Some(detail))
            }
            _ => Ok(None),
        }
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
        if limit == 0 {
            return Ok(Vec::new());
        }
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
            ORDER BY i.updated_at DESC, i.site_id ASC, i.key ASC
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

fn upsert_issue_row(
    conn: &Connection,
    site_id: &str,
    issue: &IssueSummary,
    board_id: Option<i64>,
    sprint_id: Option<i64>,
) -> Result<()> {
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

    conn.execute(
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
            sprint_id = COALESCE(excluded.sprint_id, issues.sprint_id),
            summary = excluded.summary,
            status_json = excluded.status_json,
            priority_json = excluded.priority_json,
            issue_type_json = excluded.issue_type_json,
            assignee_json = excluded.assignee_json,
            reporter_json = excluded.reporter_json,
            parent_key = COALESCE(excluded.parent_key, issues.parent_key),
            parent_summary = COALESCE(excluded.parent_summary, issues.parent_summary),
            labels_json = excluded.labels_json,
            story_points = COALESCE(excluded.story_points, issues.story_points),
            sprint_name = COALESCE(excluded.sprint_name, issues.sprint_name),
            comment_count = excluded.comment_count,
            attachment_count = excluded.attachment_count,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            web_url = excluded.web_url
        ",
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
    Ok(())
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
            .any(|f| f.id == "my_open" && f.name == "내게 할당된 미해결"));

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
}
