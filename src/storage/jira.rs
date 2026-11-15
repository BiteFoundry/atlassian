//! Jira projects, boards, sprints, issues, details, filters, and `issues_fts` persistence.

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
        db::{build_contextual_snippet, build_fts_query, escape_like},
        Storage,
    },
};

impl Storage {
    pub fn upsert_projects(&self, projects: &[ProjectSummary]) -> Result<()> {
        if projects.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO projects (key, id, name, project_type, lead_name, avatar_url, pinned)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(key) DO UPDATE SET
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
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, key, name, project_type, lead_name, avatar_url, pinned
            FROM projects
            ORDER BY pinned DESC, name COLLATE NOCASE ASC, key ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectSummary {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                project_type: row.get(3)?,
                lead_name: row.get(4)?,
                avatar_url: row.get(5)?,
                pinned: row.get::<_, i32>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_project_pinned(&self, key: &str, pinned: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE projects SET pinned = ?1 WHERE key = ?2",
            params![if pinned { 1i32 } else { 0i32 }, key],
        )?;
        Ok(())
    }

    pub fn upsert_boards(&self, boards: &[BoardSummary]) -> Result<()> {
        if boards.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO boards (id, name, board_type, project_key, project_name, columns_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(id) DO UPDATE SET
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
        let conn = self.lock_conn()?;
        let map_row = |row: &Row<'_>| -> rusqlite::Result<BoardSummary> {
            let columns_json: String = row.get(5)?;
            let columns: Vec<BoardColumn> =
                serde_json::from_str(&columns_json).unwrap_or_default();
            Ok(BoardSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                board_type: row.get(2)?,
                project_key: row.get(3)?,
                project_name: row.get(4)?,
                columns,
            })
        };

        if let Some(pk) = project_key {
            let mut stmt = conn.prepare(
                "
                SELECT id, name, board_type, project_key, project_name, columns_json
                FROM boards
                WHERE project_key = ?1
                ORDER BY name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map([pk], map_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "
                SELECT id, name, board_type, project_key, project_name, columns_json
                FROM boards
                ORDER BY name COLLATE NOCASE ASC, id ASC
                ",
            )?;
            let rows = stmt.query_map([], map_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn upsert_sprints(&self, sprints: &[SprintSummary]) -> Result<()> {
        if sprints.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO sprints (id, board_id, name, state, start_date, end_date, goal)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(id) DO UPDATE SET
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
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, board_id, name, state, start_date, end_date, goal
            FROM sprints
            WHERE board_id = ?1
            ORDER BY
                CASE state WHEN 'active' THEN 0 WHEN 'future' THEN 1 ELSE 2 END,
                id DESC
            ",
        )?;
        let rows = stmt.query_map([board_id], |row| {
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
        self.upsert_issues_inner(None, None, issues)
    }

    /// Upserts issues retrieved from a specific Agile board/sprint context.
    pub fn upsert_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        issues: &[IssueSummary],
    ) -> Result<()> {
        self.upsert_issues_inner(Some(board_id), sprint_id, issues)
    }

    fn upsert_issues_inner(
        &self,
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
                upsert_issue_row(&tx, issue, board_id, eff_sprint_id)?;
                let existing_desc: String = tx
                    .query_row(
                        "SELECT description_markdown FROM issues WHERE key = ?1",
                        [&issue.key],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                refresh_issue_fts(
                    &tx,
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
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let issue = &detail.summary;
            upsert_issue_row(&tx, issue, None, issue.sprint_id)?;
            let detail_json = serde_json::to_string(detail).context("IssueDetail 직렬화 실패")?;
            tx.execute(
                "UPDATE issues SET description_markdown = ?1, detail_json = ?2 WHERE key = ?3",
                params![detail.description_markdown, detail_json, issue.key],
            )?;
            let plain_desc = if !detail.description_blocks.is_empty() {
                adf::blocks_to_plain_text(&detail.description_blocks)
            } else {
                detail.description_markdown.clone()
            };
            refresh_issue_fts(
                &tx,
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
        let conn = self.lock_conn()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT detail_json FROM issues WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match raw {
            Some(json_str) if !json_str.trim().is_empty() => {
                let detail =
                    serde_json::from_str::<IssueDetail>(&json_str).context("IssueDetail 역직렬화 실패")?;
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
                WHERE project_key = ?1
                ORDER BY updated_at DESC, key ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![pk, limit as i64], map_issue_summary_row)?;
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
                ORDER BY updated_at DESC, key ASC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit as i64], map_issue_summary_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn list_board_issues(
        &self,
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
                WHERE sprint_id = ?1
                  AND (
                      board_id = ?2
                      OR board_id IS NULL
                      OR sprint_id IN (SELECT id FROM sprints WHERE board_id = ?2)
                  )
                ORDER BY updated_at DESC, key ASC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(params![sid, board_id, limit as i64], map_issue_summary_row)?;
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
                WHERE board_id = ?1
                   OR sprint_id IN (SELECT id FROM sprints WHERE board_id = ?1)
                   OR (
                       board_id IS NULL
                       AND project_key = (SELECT project_key FROM boards WHERE id = ?1)
                   )
                ORDER BY updated_at DESC, key ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![board_id, limit as i64], map_issue_summary_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    pub fn delete_issue(&self, key: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM issues_fts WHERE issue_key = ?1", [key])?;
        tx.execute("DELETE FROM issues WHERE key = ?1", [key])?;
        tx.commit()?;
        Ok(())
    }

    pub fn search_issues(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn()?;
        let mut results: Vec<UnifiedSearchHit> = Vec::new();
        let mut seen = HashSet::<String>::new();

        if trimmed.chars().count() >= 3 {
            if let Some(fts_q) = build_fts_query(trimmed) {
                if let Ok(fts_hits) = run_issue_fts_search(&conn, &fts_q, trimmed, limit) {
                    for hit in fts_hits {
                        if seen.insert(hit.id_or_key.clone()) {
                            results.push(hit);
                        }
                    }
                }
            }
        }

        if results.len() < limit {
            let like_hits = run_issue_like_search(&conn, trimmed, limit)?;
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

    pub fn upsert_saved_filters(&self, filters: &[SavedFilter]) -> Result<()> {
        if filters.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO saved_filters (id, name, jql, builtin)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    jql = excluded.jql,
                    builtin = CASE WHEN saved_filters.builtin != 0 THEN 1 ELSE excluded.builtin END
                ",
            )?;
            for f in filters {
                stmt.execute(params![
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
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT id, name, jql, builtin
            FROM saved_filters
            ORDER BY builtin DESC, name COLLATE NOCASE ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
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

fn upsert_issue_row(
    conn: &Connection,
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
            key, id, project_key, board_id, sprint_id, summary,
            status_json, priority_json, issue_type_json, assignee_json, reporter_json,
            parent_key, parent_summary, labels_json, story_points, sprint_name,
            comment_count, attachment_count, created_at, updated_at, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21
        )
        ON CONFLICT(key) DO UPDATE SET
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
    key: &str,
    project_key: &str,
    summary: &str,
    description_text: &str,
    labels_text: &str,
) -> Result<()> {
    conn.execute("DELETE FROM issues_fts WHERE issue_key = ?1", [key])?;
    conn.execute(
        "
        INSERT INTO issues_fts (issue_key, project_key, summary, description_text, labels_text)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
        params![key, project_key, summary, description_text, labels_text],
    )?;
    Ok(())
}

fn map_issue_summary_row(row: &Row<'_>) -> rusqlite::Result<IssueSummary> {
    let status_json: String = row.get(4)?;
    let priority_json: Option<String> = row.get(5)?;
    let issue_type_json: String = row.get(6)?;
    let assignee_json: Option<String> = row.get(7)?;
    let reporter_json: Option<String> = row.get(8)?;
    let labels_json: String = row.get(11)?;

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
        id: row.get(0)?,
        key: row.get(1)?,
        project_key: row.get(2)?,
        summary: row.get(3)?,
        status,
        priority,
        issue_type,
        assignee,
        reporter,
        parent_key: row.get(9)?,
        parent_summary: row.get(10)?,
        labels,
        story_points: row.get(12)?,
        sprint_id: row.get(13)?,
        sprint_name: row.get(14)?,
        comment_count: row.get::<_, i64>(15)?.max(0) as usize,
        attachment_count: row.get::<_, i64>(16)?.max(0) as usize,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        web_url: row.get(19)?,
    })
}

fn run_issue_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    limit: usize,
) -> Result<Vec<UnifiedSearchHit>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            i.key,
            i.project_key,
            i.summary,
            i.description_markdown,
            i.labels_json,
            i.status_json,
            i.updated_at,
            i.web_url
        FROM issues_fts f
        JOIN issues i ON i.key = f.issue_key
        WHERE issues_fts MATCH ?1
        ORDER BY i.updated_at DESC, i.key ASC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
        map_issue_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn run_issue_like_search(
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
            "LOWER(key || ' ' || project_key || ' ' || summary || ' ' || description_markdown || ' ' || labels_json) LIKE ?{p} ESCAPE '\\'"
        ));
    }
    let limit_idx = tokens.len() + 1;
    let sql = format!(
        "
        SELECT
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
        ORDER BY updated_at DESC, key ASC
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
        map_issue_hit_row(row, raw_query)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_issue_hit_row(row: &Row<'_>, raw_query: &str) -> rusqlite::Result<UnifiedSearchHit> {
    let key: String = row.get(0)?;
    let project_key: String = row.get(1)?;
    let summary: String = row.get(2)?;
    let desc: String = row.get(3)?;
    let labels_json: String = row.get(4)?;
    let status_json: String = row.get(5)?;
    let updated_at: String = row.get(6)?;
    let web_url: String = row.get(7)?;

    let status_name = serde_json::from_str::<IssueStatus>(&status_json)
        .map(|s| s.name)
        .unwrap_or_default();
    let snippet = build_contextual_snippet(&summary, &desc, &labels_json, raw_query, 120);

    Ok(UnifiedSearchHit {
        kind: SearchHitKind::JiraIssue,
        id_or_key: key,
        container: project_key,
        title: summary,
        snippet,
        status_or_version: status_name,
        updated_at,
        web_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::StatusCategory;

    fn sample_issue(key: &str, project_key: &str, summary: &str, sprint_id: Option<i64>) -> IssueSummary {
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
        assert!(filters.iter().any(|f| f.id == "my_open" && f.name == "내게 할당된 미해결"));

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
}
