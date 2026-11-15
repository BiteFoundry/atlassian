//! Unified SQLite WAL + FTS5 local cache (`cache.db`) for Jira projects, boards, sprints, issues, and Confluence spaces & pages.

pub mod confluence;
pub mod db;
pub mod jira;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::models::UnifiedSearchHit;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCounts {
    pub projects: usize,
    pub boards: usize,
    pub issues: usize,
    pub spaces: usize,
    pub pages: usize,
}

#[derive(Clone)]
pub struct Storage {
    pub(crate) conn: Arc<Mutex<Connection>>,
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

    pub fn cache_counts(&self) -> Result<CacheCounts> {
        let conn = self.lock_conn()?;
        let projects: i64 = conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
        let boards: i64 = conn.query_row("SELECT COUNT(*) FROM boards", [], |r| r.get(0))?;
        let issues: i64 = conn.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get(0))?;
        let spaces: i64 = conn.query_row("SELECT COUNT(*) FROM spaces", [], |r| r.get(0))?;
        let pages: i64 = conn.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))?;
        Ok(CacheCounts {
            projects: projects.max(0) as usize,
            boards: boards.max(0) as usize,
            issues: issues.max(0) as usize,
            spaces: spaces.max(0) as usize,
            pages: pages.max(0) as usize,
        })
    }

    pub fn search_unified(&self, query: &str, limit: usize) -> Result<Vec<UnifiedSearchHit>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits = self.search_issues(query, limit)?;
        hits.extend(self.search_pages(query, limit)?);
        hits.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        hits.truncate(limit);
        Ok(hits)
    }
}
