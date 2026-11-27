//! Unified SQLite WAL + FTS5 local cache (`cache.db`) for multi-site Jira projects, boards, sprints, issues, and Confluence spaces & pages.

pub mod confluence;
pub mod db;
pub mod jira;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::models::{UnifiedSearchHit, UserSummary};

pub use db::{DEFAULT_SITE_ID, SCHEMA_VERSION};
pub use jira::IssueListFilter;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCounts {
    pub projects: usize,
    pub boards: usize,
    pub issues: usize,
    pub spaces: usize,
    pub pages: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRecord {
    pub site_id: String,
    pub name: String,
    pub site_url: String,
    pub deployment: String,
    pub enabled: bool,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteScoped<T> {
    pub site_id: String,
    pub item: T,
}

impl<T> SiteScoped<T> {
    pub fn new(site_id: impl Into<String>, item: T) -> Self {
        Self {
            site_id: site_id.into(),
            item,
        }
    }
}

impl<T> From<(String, T)> for SiteScoped<T> {
    fn from((site_id, item): (String, T)) -> Self {
        Self { site_id, item }
    }
}

pub(crate) const MAX_READER_POOL_SIZE: usize = 4;

enum ReadConnInner<'a> {
    Pooled {
        conn: Option<Connection>,
        pool: &'a Arc<Mutex<Vec<Connection>>>,
    },
    Writer(std::sync::MutexGuard<'a, Connection>),
}

pub(crate) struct ReadConnGuard<'a>(ReadConnInner<'a>);

impl std::ops::Deref for ReadConnGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            ReadConnInner::Pooled { conn, .. } => conn
                .as_ref()
                .expect("pooled reader connection available until Drop"),
            ReadConnInner::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConnGuard<'_> {
    fn drop(&mut self) {
        if let ReadConnInner::Pooled { conn, pool } = &mut self.0 {
            if let Some(c) = conn.take() {
                if let Ok(mut guard) = pool.lock() {
                    if guard.len() < MAX_READER_POOL_SIZE {
                        guard.push(c);
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    pub(crate) conn: Arc<Mutex<Connection>>,
    pub(crate) readers: Option<Arc<Mutex<Vec<Connection>>>>,
    pub(crate) path: Option<PathBuf>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        db::open_file(path)
    }

    pub fn open_in_memory() -> Result<Self> {
        db::open_memory()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("SQLite connection mutex poisoned"))
    }

    pub(crate) fn read_conn(&self) -> Result<ReadConnGuard<'_>> {
        if let (Some(pool), Some(path)) = (&self.readers, &self.path) {
            let existing = {
                let mut guard = pool
                    .lock()
                    .map_err(|_| anyhow!("SQLite reader pool mutex poisoned"))?;
                guard.pop()
            };
            let conn = match existing {
                Some(c) => c,
                None => {
                    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let c = Connection::open_with_flags(path, flags)?;
                    db::init_reader_connection(&c)?;
                    c
                }
            };
            Ok(ReadConnGuard(ReadConnInner::Pooled {
                conn: Some(conn),
                pool,
            }))
        } else {
            Ok(ReadConnGuard(ReadConnInner::Writer(self.lock_conn()?)))
        }
    }

    pub fn upsert_site(&self, site: &SiteRecord) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "
            INSERT INTO sites (site_id, name, site_url, deployment, enabled, last_synced_at, last_error)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(site_id) DO UPDATE SET
                name = excluded.name,
                site_url = excluded.site_url,
                deployment = excluded.deployment,
                enabled = excluded.enabled,
                last_synced_at = COALESCE(excluded.last_synced_at, sites.last_synced_at),
                last_error = CASE
                    WHEN excluded.last_synced_at IS NOT NULL OR excluded.last_error IS NOT NULL
                        THEN excluded.last_error
                    ELSE sites.last_error
                END
            ",
            params![
                site.site_id,
                site.name,
                site.site_url,
                site.deployment,
                if site.enabled { 1i32 } else { 0i32 },
                site.last_synced_at,
                site.last_error,
            ],
        )?;
        Ok(())
    }

    pub fn list_sites(&self) -> Result<Vec<SiteRecord>> {
        let conn = self.read_conn()?;
        let mut stmt = conn.prepare_cached(
            "
            SELECT site_id, name, site_url, deployment, enabled, last_synced_at, last_error
            FROM sites
            ORDER BY enabled DESC, name COLLATE NOCASE ASC, site_id ASC
            ",
        )?;
        let rows = stmt.query_map([], map_site_record_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_site(&self, site_id: &str) -> Result<Option<SiteRecord>> {
        let conn = self.read_conn()?;
        let mut stmt = conn.prepare_cached(
            "
            SELECT site_id, name, site_url, deployment, enabled, last_synced_at, last_error
            FROM sites
            WHERE site_id = ?1
            ",
        )?;
        let record = stmt.query_row([site_id], map_site_record_row).optional()?;
        Ok(record)
    }

    pub fn record_site_sync(
        &self,
        site_id: &str,
        synced_at: &str,
        err: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare_cached(
            "UPDATE sites SET last_synced_at = ?1, last_error = ?2 WHERE site_id = ?3",
        )?;
        stmt.execute(params![synced_at, err, site_id])?;
        Ok(())
    }

    pub fn delete_site_data(&self, site_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM issues_fts WHERE rowid IN (SELECT rowid FROM issues WHERE site_id = ?1)",
            [site_id],
        )?;
        tx.execute(
            "DELETE FROM pages_fts WHERE rowid IN (SELECT rowid FROM pages WHERE site_id = ?1)",
            [site_id],
        )?;
        tx.execute("DELETE FROM issues WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM board_sprints WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM sprints WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM boards WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM projects WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM saved_filters WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM pages WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM spaces WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM site_identities WHERE site_id = ?1", [site_id])?;
        tx.execute("DELETE FROM sites WHERE site_id = ?1", [site_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Remembers who is signed in to a site, for offline "assigned to me" filters.
    pub fn save_identity_for_site(&self, site_id: &str, user: &UserSummary) -> Result<()> {
        let user_json = serde_json::to_string(user)?;
        let conn = self.lock_conn()?;
        conn.execute(
            "
            INSERT INTO site_identities (site_id, user_json, updated_at)
            VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            ON CONFLICT(site_id) DO UPDATE SET
                user_json = excluded.user_json,
                updated_at = excluded.updated_at
            ",
            params![site_id, user_json],
        )?;
        Ok(())
    }

    /// The user [Self::save_identity_for_site] stored for a site, if any.
    pub fn identity_for_site(&self, site_id: &str) -> Result<Option<UserSummary>> {
        let conn = self.read_conn()?;
        let mut stmt =
            conn.prepare_cached("SELECT user_json FROM site_identities WHERE site_id = ?1")?;
        let user_json: Option<String> = stmt.query_row([site_id], |row| row.get(0)).optional()?;
        Ok(user_json.and_then(|raw| serde_json::from_str(&raw).ok()))
    }

    /// All cached per-site signed-in identities, keyed by `site_id`.
    pub fn list_identities(&self) -> Result<HashMap<String, UserSummary>> {
        let conn = self.read_conn()?;
        let mut stmt = conn.prepare_cached("SELECT site_id, user_json FROM site_identities")?;
        let mut out = HashMap::new();
        let rows = stmt.query_map([], |row| {
            let sid: String = row.get(0)?;
            let raw: String = row.get(1)?;
            Ok((sid, raw))
        })?;
        for row in rows {
            let (sid, raw) = row?;
            if let Ok(user) = serde_json::from_str::<UserSummary>(&raw) {
                out.insert(sid, user);
            }
        }
        Ok(out)
    }

    pub fn cache_counts(&self) -> Result<CacheCounts> {
        let conn = self.read_conn()?;
        let projects: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM projects")?
            .query_row([], |r| r.get(0))?;
        let boards: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM boards")?
            .query_row([], |r| r.get(0))?;
        let issues: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM issues")?
            .query_row([], |r| r.get(0))?;
        let spaces: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM spaces")?
            .query_row([], |r| r.get(0))?;
        let pages: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM pages")?
            .query_row([], |r| r.get(0))?;
        Ok(CacheCounts {
            projects: projects.max(0) as usize,
            boards: boards.max(0) as usize,
            issues: issues.max(0) as usize,
            spaces: spaces.max(0) as usize,
            pages: pages.max(0) as usize,
        })
    }

    pub fn cache_counts_for_site(&self, site_id: &str) -> Result<CacheCounts> {
        let conn = self.read_conn()?;
        let projects: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM projects WHERE site_id = ?1")?
            .query_row([site_id], |r| r.get(0))?;
        let boards: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM boards WHERE site_id = ?1")?
            .query_row([site_id], |r| r.get(0))?;
        let issues: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM issues WHERE site_id = ?1")?
            .query_row([site_id], |r| r.get(0))?;
        let spaces: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM spaces WHERE site_id = ?1")?
            .query_row([site_id], |r| r.get(0))?;
        let pages: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM pages WHERE site_id = ?1")?
            .query_row([site_id], |r| r.get(0))?;
        Ok(CacheCounts {
            projects: projects.max(0) as usize,
            boards: boards.max(0) as usize,
            issues: issues.max(0) as usize,
            spaces: spaces.max(0) as usize,
            pages: pages.max(0) as usize,
        })
    }

    pub fn unified_search_scoped(
        &self,
        query: &str,
        site_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, UnifiedSearchHit)>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits = self.search_issues_scoped(query, site_id, limit)?;
        hits.extend(self.search_pages_scoped(query, site_id, limit)?);
        hits.sort_by(|a, b| compare_unified_hits(&a.0, &a.1, &b.0, &b.1));
        hits.truncate(limit);
        Ok(hits)
    }

    pub fn search_unified(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        let scoped = self.unified_search_scoped(query, None, limit)?;
        Ok(scoped.into_iter().map(|(_, hit)| hit).collect())
    }
}

fn parse_unified_timestamp_millis(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .or_else(|_| chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .or_else(|_| chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%z"))
        .ok()
        .map(|dt| dt.timestamp_millis())
}

pub(crate) fn compare_unified_hits(
    site_a: &str,
    hit_a: &UnifiedSearchHit,
    site_b: &str,
    hit_b: &UnifiedSearchHit,
) -> std::cmp::Ordering {
    let time_ord = match (
        parse_unified_timestamp_millis(&hit_a.updated_at),
        parse_unified_timestamp_millis(&hit_b.updated_at),
    ) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => hit_b.updated_at.trim().cmp(hit_a.updated_at.trim()),
    };
    time_ord
        .then_with(|| site_a.cmp(site_b))
        .then_with(|| hit_a.id_or_key.cmp(&hit_b.id_or_key))
}

pub(crate) fn sort_federated_hits(hits: &mut [crate::models::FederatedSearchHit]) {
    hits.sort_by(|a, b| compare_unified_hits(&a.site_id, &a.hit, &b.site_id, &b.hit));
}

fn map_site_record_row(row: &Row<'_>) -> rusqlite::Result<SiteRecord> {
    Ok(SiteRecord {
        site_id: row.get(0)?,
        name: row.get(1)?,
        site_url: row.get(2)?,
        deployment: row.get(3)?,
        enabled: row.get::<_, i32>(4)? != 0,
        last_synced_at: row.get(5)?,
        last_error: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        BoardSummary, IssueDetail, IssueStatus, IssueSummary, IssueType, PageDetail, PageSummary,
        ProjectSummary, SearchHitKind, SpaceSummary, SprintSummary, StatusCategory,
    };

    fn make_project(key: &str, name: &str) -> ProjectSummary {
        ProjectSummary {
            id: format!("proj-{key}"),
            key: key.to_string(),
            name: name.to_string(),
            project_type: "software".into(),
            lead_name: None,
            avatar_url: None,
            pinned: false,
        }
    }

    fn make_board(id: i64, name: &str, project_key: &str) -> BoardSummary {
        BoardSummary {
            id,
            name: name.to_string(),
            board_type: "scrum".into(),
            project_key: Some(project_key.to_string()),
            project_name: Some("Engineering".into()),
            columns: vec![],
        }
    }

    fn make_sprint(id: i64, board_id: i64, name: &str) -> SprintSummary {
        SprintSummary {
            id,
            board_id,
            origin_board_id: None,
            name: name.to_string(),
            state: "active".into(),
            start_date: None,
            end_date: None,
            goal: None,
        }
    }

    fn make_issue(
        key: &str,
        project_key: &str,
        summary: &str,
        updated_at: &str,
        category: StatusCategory,
        site_host: &str,
    ) -> IssueSummary {
        IssueSummary {
            id: format!("id-{key}"),
            key: key.to_string(),
            project_key: project_key.to_string(),
            summary: summary.to_string(),
            status: IssueStatus {
                id: "1".into(),
                name: category.badge_label().into(),
                category,
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
            labels: vec!["federation".into()],
            story_points: Some(2.0),
            sprint_id: Some(10),
            sprint_name: Some("Sprint 10".into()),
            comment_count: 0,
            attachment_count: 0,
            created_at: "2026-11-01T00:00:00Z".into(),
            updated_at: updated_at.to_string(),
            web_url: format!("https://{site_host}/browse/{key}"),
        }
    }

    fn make_space(key: &str, name: &str, site_host: &str) -> SpaceSummary {
        SpaceSummary {
            id: format!("sp-{key}"),
            key: key.to_string(),
            name: name.to_string(),
            space_type: "global".into(),
            homepage_id: Some("100".into()),
            description: Some(format!("{name} docs")),
            web_url: format!("https://{site_host}/wiki/spaces/{key}"),
            pinned: false,
        }
    }

    fn make_page(id: &str, space_key: &str, title: &str, updated_at: &str, site_host: &str) -> PageSummary {
        PageSummary {
            id: id.to_string(),
            space_id: format!("sp-{space_key}"),
            space_key: space_key.to_string(),
            parent_id: None,
            title: title.to_string(),
            status: "current".into(),
            version_number: 1,
            author_name: Some("Author".into()),
            updated_at: updated_at.to_string(),
            position: 1,
            has_children: None,
            web_url: format!("https://{site_host}/wiki/spaces/{space_key}/pages/{id}"),
        }
    }

    #[test]
    fn test_site_registry_lifecycle_and_delete_site_data() {
        let storage = Storage::open_in_memory().unwrap();
        let acme = SiteRecord {
            site_id: "acme-cloud".into(),
            name: "Acme Cloud".into(),
            site_url: "https://acme.atlassian.net".into(),
            deployment: "cloud".into(),
            enabled: true,
            last_synced_at: None,
            last_error: None,
        };
        let corp = SiteRecord {
            site_id: "corp-dc".into(),
            name: "Corp Data Center".into(),
            site_url: "https://jira.corp.internal".into(),
            deployment: "datacenter".into(),
            enabled: true,
            last_synced_at: None,
            last_error: None,
        };
        storage.upsert_site(&acme).unwrap();
        storage.upsert_site(&corp).unwrap();

        storage
            .record_site_sync("acme-cloud", "2026-11-19T10:00:00Z", None)
            .unwrap();
        storage
            .record_site_sync("corp-dc", "2026-11-19T10:05:00Z", Some("HTTP 503"))
            .unwrap();

        let sites = storage.list_sites().unwrap();
        assert_eq!(sites.len(), 2);
        let acme_loaded = storage.get_site("acme-cloud").unwrap().unwrap();
        assert_eq!(
            acme_loaded.last_synced_at.as_deref(),
            Some("2026-11-19T10:00:00Z")
        );
        assert_eq!(acme_loaded.last_error, None);

        let corp_loaded = storage.get_site("corp-dc").unwrap().unwrap();
        assert_eq!(corp_loaded.last_error.as_deref(), Some("HTTP 503"));

        // Re-upserting metadata with None sync fields preserves sync state
        let mut corp_renamed = corp.clone();
        corp_renamed.name = "Corp On-Prem".into();
        storage.upsert_site(&corp_renamed).unwrap();
        let corp_after = storage.get_site("corp-dc").unwrap().unwrap();
        assert_eq!(corp_after.name, "Corp On-Prem");
        assert_eq!(corp_after.last_error.as_deref(), Some("HTTP 503"));

        // Populate both sites with colliding keys then delete acme-cloud
        storage
            .upsert_projects_for_site("acme-cloud", &[make_project("ENG", "Acme Eng")])
            .unwrap();
        storage
            .upsert_projects_for_site("corp-dc", &[make_project("ENG", "Corp Eng")])
            .unwrap();
        storage
            .upsert_issues_for_site(
                "acme-cloud",
                &[make_issue(
                    "ENG-1",
                    "ENG",
                    "Acme deployment task",
                    "2026-11-19T10:00:00Z",
                    StatusCategory::InProgress,
                    "acme.atlassian.net",
                )],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "corp-dc",
                &[make_issue(
                    "ENG-1",
                    "ENG",
                    "Corp deployment task",
                    "2026-11-19T11:00:00Z",
                    StatusCategory::InProgress,
                    "jira.corp.internal",
                )],
            )
            .unwrap();

        storage.delete_site_data("acme-cloud").unwrap();
        assert!(storage.get_site("acme-cloud").unwrap().is_none());
        assert_eq!(storage.cache_counts_for_site("acme-cloud").unwrap().issues, 0);
        assert_eq!(storage.cache_counts_for_site("corp-dc").unwrap().issues, 1);
        assert_eq!(storage.cache_counts_for_site("corp-dc").unwrap().projects, 1);

        let hits = storage.unified_search_scoped("deployment", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "corp-dc");
    }

    #[test]
    fn test_multi_site_collision_isolation_and_federated_search() {
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_site(&SiteRecord {
                site_id: "acme-cloud".into(),
                name: "Acme Cloud".into(),
                site_url: "https://acme.atlassian.net".into(),
                deployment: "cloud".into(),
                enabled: true,
                last_synced_at: None,
                last_error: None,
            })
            .unwrap();
        storage
            .upsert_site(&SiteRecord {
                site_id: "corp-dc".into(),
                name: "Corp DC".into(),
                site_url: "https://jira.corp.internal".into(),
                deployment: "datacenter".into(),
                enabled: true,
                last_synced_at: None,
                last_error: None,
            })
            .unwrap();

        // Identical project key ENG, board 1, sprint 10, issue ENG-1, space DOCS, page 100 in BOTH sites
        storage
            .upsert_projects_for_site("acme-cloud", &[make_project("ENG", "Acme Cloud Eng")])
            .unwrap();
        storage
            .upsert_projects_for_site("corp-dc", &[make_project("ENG", "Corp DC Eng")])
            .unwrap();

        storage
            .upsert_boards_for_site("acme-cloud", &[make_board(1, "Acme Board", "ENG")])
            .unwrap();
        storage
            .upsert_boards_for_site("corp-dc", &[make_board(1, "Corp Board", "ENG")])
            .unwrap();

        storage
            .upsert_sprints_for_site("acme-cloud", &[make_sprint(10, 1, "Acme Sprint 10")])
            .unwrap();
        storage
            .upsert_sprints_for_site("corp-dc", &[make_sprint(10, 1, "Corp Sprint 10")])
            .unwrap();

        let acme_issue = make_issue(
            "ENG-1",
            "ENG",
            "Acme OAuth federation gateway",
            "2026-11-19T09:00:00Z",
            StatusCategory::InProgress,
            "acme.atlassian.net",
        );
        let corp_issue = make_issue(
            "ENG-1",
            "ENG",
            "Corp PAT federation gateway",
            "2026-11-19T12:00:00Z",
            StatusCategory::Todo,
            "jira.corp.internal",
        );
        let corp_done_issue = make_issue(
            "ENG-2",
            "ENG",
            "Corp completed migration",
            "2026-11-19T13:00:00Z",
            StatusCategory::Done,
            "jira.corp.internal",
        );

        storage
            .upsert_issue_detail_for_site(
                "acme-cloud",
                &IssueDetail {
                    summary: acme_issue.clone(),
                    description_markdown: "Acme Cloud detailed description".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        storage
            .upsert_issue_detail_for_site(
                "corp-dc",
                &IssueDetail {
                    summary: corp_issue.clone(),
                    description_markdown: "Corp DC detailed description".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        storage
            .upsert_issues_for_site("corp-dc", &[corp_done_issue])
            .unwrap();

        storage
            .upsert_spaces_for_site("acme-cloud", &[make_space("DOCS", "Acme Docs", "acme.atlassian.net")])
            .unwrap();
        storage
            .upsert_spaces_for_site("corp-dc", &[make_space("DOCS", "Corp Docs", "jira.corp.internal")])
            .unwrap();

        let acme_page = make_page(
            "100",
            "DOCS",
            "Acme Federation Runbook",
            "2026-11-19T08:00:00Z",
            "acme.atlassian.net",
        );
        let corp_page = make_page(
            "100",
            "DOCS",
            "Corp Federation Runbook",
            "2026-11-19T11:00:00Z",
            "jira.corp.internal",
        );
        storage
            .upsert_page_detail_for_site(
                "acme-cloud",
                &PageDetail {
                    summary: acme_page.clone(),
                    body_markdown: "Acme Cloud gateway runbook steps".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        storage
            .upsert_page_detail_for_site(
                "corp-dc",
                &PageDetail {
                    summary: corp_page.clone(),
                    body_markdown: "Corp DC gateway runbook steps".into(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Verify zero overwrite across all entity types
        let all_projects = storage.list_projects_all_sites().unwrap();
        assert_eq!(all_projects.len(), 2);
        assert_eq!(
            storage.list_projects_for_site("acme-cloud").unwrap()[0].name,
            "Acme Cloud Eng"
        );
        assert_eq!(
            storage.list_projects_for_site("corp-dc").unwrap()[0].name,
            "Corp DC Eng"
        );

        let all_boards = storage.list_boards_all_sites(Some("ENG")).unwrap();
        assert_eq!(all_boards.len(), 2);
        assert_eq!(
            storage.list_boards_for_site("acme-cloud", None).unwrap()[0].name,
            "Acme Board"
        );
        assert_eq!(
            storage.list_boards_for_site("corp-dc", None).unwrap()[0].name,
            "Corp Board"
        );

        assert_eq!(
            storage.list_sprints_for_site("acme-cloud", 1).unwrap()[0].name,
            "Acme Sprint 10"
        );
        assert_eq!(
            storage.list_sprints_for_site("corp-dc", 1).unwrap()[0].name,
            "Corp Sprint 10"
        );

        let all_spaces = storage.list_spaces_all_sites().unwrap();
        assert_eq!(all_spaces.len(), 2);
        assert_eq!(
            storage.list_spaces_for_site("acme-cloud").unwrap()[0].name,
            "Acme Docs"
        );
        assert_eq!(
            storage.list_spaces_for_site("corp-dc").unwrap()[0].name,
            "Corp Docs"
        );

        // Verify independent detail preservation
        let acme_issue_detail = storage
            .get_issue_detail_for_site("acme-cloud", "ENG-1")
            .unwrap()
            .unwrap();
        let corp_issue_detail = storage
            .get_issue_detail_for_site("corp-dc", "ENG-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            acme_issue_detail.description_markdown,
            "Acme Cloud detailed description"
        );
        assert_eq!(
            corp_issue_detail.description_markdown,
            "Corp DC detailed description"
        );

        let acme_page_detail = storage
            .get_page_detail_for_site("acme-cloud", "100")
            .unwrap()
            .unwrap();
        let corp_page_detail = storage
            .get_page_detail_for_site("corp-dc", "100")
            .unwrap()
            .unwrap();
        assert_eq!(
            acme_page_detail.body_markdown,
            "Acme Cloud gateway runbook steps"
        );
        assert_eq!(
            corp_page_detail.body_markdown,
            "Corp DC gateway runbook steps"
        );

        // Verify site_id = Some(...) scoping on unified_search_scoped
        let acme_only = storage
            .unified_search_scoped("gateway", Some("acme-cloud"), 10)
            .unwrap();
        assert_eq!(acme_only.len(), 2);
        assert!(acme_only.iter().all(|(sid, _)| sid == "acme-cloud"));

        let corp_only = storage
            .unified_search_scoped("gateway", Some("corp-dc"), 10)
            .unwrap();
        assert_eq!(corp_only.len(), 2);
        assert!(corp_only.iter().all(|(sid, _)| sid == "corp-dc"));

        // Verify federated unified_search_scoped returns ALL 4 hits (both issues + both pages) without key collision!
        let federated = storage.unified_search_scoped("gateway", None, 10).unwrap();
        assert_eq!(federated.len(), 4);
        assert_eq!(
            federated
                .iter()
                .filter(|(_, h)| h.kind == SearchHitKind::JiraIssue && h.id_or_key == "ENG-1")
                .count(),
            2
        );
        assert_eq!(
            federated
                .iter()
                .filter(|(_, h)| h.kind == SearchHitKind::ConfluencePage && h.id_or_key == "100")
                .count(),
            2
        );

        // Verify exact colliding key search returns BOTH sites
        let by_key = storage.unified_search_scoped("ENG-1", None, 10).unwrap();
        assert_eq!(by_key.len(), 2);

        // Verify federated My Work excludes Done issues and includes both enabled sites
        let my_work = storage.list_my_work_all_sites(10).unwrap();
        assert_eq!(my_work.len(), 2);
        assert_eq!(my_work[0].0, "corp-dc");
        assert_eq!(my_work[0].1.key, "ENG-1");
        assert_eq!(my_work[1].0, "acme-cloud");
        assert_eq!(my_work[1].1.key, "ENG-1");

        // Disabling corp-dc excludes it from federated My Work without deleting its cache
        storage
            .upsert_site(&SiteRecord {
                site_id: "corp-dc".into(),
                name: "Corp DC".into(),
                site_url: "https://jira.corp.internal".into(),
                deployment: "datacenter".into(),
                enabled: false,
                last_synced_at: None,
                last_error: None,
            })
            .unwrap();
        let my_work_after_disable = storage.list_my_work_all_sites(10).unwrap();
        assert_eq!(my_work_after_disable.len(), 1);
        assert_eq!(my_work_after_disable[0].0, "acme-cloud");

        // Counts per site vs total
        let acme_counts = storage.cache_counts_for_site("acme-cloud").unwrap();
        assert_eq!(acme_counts.issues, 1);
        assert_eq!(acme_counts.pages, 1);
        let corp_counts = storage.cache_counts_for_site("corp-dc").unwrap();
        assert_eq!(corp_counts.issues, 2);
        assert_eq!(corp_counts.pages, 1);
        assert_eq!(storage.cache_counts().unwrap().issues, 3);
    }

    #[test]
    fn test_korean_2char_search_across_multiple_sites() {
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_issues_for_site(
                "acme-cloud",
                &[make_issue(
                    "ENG-1",
                    "ENG",
                    "클라우드 배포 파이프라인 점검",
                    "2026-11-19T09:00:00Z",
                    StatusCategory::InProgress,
                    "acme.atlassian.net",
                )],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "corp-dc",
                &[make_issue(
                    "ENG-1",
                    "ENG",
                    "사내 배포 승인 자동화",
                    "2026-11-19T10:00:00Z",
                    StatusCategory::Todo,
                    "jira.corp.internal",
                )],
            )
            .unwrap();

        storage
            .upsert_pages_for_site(
                "acme-cloud",
                &[make_page(
                    "100",
                    "DOCS",
                    "배포 체크리스트 (Cloud)",
                    "2026-11-19T08:00:00Z",
                    "acme.atlassian.net",
                )],
            )
            .unwrap();
        storage
            .upsert_pages_for_site(
                "corp-dc",
                &[make_page(
                    "100",
                    "DOCS",
                    "배포 체크리스트 (DC)",
                    "2026-11-19T11:00:00Z",
                    "jira.corp.internal",
                )],
            )
            .unwrap();

        // 2-char Korean query "배포" must match all 4 items across both sites via LIKE fallback
        let all_hits = storage.unified_search_scoped("배포", None, 20).unwrap();
        assert_eq!(all_hits.len(), 4);
        assert_eq!(
            all_hits.iter().filter(|(sid, _)| sid == "acme-cloud").count(),
            2
        );
        assert_eq!(
            all_hits.iter().filter(|(sid, _)| sid == "corp-dc").count(),
            2
        );

        // Site-scoped 2-char Korean search
        let acme_hits = storage
            .unified_search_scoped("배포", Some("acme-cloud"), 20)
            .unwrap();
        assert_eq!(acme_hits.len(), 2);
        assert!(acme_hits.iter().all(|(sid, _)| sid == "acme-cloud"));

        let corp_hits = storage
            .unified_search_scoped("승인", Some("corp-dc"), 20)
            .unwrap();
        assert_eq!(corp_hits.len(), 1);
        assert_eq!(corp_hits[0].0, "corp-dc");
        assert_eq!(corp_hits[0].1.id_or_key, "ENG-1");
    }

    #[test]
    fn delete_site_data_clears_board_sprints() {
        let storage = Storage::open_in_memory().unwrap();
        let shared = SprintSummary {
            origin_board_id: Some(1),
            ..make_sprint(10, 2, "Shared Sprint 10")
        };
        for site in ["acme-cloud", "corp-dc"] {
            storage
                .upsert_sprints_for_site(site, &[make_sprint(10, 1, "Sprint 10"), shared.clone()])
                .unwrap();
        }
        let links = |site: &str| -> i64 {
            let conn = storage.lock_conn().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM board_sprints WHERE site_id = ?1",
                [site],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(links("acme-cloud"), 2);

        storage.delete_site_data("acme-cloud").unwrap();

        assert_eq!(links("acme-cloud"), 0);
        assert!(storage.list_sprints_for_site("acme-cloud", 1).unwrap().is_empty());
        assert!(storage.list_sprints_for_site("acme-cloud", 2).unwrap().is_empty());
        // The other site keeps its sprint and both board links.
        assert_eq!(links("corp-dc"), 2);
        assert_eq!(storage.list_sprints_for_site("corp-dc", 2).unwrap().len(), 1);
    }

    #[test]
    fn site_identity_round_trips_and_leaves_with_its_site() {
        let storage = Storage::open_in_memory().unwrap();
        let me = |id: &str| UserSummary {
            id: id.into(),
            display_name: format!("User {id}"),
            ..Default::default()
        };
        assert_eq!(storage.identity_for_site("acme").unwrap(), None);
        storage.save_identity_for_site("acme", &me("acct-1")).unwrap();
        storage.save_identity_for_site("acme", &me("acct-2")).unwrap();
        storage.save_identity_for_site("corp", &me("jdoe")).unwrap();
        assert_eq!(storage.identity_for_site("acme").unwrap(), Some(me("acct-2")));

        storage.delete_site_data("acme").unwrap();
        assert_eq!(storage.identity_for_site("acme").unwrap(), None);
        assert_eq!(storage.identity_for_site("corp").unwrap(), Some(me("jdoe")));
    }
}
