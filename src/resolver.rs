//! Human-friendly fuzzy entity resolver for CLI, HTTP API, and MCP.
//!
//! Resolves Jira projects, Jira Agile boards, Confluence spaces, and Atlassian users
//! from the local SQLite `Storage` cache with optional live `AtlassianClient` API fallback.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    atlassian::{
        client::{CodedError, HttpStatusError},
        AtlassianClient,
    },
    models::{BoardSummary, ProjectSummary, SpaceSummary, UserSummary},
    storage::Storage,
};

const MAX_CANDIDATE_LIST: usize = 8;

/// An entity candidate tagged with the Atlassian site it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteCandidate<T> {
    pub site_id: String,
    pub site_name: String,
    pub item: T,
}

fn filter_candidates_by_site<'a, T>(
    candidates: &'a [SiteCandidate<T>],
    site_filter: Option<&str>,
) -> Result<Vec<&'a SiteCandidate<T>>> {
    let Some(raw_filter) = site_filter.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(candidates.iter().collect());
    };

    let by_id: Vec<&SiteCandidate<T>> = candidates
        .iter()
        .filter(|c| c.site_id.trim().eq_ignore_ascii_case(raw_filter))
        .collect();
    if !by_id.is_empty() {
        return Ok(by_id);
    }

    let by_name: Vec<&SiteCandidate<T>> = candidates
        .iter()
        .filter(|c| c.site_name.trim().eq_ignore_ascii_case(raw_filter))
        .collect();
    if !by_name.is_empty() {
        return Ok(by_name);
    }

    let mut available_sites: Vec<&str> = Vec::new();
    let mut seen = HashSet::new();
    for c in candidates {
        let key = c.site_id.trim().to_ascii_lowercase();
        if !key.is_empty() && seen.insert(key) {
            available_sites.push(c.site_id.trim());
        }
    }
    if available_sites.is_empty() {
        bail!("Site '{raw_filter}' not found (no sites available).");
    }
    bail!(
        "Site '{raw_filter}' not found. Available sites: {}.",
        available_sites.join(", ")
    );
}

fn count_distinct_sites<T>(matches: &[&SiteCandidate<T>]) -> usize {
    let mut sites = HashSet::new();
    for m in matches {
        sites.insert(m.site_id.trim().to_ascii_lowercase());
    }
    sites.len()
}

fn format_multi_site_matches<T, F>(matches: &[&SiteCandidate<T>], label_fn: F) -> String
where
    F: Fn(&SiteCandidate<T>) -> String,
{
    let mut parts: Vec<String> = matches
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|m| label_fn(m))
        .collect();
    if matches.len() > MAX_CANDIDATE_LIST {
        parts.push(format!("... and {} more", matches.len() - MAX_CANDIDATE_LIST));
    }
    parts.join(", ")
}

/// Resolves a Jira project query across one or more Atlassian sites.
///
/// When `site_filter` is `Some`, resolution is scoped to that site (`site_id` or `site_name`).
/// When `site_filter` is `None` and the same key/query matches multiple sites, returns an
/// actionable error listing each matching site and instructing the caller to pass `--site <id>`.
pub fn resolve_project_across_sites(
    query: &str,
    site_filter: Option<&str>,
    candidates: &[SiteCandidate<ProjectSummary>],
) -> Result<SiteCandidate<ProjectSummary>> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira project key or name is empty.");
    }

    let scoped = filter_candidates_by_site(candidates, site_filter)?;
    let q_lower = q.to_lowercase();

    let tiers: [Vec<&SiteCandidate<ProjectSummary>>; 5] = [
        // Priority 1: Exact case-insensitive key
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.key.eq_ignore_ascii_case(q))
            .collect(),
        // Priority 2: Exact id
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.id == q)
            .collect(),
        // Priority 3: Exact case-insensitive name
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.name.trim().to_lowercase() == q_lower)
            .collect(),
        // Priority 4: Prefix on key or name
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item.key.to_lowercase().starts_with(&q_lower)
                    || c.item.name.trim().to_lowercase().starts_with(&q_lower)
            })
            .collect(),
        // Priority 5: Substring on key or name
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item.key.to_lowercase().contains(&q_lower)
                    || c.item.name.to_lowercase().contains(&q_lower)
            })
            .collect(),
    ];

    for matches in tiers {
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if matches.len() > 1 {
            if count_distinct_sites(&matches) > 1 {
                let list = format_multi_site_matches(&matches, |c| {
                    format!("{} ({})", c.item.key, c.site_id)
                });
                bail!(
                    "Ambiguous project '{q}' matched multiple sites: {list}. Pass --site <id> to disambiguate."
                );
            }
            let items: Vec<&ProjectSummary> = matches.iter().map(|c| &c.item).collect();
            bail!(
                "Ambiguous Jira project '{q}' matches {} projects in site '{}'. Specify an exact project key:\n{}",
                items.len(),
                matches[0].site_id,
                format_project_candidates(&items)
            );
        }
    }

    if let Some(filter) = site_filter.map(str::trim).filter(|s| !s.is_empty()) {
        bail!("Jira project not found: '{q}' in site '{filter}'. Check the project key or run sync first.");
    }
    bail!("Jira project not found: '{q}'. Check the project key or run sync first.")
}

/// Resolves a Confluence space query across one or more Atlassian sites.
///
/// When `site_filter` is `Some`, resolution is scoped to that site (`site_id` or `site_name`).
/// When `site_filter` is `None` and the same key/query matches multiple sites, returns an
/// actionable error listing each matching site and instructing the caller to pass `--site <id>`.
pub fn resolve_space_across_sites(
    query: &str,
    site_filter: Option<&str>,
    candidates: &[SiteCandidate<SpaceSummary>],
) -> Result<SiteCandidate<SpaceSummary>> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Confluence space key or name is empty.");
    }

    let scoped = filter_candidates_by_site(candidates, site_filter)?;
    let q_lower = q.to_lowercase();

    let tiers: [Vec<&SiteCandidate<SpaceSummary>>; 5] = [
        // Priority 1: Exact case-insensitive key
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.key.eq_ignore_ascii_case(q))
            .collect(),
        // Priority 2: Exact id
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.id == q)
            .collect(),
        // Priority 3: Exact case-insensitive name
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.name.trim().to_lowercase() == q_lower)
            .collect(),
        // Priority 4: Prefix on key or name
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item.key.to_lowercase().starts_with(&q_lower)
                    || c.item.name.trim().to_lowercase().starts_with(&q_lower)
            })
            .collect(),
        // Priority 5: Substring on key or name
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item.key.to_lowercase().contains(&q_lower)
                    || c.item.name.to_lowercase().contains(&q_lower)
            })
            .collect(),
    ];

    for matches in tiers {
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if matches.len() > 1 {
            if count_distinct_sites(&matches) > 1 {
                let list = format_multi_site_matches(&matches, |c| {
                    format!("{} ({})", c.item.key, c.site_id)
                });
                bail!(
                    "Ambiguous space '{q}' matched multiple sites: {list}. Pass --site <id> to disambiguate."
                );
            }
            let items: Vec<&SpaceSummary> = matches.iter().map(|c| &c.item).collect();
            bail!(
                "Ambiguous Confluence space '{q}' matches {} spaces in site '{}'. Specify an exact space key:\n{}",
                items.len(),
                matches[0].site_id,
                format_space_candidates(&items)
            );
        }
    }

    if let Some(filter) = site_filter.map(str::trim).filter(|s| !s.is_empty()) {
        bail!("Confluence space not found: '{q}' in site '{filter}'. Check the space key or run sync first.");
    }
    bail!("Confluence space not found: '{q}'. Check the space key or run sync first.")
}

/// Resolves a Jira Agile board query across one or more Atlassian sites.
///
/// When `site_filter` is `Some`, resolution is scoped to that site (`site_id` or `site_name`).
/// When `site_filter` is `None` and the same ID/name/project matches multiple sites, returns an
/// actionable error listing each matching site and instructing the caller to pass `--site <id>`.
pub fn resolve_board_across_sites(
    query: &str,
    site_filter: Option<&str>,
    candidates: &[SiteCandidate<BoardSummary>],
) -> Result<SiteCandidate<BoardSummary>> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira board ID or name is empty.");
    }

    let scoped = filter_candidates_by_site(candidates, site_filter)?;
    let id_candidate = q.strip_prefix('#').unwrap_or(q).trim();
    let parsed_id = id_candidate.parse::<i64>().ok();
    let q_lower = q.to_lowercase();

    let tiers: [Vec<&SiteCandidate<BoardSummary>>; 5] = [
        // Priority 1: Exact numeric ID (with optional leading '#')
        match parsed_id {
            Some(board_id) => scoped
                .iter()
                .copied()
                .filter(|c| c.item.id == board_id)
                .collect(),
            None => Vec::new(),
        },
        // Priority 2: Exact case-insensitive name
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.name.trim().to_lowercase() == q_lower)
            .collect(),
        // Priority 3: Exact case-insensitive project_key
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item
                    .project_key
                    .as_deref()
                    .is_some_and(|pk| pk.eq_ignore_ascii_case(q))
            })
            .collect(),
        // Priority 4: Prefix on name
        scoped
            .iter()
            .copied()
            .filter(|c| c.item.name.trim().to_lowercase().starts_with(&q_lower))
            .collect(),
        // Priority 5: Substring on name, project_key, or project_name
        scoped
            .iter()
            .copied()
            .filter(|c| {
                c.item.name.to_lowercase().contains(&q_lower)
                    || c.item
                        .project_key
                        .as_deref()
                        .is_some_and(|pk| pk.to_lowercase().contains(&q_lower))
                    || c.item
                        .project_name
                        .as_deref()
                        .is_some_and(|pn| pn.to_lowercase().contains(&q_lower))
            })
            .collect(),
    ];

    for matches in tiers {
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if matches.len() > 1 {
            if count_distinct_sites(&matches) > 1 {
                let list = format_multi_site_matches(&matches, |c| {
                    format!("#{} {} ({})", c.item.id, c.item.name, c.site_id)
                });
                bail!(
                    "Ambiguous board '{q}' matched multiple sites: {list}. Pass --site <id> to disambiguate."
                );
            }
            let items: Vec<&BoardSummary> = matches.iter().map(|c| &c.item).collect();
            bail!(
                "Ambiguous Jira board '{q}' matches {} boards in site '{}'. Specify a board ID:\n{}",
                items.len(),
                matches[0].site_id,
                format_board_candidates(&items)
            );
        }
    }

    if let Some(filter) = site_filter.map(str::trim).filter(|s| !s.is_empty()) {
        bail!("Jira board not found: '{q}' in site '{filter}'. Check the board ID or run sync first.");
    }
    bail!("Jira board not found: '{q}'. Check the board ID or run sync first.")
}

/// Resolves a Jira project query against an in-memory slice of `ProjectSummary` records.
///
/// Priority:
/// 1. Exact case-insensitive `key` match (e.g. `ENG`, `eng`)
/// 2. Exact `id` match
/// 3. Unique case-insensitive exact `name` match
/// 4. Unique case-insensitive `key` or `name` prefix match
/// 5. Unique case-insensitive `key` or `name` substring match
pub fn resolve_project_from_slice(
    projects: &[ProjectSummary],
    query: &str,
) -> Result<ProjectSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira project key or name is empty.");
    }

    // Priority 1: Exact case-insensitive key
    if let Some(exact_key) = projects.iter().find(|p| p.key.eq_ignore_ascii_case(q)) {
        return Ok(exact_key.clone());
    }

    // Priority 2: Exact id
    if let Some(exact_id) = projects.iter().find(|p| p.id == q) {
        return Ok(exact_id.clone());
    }

    let q_lower = q.to_lowercase();

    // Priority 3: Unique case-insensitive exact name
    let exact_name: Vec<&ProjectSummary> = projects
        .iter()
        .filter(|p| p.name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous Jira project '{q}' matches {} projects exactly. Specify an exact project key:\n{}",
            exact_name.len(),
            format_project_candidates(&exact_name)
        );
    }

    // Priority 4: Unique case-insensitive prefix on key or name
    let prefix_matches: Vec<&ProjectSummary> = projects
        .iter()
        .filter(|p| {
            p.key.to_lowercase().starts_with(&q_lower)
                || p.name.trim().to_lowercase().starts_with(&q_lower)
        })
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous Jira project prefix '{q}' matches {} projects. Specify an exact project key:\n{}",
            prefix_matches.len(),
            format_project_candidates(&prefix_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring on key or name
    let sub_matches: Vec<&ProjectSummary> = projects
        .iter()
        .filter(|p| {
            p.key.to_lowercase().contains(&q_lower) || p.name.to_lowercase().contains(&q_lower)
        })
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous Jira project '{q}' matches {n} projects. Specify an exact project key:\n{}",
            format_project_candidates(&sub_matches)
        ),
        _ => bail!("Jira project not found: '{q}'. Check the project key or run sync first."),
    }
}

/// Resolves a Jira project purely from the local SQLite cache.
pub fn resolve_project_cached(storage: &Storage, query: &str) -> Result<ProjectSummary> {
    let cached = storage.list_projects()?;
    resolve_project_from_slice(&cached, query)
}

/// Resolves a Jira project from local SQLite cache first, falling back to the live API when
/// `client` is provided and no cached match is found.
pub async fn resolve_project(
    storage: &Storage,
    client: Option<&AtlassianClient>,
    query: &str,
) -> Result<ProjectSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira project key or name is empty.");
    }

    let cached = storage.list_projects()?;
    if !cached.is_empty() {
        match resolve_project_from_slice(&cached, q) {
            Ok(found) => return Ok(found),
            Err(err) if client.is_none() || err.to_string().starts_with("Ambiguous") => {
                return Err(err);
            }
            Err(_) => {}
        }
    }

    let Some(api) = client else {
        return resolve_project_from_slice(&cached, q);
    };

    let fetched = api.list_projects().await?;
    storage.upsert_projects(&fetched)?;
    let refreshed = storage.list_projects()?;
    resolve_project_from_slice(&refreshed, q)
}

/// Resolves a Jira Agile board query against an in-memory slice of `BoardSummary` records.
///
/// Priority:
/// 1. Exact numeric `id` match (accepts optional leading `#`, e.g. `"42"` or `"#42"`)
/// 2. Unique case-insensitive exact `name` match
/// 3. Unique case-insensitive exact `project_key` match
/// 4. Unique case-insensitive prefix match on `name`
/// 5. Unique case-insensitive substring match on `name`, `project_key`, or `project_name`
pub fn resolve_board_from_slice(boards: &[BoardSummary], query: &str) -> Result<BoardSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira board ID or name is empty.");
    }

    // Priority 1: Exact numeric ID (with optional leading '#')
    let id_candidate = q.strip_prefix('#').unwrap_or(q).trim();
    if let Ok(board_id) = id_candidate.parse::<i64>() {
        if let Some(exact_id) = boards.iter().find(|b| b.id == board_id) {
            return Ok(exact_id.clone());
        }
    }

    let q_lower = q.to_lowercase();

    // Priority 2: Unique case-insensitive exact name
    let exact_name: Vec<&BoardSummary> = boards
        .iter()
        .filter(|b| b.name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous Jira board '{q}' matches {} boards exactly. Specify a board ID:\n{}",
            exact_name.len(),
            format_board_candidates(&exact_name)
        );
    }

    // Priority 3: Unique case-insensitive exact project_key
    let exact_project: Vec<&BoardSummary> = boards
        .iter()
        .filter(|b| {
            b.project_key
                .as_deref()
                .is_some_and(|pk| pk.eq_ignore_ascii_case(q))
        })
        .collect();
    if exact_project.len() == 1 {
        return Ok(exact_project[0].clone());
    } else if exact_project.len() > 1 {
        bail!(
            "Ambiguous Jira board '{q}' matches {} boards in project '{q}'. Specify a board ID:\n{}",
            exact_project.len(),
            format_board_candidates(&exact_project)
        );
    }

    // Priority 4: Unique case-insensitive name prefix
    let prefix_matches: Vec<&BoardSummary> = boards
        .iter()
        .filter(|b| b.name.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous Jira board prefix '{q}' matches {} boards. Specify a board ID:\n{}",
            prefix_matches.len(),
            format_board_candidates(&prefix_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring on name, project_key, or project_name
    let sub_matches: Vec<&BoardSummary> = boards
        .iter()
        .filter(|b| {
            b.name.to_lowercase().contains(&q_lower)
                || b.project_key
                    .as_deref()
                    .is_some_and(|pk| pk.to_lowercase().contains(&q_lower))
                || b.project_name
                    .as_deref()
                    .is_some_and(|pn| pn.to_lowercase().contains(&q_lower))
        })
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous Jira board '{q}' matches {n} boards. Specify a board ID:\n{}",
            format_board_candidates(&sub_matches)
        ),
        _ => bail!("Jira board not found: '{q}'. Check the board ID or run sync first."),
    }
}

/// Resolves a Jira board purely from the local SQLite cache.
pub fn resolve_board_cached(storage: &Storage, query: &str) -> Result<BoardSummary> {
    let cached = storage.list_boards(None)?;
    resolve_board_from_slice(&cached, query)
}

/// Resolves a Jira board from local SQLite cache first, falling back to the live API when
/// `client` is provided and no cached match is found.
pub async fn resolve_board(
    storage: &Storage,
    client: Option<&AtlassianClient>,
    query: &str,
) -> Result<BoardSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Jira board ID or name is empty.");
    }

    let cached = storage.list_boards(None)?;
    if !cached.is_empty() {
        match resolve_board_from_slice(&cached, q) {
            Ok(found) => return Ok(found),
            Err(err) if client.is_none() || err.to_string().starts_with("Ambiguous") => {
                return Err(err);
            }
            Err(_) => {}
        }
    }

    let Some(api) = client else {
        return resolve_board_from_slice(&cached, q);
    };

    let fetched = api.list_boards(None).await?;
    storage.upsert_boards(&fetched)?;
    let refreshed = storage.list_boards(None)?;
    resolve_board_from_slice(&refreshed, q)
}

/// Resolves a Confluence space query against an in-memory slice of `SpaceSummary` records.
///
/// Priority:
/// 1. Exact case-insensitive `key` match (e.g. `ENG`, `~personal`)
/// 2. Exact `id` match
/// 3. Unique case-insensitive exact `name` match
/// 4. Unique case-insensitive `key` or `name` prefix match
/// 5. Unique case-insensitive `key` or `name` substring match
pub fn resolve_space_from_slice(spaces: &[SpaceSummary], query: &str) -> Result<SpaceSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Confluence space key or name is empty.");
    }

    // Priority 1: Exact case-insensitive key
    if let Some(exact_key) = spaces.iter().find(|s| s.key.eq_ignore_ascii_case(q)) {
        return Ok(exact_key.clone());
    }

    // Priority 2: Exact id
    if let Some(exact_id) = spaces.iter().find(|s| s.id == q) {
        return Ok(exact_id.clone());
    }

    let q_lower = q.to_lowercase();

    // Priority 3: Unique case-insensitive exact name
    let exact_name: Vec<&SpaceSummary> = spaces
        .iter()
        .filter(|s| s.name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous Confluence space '{q}' matches {} spaces exactly. Specify an exact space key:\n{}",
            exact_name.len(),
            format_space_candidates(&exact_name)
        );
    }

    // Priority 4: Unique case-insensitive prefix on key or name
    let prefix_matches: Vec<&SpaceSummary> = spaces
        .iter()
        .filter(|s| {
            s.key.to_lowercase().starts_with(&q_lower)
                || s.name.trim().to_lowercase().starts_with(&q_lower)
        })
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous Confluence space prefix '{q}' matches {} spaces. Specify an exact space key:\n{}",
            prefix_matches.len(),
            format_space_candidates(&prefix_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring on key or name
    let sub_matches: Vec<&SpaceSummary> = spaces
        .iter()
        .filter(|s| {
            s.key.to_lowercase().contains(&q_lower) || s.name.to_lowercase().contains(&q_lower)
        })
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous Confluence space '{q}' matches {n} spaces. Specify an exact space key:\n{}",
            format_space_candidates(&sub_matches)
        ),
        _ => bail!("Confluence space not found: '{q}'. Check the space key or run sync first."),
    }
}

/// Resolves a Confluence space purely from the local SQLite cache.
pub fn resolve_space_cached(storage: &Storage, query: &str) -> Result<SpaceSummary> {
    let cached = storage.list_spaces()?;
    resolve_space_from_slice(&cached, query)
}

/// Resolves a Confluence space from local SQLite cache first, falling back to the live API when
/// `client` is provided and no cached match is found.
pub async fn resolve_space(
    storage: &Storage,
    client: Option<&AtlassianClient>,
    query: &str,
) -> Result<SpaceSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Confluence space key or name is empty.");
    }

    let cached = storage.list_spaces()?;
    if !cached.is_empty() {
        match resolve_space_from_slice(&cached, q) {
            Ok(found) => return Ok(found),
            Err(err) if client.is_none() || err.to_string().starts_with("Ambiguous") => {
                return Err(err);
            }
            Err(_) => {}
        }
    }

    let Some(api) = client else {
        return resolve_space_from_slice(&cached, q);
    };

    let fetched = api.list_spaces(250).await?;
    storage.upsert_spaces(&fetched)?;
    let refreshed = storage.list_spaces()?;
    resolve_space_from_slice(&refreshed, q)
}

/// Collects deduplicated users observed across cached Jira issues (assignees and reporters).
pub fn collect_cached_users(storage: &Storage) -> Result<Vec<UserSummary>> {
    let issues = storage.list_issues(None, 500)?;
    let mut by_id: HashMap<String, UserSummary> = HashMap::new();

    for issue in issues {
        for maybe_user in [issue.assignee, issue.reporter] {
            if let Some(user) = maybe_user {
                let id = user.id.trim();
                if id.is_empty() {
                    continue;
                }
                by_id
                    .entry(id.to_string())
                    .and_modify(|existing| {
                        if existing.display_name.trim().is_empty()
                            && !user.display_name.trim().is_empty()
                        {
                            existing.display_name = user.display_name.clone();
                        }
                        if existing.email.is_none() && user.email.is_some() {
                            existing.email = user.email.clone();
                        }
                        if existing.avatar_url.is_none() && user.avatar_url.is_some() {
                            existing.avatar_url = user.avatar_url.clone();
                        }
                    })
                    .or_insert(user);
            }
        }
    }

    let mut users: Vec<UserSummary> = by_id.into_values().collect();
    users.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(users)
}

/// Resolves an Atlassian user query against an in-memory slice of `UserSummary` records.
///
/// Priority:
/// 1. Exact `id` match (Cloud `accountId` or Data Center `name`/`key`, exact then case-insensitive)
/// 2. Exact case-insensitive `email` match
/// 3. Unique case-insensitive exact `display_name` match
/// 4. Unique case-insensitive prefix match on `display_name` or `email`
/// 5. Unique case-insensitive substring match on `display_name`, `email`, or `id`
pub fn resolve_user_from_slice(users: &[UserSummary], query: &str) -> Result<UserSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("User accountId, email, or display name is empty.");
    }

    // Priority 1: Exact accountId / username
    if let Some(exact_id) = users.iter().find(|u| u.id == q) {
        return Ok(exact_id.clone());
    }
    if let Some(ci_id) = users.iter().find(|u| u.id.eq_ignore_ascii_case(q)) {
        return Ok(ci_id.clone());
    }

    let q_lower = q.to_lowercase();

    // Priority 2: Exact case-insensitive email
    let exact_email: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.email
                .as_deref()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(q))
        })
        .collect();
    if exact_email.len() == 1 {
        return Ok(exact_email[0].clone());
    } else if exact_email.len() > 1 {
        bail!(
            "Ambiguous user email '{q}' matches {} users. Specify an exact accountId:\n{}",
            exact_email.len(),
            format_user_candidates(&exact_email)
        );
    }

    // Priority 3: Unique case-insensitive exact display_name
    let exact_name: Vec<&UserSummary> = users
        .iter()
        .filter(|u| u.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous user '{q}' matches {} users with the same display name. Specify an email or accountId:\n{}",
            exact_name.len(),
            format_user_candidates(&exact_name)
        );
    }

    // Priority 4: Unique case-insensitive prefix on display_name or email
    let prefix_matches: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.display_name.trim().to_lowercase().starts_with(&q_lower)
                || u.email
                    .as_deref()
                    .is_some_and(|e| e.trim().to_lowercase().starts_with(&q_lower))
        })
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous user prefix '{q}' matches {} users. Specify an email or accountId:\n{}",
            prefix_matches.len(),
            format_user_candidates(&prefix_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring on display_name, email, or id
    let sub_matches: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.display_name.to_lowercase().contains(&q_lower)
                || u.email
                    .as_deref()
                    .is_some_and(|e| e.to_lowercase().contains(&q_lower))
                || u.id.to_lowercase().contains(&q_lower)
        })
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous user '{q}' matches {n} users. Specify an email or accountId:\n{}",
            format_user_candidates(&sub_matches)
        ),
        _ => bail!(
            "User not found: '{q}'. Provide an exact accountId, email address, or full display name."
        ),
    }
}

/// Resolves a user purely from cached SQLite issue assignees/reporters.
pub fn resolve_user_cached(storage: &Storage, query: &str) -> Result<UserSummary> {
    let cached = collect_cached_users(storage)?;
    resolve_user_from_slice(&cached, query)
}

/// Resolves an Atlassian user by `"me"`/`"self"`/`"currentUser"`, `accountId`, email, or partial
/// display name using cached SQLite users first, with optional live Jira assignable-user API fallback.
pub async fn resolve_user(
    storage: &Storage,
    client: Option<&AtlassianClient>,
    project_or_issue_key: Option<&str>,
    query: &str,
) -> Result<UserSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("User accountId, email, or display name is empty.");
    }

    if is_self_alias(q) {
        if let Some(api) = client {
            if let Some(me) = api.cached_me() {
                return Ok(me);
            }
            return api.get_myself().await;
        }
    }

    let mut cached = collect_cached_users(storage)?;
    if let Some(me) = client.and_then(|api| api.cached_me()) {
        if !me.id.trim().is_empty() && !cached.iter().any(|u| u.id == me.id) {
            cached.push(me);
        }
    }

    if !cached.is_empty() {
        match resolve_user_from_slice(&cached, q) {
            Ok(found) => return Ok(found),
            Err(err) if client.is_none() || err.to_string().starts_with("Ambiguous") => {
                return Err(err);
            }
            Err(_) => {}
        }
    }

    let Some(api) = client else {
        return resolve_user_from_slice(&cached, q);
    };

    let scope_key = project_or_issue_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            api.config()
                .default_project
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            storage
                .list_projects()
                .ok()
                .and_then(|list| list.into_iter().next().map(|p| p.key))
        });

    if let Some(scope) = scope_key {
        let remote_users = api.search_assignable_users(&scope, q).await?;
        let merged = merge_users(cached, remote_users);
        return resolve_user_from_slice(&merged, q);
    }

    // Fallback: check current authenticated user if no project scope is known yet.
    let me = api.get_myself().await?;
    let merged = merge_users(cached, vec![me]);
    resolve_user_from_slice(&merged, q)
}

/// After the server rejects an assignee value, try resolving once by name or email.
/// If `None`, the caller returns the original error as-is.
pub(crate) async fn resolve_rejected_assignee(
    client: &AtlassianClient,
    scope_key: &str,
    raw: &str,
    err: &anyhow::Error,
    require_assignee_field: bool,
) -> Option<Result<UserSummary>> {
    let http_err = err.downcast_ref::<HttpStatusError>()?;
    if !matches!(
        http_err.status,
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
    ) {
        return None;
    }
    if require_assignee_field {
        let has_assignee_error = serde_json::from_str::<Value>(&http_err.body)
            .ok()
            .and_then(|v| v.get("errors").and_then(|e| e.as_object()).cloned())
            .is_some_and(|obj| obj.contains_key("assignee"));
        if !has_assignee_error {
            return None;
        }
    }
    let q = raw.trim();
    if q.is_empty() {
        return None;
    }
    let users = client.search_assignable_users(scope_key, q).await.ok()?;
    match users.len() {
        0 => None,
        1 => Some(Ok(users.into_iter().next().unwrap())),
        _ => match resolve_user_from_slice(&users, q) {
            Ok(user) => Some(Ok(user)),
            Err(_) => {
                let refs: Vec<&UserSummary> = users.iter().collect();
                let candidates: Vec<Value> = users
                    .iter()
                    .map(|u| {
                        json!({
                            "id": u.id,
                            "display_name": u.display_name,
                            "email": u.email,
                        })
                    })
                    .collect();
                Some(Err(CodedError {
                    code: "ambiguous_user",
                    message: format!(
                        "Multiple users ({}) match '{q}'. Specify an accountId:\n{}",
                        users.len(),
                        format_user_candidates(&refs),
                    ),
                    details: Some(json!({ "candidates": candidates })),
                }
                .into()))
            }
        },
    }
}

fn is_self_alias(q: &str) -> bool {
    matches!(
        q.trim().to_ascii_lowercase().as_str(),
        "me" | "self" | "currentuser" | "currentuser()"
    )
}

fn merge_users(existing: Vec<UserSummary>, incoming: Vec<UserSummary>) -> Vec<UserSummary> {
    let mut by_id: HashMap<String, UserSummary> = HashMap::new();
    for user in existing.into_iter().chain(incoming) {
        let id = user.id.trim();
        if id.is_empty() {
            continue;
        }
        by_id
            .entry(id.to_string())
            .and_modify(|cur| {
                if cur.display_name.trim().is_empty() && !user.display_name.trim().is_empty() {
                    cur.display_name = user.display_name.clone();
                }
                if cur.email.is_none() && user.email.is_some() {
                    cur.email = user.email.clone();
                }
            })
            .or_insert(user);
    }
    let mut out: Vec<UserSummary> = by_id.into_values().collect();
    out.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

fn format_project_candidates(candidates: &[&ProjectSummary]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|p| format!("  - {} — {} (id: {})", p.key, p.name, p.id))
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_CANDIDATE_LIST));
    }
    lines.join("\n")
}

fn format_board_candidates(candidates: &[&BoardSummary]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|b| match b.project_key.as_deref() {
            Some(pk) if !pk.is_empty() => {
                format!("  - #{} {} [{}] ({})", b.id, b.name, pk, b.board_type)
            }
            _ => format!("  - #{} {} ({})", b.id, b.name, b.board_type),
        })
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_CANDIDATE_LIST));
    }
    lines.join("\n")
}

fn format_space_candidates(candidates: &[&SpaceSummary]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|s| format!("  - {} — {} ({})", s.key, s.name, s.space_type))
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_CANDIDATE_LIST));
    }
    lines.join("\n")
}

fn format_user_candidates(candidates: &[&UserSummary]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|u| match u.email.as_deref() {
            Some(email) if !email.is_empty() => {
                format!("  - {} <{}> (id: {})", u.display_name, email, u.id)
            }
            _ => format!("  - {} (id: {})", u.display_name, u.id),
        })
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_CANDIDATE_LIST));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    use crate::{
        auth::{AuthManager, StoredCredentials},
        config::{AppConfig, DeploymentKind},
        models::IssueSummary,
    };

    fn sample_project(id: &str, key: &str, name: &str) -> ProjectSummary {
        ProjectSummary {
            id: id.into(),
            key: key.into(),
            name: name.into(),
            project_type: "software".into(),
            lead_name: None,
            avatar_url: None,
            pinned: false,
        }
    }

    fn sample_board(id: i64, name: &str, project_key: Option<&str>) -> BoardSummary {
        BoardSummary {
            id,
            name: name.into(),
            board_type: "scrum".into(),
            project_key: project_key.map(str::to_string),
            project_name: None,
            columns: Vec::new(),
        }
    }

    fn sample_space(id: &str, key: &str, name: &str) -> SpaceSummary {
        SpaceSummary {
            id: id.into(),
            key: key.into(),
            name: name.into(),
            space_type: "global".into(),
            homepage_id: None,
            description: None,
            web_url: format!("https://acme.atlassian.net/wiki/spaces/{key}"),
            pinned: false,
        }
    }

    fn sample_user(id: &str, display_name: &str, email: Option<&str>) -> UserSummary {
        UserSummary {
            id: id.into(),
            display_name: display_name.into(),
            email: email.map(str::to_string),
            avatar_url: None,
            active: true,
        }
    }

    #[test]
    fn test_resolve_project_priorities_and_errors() {
        let projects = vec![
            sample_project("10001", "ENG", "Core Engineering"),
            sample_project("10002", "PLAT", "Platform Infrastructure"),
            sample_project("10003", "OPS", "Platform Operations"),
        ];

        assert_eq!(resolve_project_from_slice(&projects, "eng").unwrap().key, "ENG");
        assert_eq!(resolve_project_from_slice(&projects, "10002").unwrap().key, "PLAT");
        assert_eq!(
            resolve_project_from_slice(&projects, "core engineering")
                .unwrap()
                .key,
            "ENG"
        );
        assert_eq!(resolve_project_from_slice(&projects, "Core").unwrap().key, "ENG");
        assert_eq!(resolve_project_from_slice(&projects, "Infra").unwrap().key, "PLAT");

        // Ambiguous prefix lists candidates clearly.
        let amb = resolve_project_from_slice(&projects, "Platform")
            .unwrap_err()
            .to_string();
        assert!(amb.contains("Ambiguous"));
        assert!(amb.contains("PLAT"));
        assert!(amb.contains("OPS"));

        // Empty and not-found errors.
        assert!(resolve_project_from_slice(&projects, "   ").is_err());
        let missing = resolve_project_from_slice(&projects, "MISSING")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("not found"));
    }

    #[test]
    fn test_resolve_board_by_id_project_name_and_ambiguity() {
        let boards = vec![
            sample_board(10, "ENG Sprint Board", Some("ENG")),
            sample_board(20, "Platform Kanban", Some("PLAT")),
            sample_board(21, "Platform Release", Some("PLAT")),
        ];

        assert_eq!(resolve_board_from_slice(&boards, "10").unwrap().id, 10);
        assert_eq!(resolve_board_from_slice(&boards, "#20").unwrap().id, 20);
        assert_eq!(resolve_board_from_slice(&boards, "eng").unwrap().id, 10);
        assert_eq!(
            resolve_board_from_slice(&boards, "Platform Release")
                .unwrap()
                .id,
            21
        );
        assert_eq!(resolve_board_from_slice(&boards, "Kanban").unwrap().id, 20);

        // Ambiguous project key with two boards.
        let amb_proj = resolve_board_from_slice(&boards, "PLAT")
            .unwrap_err()
            .to_string();
        assert!(amb_proj.contains("#20"));
        assert!(amb_proj.contains("#21"));

        // Ambiguous prefix.
        let amb_pref = resolve_board_from_slice(&boards, "Platform")
            .unwrap_err()
            .to_string();
        assert!(amb_pref.contains("Ambiguous"));
        assert!(amb_pref.contains("Platform Kanban"));
        assert!(amb_pref.contains("Platform Release"));

        assert!(resolve_board_from_slice(&boards, "999").is_err());
    }

    #[test]
    fn test_resolve_space_priorities_and_errors() {
        let spaces = vec![
            sample_space("sp-1", "ENG", "Engineering Handbook"),
            sample_space("sp-2", "ENGSEC", "Engineering Security"),
            sample_space("sp-3", "DESIGN", "Product Design"),
        ];

        assert_eq!(resolve_space_from_slice(&spaces, "eng").unwrap().key, "ENG");
        assert_eq!(resolve_space_from_slice(&spaces, "sp-2").unwrap().key, "ENGSEC");
        assert_eq!(
            resolve_space_from_slice(&spaces, "product design")
                .unwrap()
                .key,
            "DESIGN"
        );
        assert_eq!(
            resolve_space_from_slice(&spaces, "Security").unwrap().key,
            "ENGSEC"
        );

        let amb = resolve_space_from_slice(&spaces, "Engineering")
            .unwrap_err()
            .to_string();
        assert!(amb.contains("ENG"));
        assert!(amb.contains("ENGSEC"));

        assert!(resolve_space_from_slice(&spaces, "unknown").is_err());
    }

    #[test]
    fn test_resolve_user_by_account_id_email_and_partial_name() {
        let users = vec![
            sample_user("acc-100", "Alice Smith", Some("alice.smith@acme.com")),
            sample_user("acc-200", "Bob Jones", Some("bob.jones@acme.com")),
            sample_user("acc-300", "Alice Cooper", Some("alice.cooper@acme.com")),
        ];

        assert_eq!(resolve_user_from_slice(&users, "acc-200").unwrap().id, "acc-200");
        assert_eq!(
            resolve_user_from_slice(&users, "ALICE.SMITH@ACME.COM")
                .unwrap()
                .id,
            "acc-100"
        );
        assert_eq!(
            resolve_user_from_slice(&users, "Bob Jones").unwrap().id,
            "acc-200"
        );
        assert_eq!(resolve_user_from_slice(&users, "Cooper").unwrap().id, "acc-300");

        let amb = resolve_user_from_slice(&users, "Alice")
            .unwrap_err()
            .to_string();
        assert!(amb.contains("Alice Smith"));
        assert!(amb.contains("Alice Cooper"));
        assert!(amb.contains("acc-100"));
        assert!(amb.contains("acc-300"));

        assert!(resolve_user_from_slice(&users, "charlie").is_err());
    }

    #[test]
    fn test_collect_cached_users_from_sqlite_issues() {
        let storage = Storage::open_in_memory().unwrap();
        let issue = IssueSummary {
            id: "1".into(),
            key: "ENG-1".into(),
            project_key: "ENG".into(),
            summary: "Fix auth".into(),
            assignee: Some(sample_user("acc-a", "Alice Smith", Some("alice@acme.com"))),
            reporter: Some(sample_user("acc-b", "Bob Jones", None)),
            ..Default::default()
        };
        storage.upsert_issues(&[issue]).unwrap();

        let alice = resolve_user_cached(&storage, "alice@acme.com").unwrap();
        assert_eq!(alice.id, "acc-a");
        let bob = resolve_user_cached(&storage, "Bob").unwrap();
        assert_eq!(bob.id, "acc-b");
    }

    #[tokio::test]
    async fn test_async_resolvers_use_cache_and_api_fallback_preserving_errors() {
        let proj_hits = Arc::new(AtomicUsize::new(0));
        let board_hits = Arc::new(AtomicUsize::new(0));
        let space_hits = Arc::new(AtomicUsize::new(0));
        let user_hits = Arc::new(AtomicUsize::new(0));
        let fail_mode = Arc::new(AtomicBool::new(false));

        let ph = proj_hits.clone();
        let pf = fail_mode.clone();
        let bh = board_hits.clone();
        let sh = space_hits.clone();
        let uh = user_hits.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/project/search",
                get(move || {
                    let hits = ph.clone();
                    let fail = pf.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(json!({"errorMessages": ["Forbidden"]})),
                            )
                                .into_response();
                        }
                        Json(json!({
                            "values": [{
                                "id": "10001",
                                "key": "ENG",
                                "name": "Engineering",
                                "projectTypeKey": "software"
                            }]
                        }))
                        .into_response()
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board",
                get(move || {
                    let hits = bh.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "values": [{
                                "id": 42,
                                "name": "ENG Scrum",
                                "type": "scrum",
                                "location": { "projectKey": "ENG", "projectName": "Engineering" }
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(move || {
                    let hits = sh.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "results": [{
                                "id": "sp-1",
                                "key": "DOCS",
                                "name": "Engineering Docs",
                                "type": "global"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/3/user/assignable/search",
                get(move || {
                    let hits = uh.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!([{
                            "accountId": "acc-99",
                            "displayName": "Dana Scully",
                            "emailAddress": "dana@acme.com",
                            "active": true
                        }]))
                    }
                }),
            )
            .route(
                "/rest/api/3/myself",
                get(|| async {
                    Json(json!({
                        "accountId": "acc-me",
                        "displayName": "Current User",
                        "emailAddress": "me@acme.com",
                        "active": true
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base_url = format!("http://{addr}");
        let cfg = AppConfig {
            site_url: base_url.clone(),
            deployment: DeploymentKind::Cloud,
            default_project: Some("ENG".into()),
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url,
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        // 1. Cold cache falls back to API once, then serves from SQLite cache.
        let p1 = resolve_project(&storage, Some(&client), "engin")
            .await
            .unwrap();
        assert_eq!(p1.key, "ENG");
        assert_eq!(proj_hits.load(Ordering::SeqCst), 1);

        let p2 = resolve_project(&storage, Some(&client), "ENG")
            .await
            .unwrap();
        assert_eq!(p2.id, "10001");
        assert_eq!(proj_hits.load(Ordering::SeqCst), 1);

        // 2. Board & space fallback + caching.
        let b = resolve_board(&storage, Some(&client), "scrum")
            .await
            .unwrap();
        assert_eq!(b.id, 42);
        assert_eq!(board_hits.load(Ordering::SeqCst), 1);

        let s = resolve_space(&storage, Some(&client), "docs")
            .await
            .unwrap();
        assert_eq!(s.key, "DOCS");
        assert_eq!(space_hits.load(Ordering::SeqCst), 1);

        // 3. User resolution via "me" alias and assignable-user API fallback.
        let me = resolve_user(&storage, Some(&client), None, "me")
            .await
            .unwrap();
        assert_eq!(me.id, "acc-me");

        let dana = resolve_user(&storage, Some(&client), Some("ENG"), "scully")
            .await
            .unwrap();
        assert_eq!(dana.id, "acc-99");
        assert_eq!(user_hits.load(Ordering::SeqCst), 1);

        // 4. 403 API error on cold cache is preserved instead of masked as "not found".
        fail_mode.store(true, Ordering::SeqCst);
        let cold_storage = Storage::open_in_memory().unwrap();
        let err = resolve_project(&cold_storage, Some(&client), "ENG")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("403") || err.contains("Forbidden"),
            "expected preserved 403 error, got: {err}"
        );

        server.abort();
    }

    #[test]
    fn test_resolve_project_across_sites_disambiguation_and_site_filter() {
        let candidates = vec![
            SiteCandidate {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                item: sample_project("10001", "ENG", "Acme Engineering"),
            },
            SiteCandidate {
                site_id: "corp-dc".into(),
                site_name: "Corp Data Center".into(),
                item: sample_project("20001", "ENG", "Corp Engineering"),
            },
            SiteCandidate {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                item: sample_project("10002", "PLAT", "Platform Services"),
            },
        ];

        // Ambiguous key across sites returns actionable error with each matching site.
        let err = resolve_project_across_sites("ENG", None, &candidates)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Ambiguous project 'ENG' matched multiple sites: ENG (acme-cloud), ENG (corp-dc). Pass --site <id> to disambiguate."
        );

        // Passing --site <id> disambiguates cleanly.
        let acme = resolve_project_across_sites("eng", Some("acme-cloud"), &candidates).unwrap();
        assert_eq!(acme.site_id, "acme-cloud");
        assert_eq!(acme.item.id, "10001");

        let corp =
            resolve_project_across_sites("ENG", Some("Corp Data Center"), &candidates).unwrap();
        assert_eq!(corp.site_id, "corp-dc");
        assert_eq!(corp.item.id, "20001");

        // Unique key across all sites resolves without --site.
        let plat = resolve_project_across_sites("PLAT", None, &candidates).unwrap();
        assert_eq!(plat.site_id, "acme-cloud");
        assert_eq!(plat.item.key, "PLAT");

        // Unknown site filter returns clear error listing available sites.
        let missing_site = resolve_project_across_sites("ENG", Some("ghost"), &candidates)
            .unwrap_err()
            .to_string();
        assert!(missing_site.contains("Site 'ghost' not found"));
        assert!(missing_site.contains("acme-cloud"));
        assert!(missing_site.contains("corp-dc"));
    }

    #[test]
    fn test_resolve_space_and_board_across_sites_disambiguation_and_site_filter() {
        let space_candidates = vec![
            SiteCandidate {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                item: sample_space("sp-1", "DOCS", "Acme Docs"),
            },
            SiteCandidate {
                site_id: "corp-dc".into(),
                site_name: "Corp DC".into(),
                item: sample_space("sp-2", "DOCS", "Corp Docs"),
            },
            SiteCandidate {
                site_id: "corp-dc".into(),
                site_name: "Corp DC".into(),
                item: sample_space("sp-3", "OPS", "Operations"),
            },
        ];

        let space_err = resolve_space_across_sites("DOCS", None, &space_candidates)
            .unwrap_err()
            .to_string();
        assert_eq!(
            space_err,
            "Ambiguous space 'DOCS' matched multiple sites: DOCS (acme-cloud), DOCS (corp-dc). Pass --site <id> to disambiguate."
        );

        let corp_docs =
            resolve_space_across_sites("docs", Some("corp-dc"), &space_candidates).unwrap();
        assert_eq!(corp_docs.site_id, "corp-dc");
        assert_eq!(corp_docs.item.id, "sp-2");

        let ops = resolve_space_across_sites("OPS", None, &space_candidates).unwrap();
        assert_eq!(ops.site_id, "corp-dc");
        assert_eq!(ops.item.key, "OPS");

        let board_candidates = vec![
            SiteCandidate {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                item: sample_board(10, "ENG Sprint", Some("ENG")),
            },
            SiteCandidate {
                site_id: "corp-dc".into(),
                site_name: "Corp DC".into(),
                item: sample_board(10, "Core Sprint", Some("ENG")),
            },
            SiteCandidate {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                item: sample_board(25, "Platform Kanban", Some("PLAT")),
            },
        ];

        // Colliding board ID across sites requires --site.
        let board_err = resolve_board_across_sites("#10", None, &board_candidates)
            .unwrap_err()
            .to_string();
        assert!(board_err.contains("Ambiguous board '#10' matched multiple sites:"));
        assert!(board_err.contains("#10 ENG Sprint (acme-cloud)"));
        assert!(board_err.contains("#10 Core Sprint (corp-dc)"));
        assert!(board_err.contains("Pass --site <id> to disambiguate."));

        // Colliding project key across sites requires --site.
        let board_proj_err = resolve_board_across_sites("ENG", None, &board_candidates)
            .unwrap_err()
            .to_string();
        assert!(board_proj_err.contains("matched multiple sites"));
        assert!(board_proj_err.contains("acme-cloud"));
        assert!(board_proj_err.contains("corp-dc"));

        // Passing --site resolves cleanly.
        let acme_board =
            resolve_board_across_sites("10", Some("acme-cloud"), &board_candidates).unwrap();
        assert_eq!(acme_board.site_id, "acme-cloud");
        assert_eq!(acme_board.item.name, "ENG Sprint");

        let unique_board = resolve_board_across_sites("Kanban", None, &board_candidates).unwrap();
        assert_eq!(unique_board.site_id, "acme-cloud");
        assert_eq!(unique_board.item.id, 25);
    }
}
