//! Background `CoreEvent` reconciliation, revision checks, and 404 purge helpers.

use crate::{
    models::{BoardSummary, CoreEvent, IssueSummary, PageSummary, SpaceSummary},
    sync,
};

use super::{
    helpers::SiteKey,
    state::UiState,
    AppState,
};

impl AppState {
    // ------------------------------------------------------------------
    // Event handlers
    // ------------------------------------------------------------------

    pub(super) fn maybe_refresh_open_items_after_sync(&mut self, ui: &mut UiState) {
        if let Some((sid, key)) = self.current_open_issue_identity() {
            let stale = self
                .storage
                .is_issue_detail_stale_for_site(&sid, &key)
                .unwrap_or(false);
            if stale {
                let row_updated = self
                    .storage
                    .get_issue_row_updated_at_for_site(&sid, &key)
                    .ok()
                    .flatten();
                self.request_issue_detail_for_site_if_needed(
                    ui,
                    Some(&sid),
                    &key,
                    row_updated.as_deref(),
                );
            }
        }
        if let Some((sid, pid)) = self.current_open_page_identity() {
            let stale = self
                .storage
                .is_page_detail_stale_for_site(&sid, &pid)
                .unwrap_or(false);
            if stale {
                let row_ver = self
                    .storage
                    .get_page_row_version_for_site(&sid, &pid)
                    .ok()
                    .flatten();
                self.request_page_detail_for_site_if_needed(ui, Some(&sid), &pid, row_ver);
            }
        }
    }

    pub(super) fn apply_issue_summary_to_lists(&mut self, site_id: &str, summary: &IssueSummary) {
        if self
            .board_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or(true)
        {
            if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == summary.key) {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
        for (idx, it) in self.issues.iter_mut().enumerate() {
            let row_matches_site = self
                .issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if row_matches_site && it.key == summary.key {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
        for (idx, it) in self.my_issues.iter_mut().enumerate() {
            let row_matches_site = self
                .my_issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if row_matches_site && it.key == summary.key {
                let prev_comments = it.comment_count;
                let prev_atts = it.attachment_count;
                *it = summary.clone();
                if it.comment_count == 0 && prev_comments > 0 {
                    it.comment_count = prev_comments;
                }
                if it.attachment_count == 0 && prev_atts > 0 {
                    it.attachment_count = prev_atts;
                }
            }
        }
    }

    pub(super) fn issue_revision_needs_detail(&self, site_id: &str, incoming: &IssueSummary) -> bool {
        let Some(cur) = self.active_issue.as_ref() else {
            return true;
        };
        let site_matches = self
            .active_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or_else(|| {
                self.effective_site_id_and_client()
                    .0
                    .eq_ignore_ascii_case(site_id)
            });
        if cur.summary.key != incoming.key || !site_matches {
            return true;
        }
        if self
            .storage
            .is_issue_detail_stale_for_site(site_id, &incoming.key)
            .unwrap_or(false)
        {
            return true;
        }
        match self.active_issue_detail_updated_at.as_deref() {
            None => return true,
            Some(detail_ts) => {
                if crate::storage::jira::compare_updated_at(&incoming.updated_at, detail_ts)
                    == std::cmp::Ordering::Greater
                {
                    return true;
                }
            }
        }
        incoming.summary != cur.summary.summary
            || incoming.status != cur.summary.status
            || incoming.priority != cur.summary.priority
            || incoming.assignee != cur.summary.assignee
            || incoming.labels != cur.summary.labels
    }

    pub(super) fn page_revision_needs_detail(&self, site_id: &str, incoming: &PageSummary) -> bool {
        let Some(cur) = self.active_page.as_ref() else {
            return true;
        };
        let site_matches = self
            .active_page_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or_else(|| {
                self.effective_site_id_and_client()
                    .0
                    .eq_ignore_ascii_case(site_id)
            });
        if cur.summary.id != incoming.id || !site_matches {
            return true;
        }
        if self
            .storage
            .is_page_detail_stale_for_site(site_id, &incoming.id)
            .unwrap_or(false)
        {
            return true;
        }
        match self.active_page_detail_version {
            None => return true,
            Some(detail_ver) if incoming.version_number > detail_ver => return true,
            _ => {}
        }
        incoming.title != cur.summary.title
            || crate::storage::jira::compare_updated_at(&incoming.updated_at, &cur.summary.updated_at)
                == std::cmp::Ordering::Greater
    }

    pub(super) fn purge_deleted_issue(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        key: &str,
        notify_toast: bool,
    ) {
        let _ = self.storage.delete_issue_for_site(site_id, key);
        let mut idx = 0usize;
        while idx < self.issues.len() {
            let matches_site = self
                .issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.issues[idx].key == key {
                self.issues.remove(idx);
                if idx < self.issue_site_ids.len() {
                    self.issue_site_ids.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
        if self
            .board_issue_site_id
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(site_id))
            .unwrap_or(true)
        {
            self.board_issues.retain(|i| i.key != key);
        }
        self.remove_my_issue(site_id, key);
        let mut hit_idx = 0usize;
        while hit_idx < self.search_hits.len() {
            let matches_site = self
                .search_hit_site_ids
                .get(hit_idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site
                && self.search_hits[hit_idx].kind == crate::models::SearchHitKind::JiraIssue
                && self.search_hits[hit_idx].id_or_key == key
            {
                self.search_hits.remove(hit_idx);
                if hit_idx < self.search_hit_site_ids.len() {
                    self.search_hit_site_ids.remove(hit_idx);
                }
                if hit_idx < self.search_hit_site_labels.len() {
                    self.search_hit_site_labels.remove(hit_idx);
                }
            } else {
                hit_idx += 1;
            }
        }
        let cleared_active = self.active_issue.as_ref().is_some_and(|cur| {
            cur.summary.key == key
                && self
                    .active_issue_site_id
                    .as_deref()
                    .map(|s| s.eq_ignore_ascii_case(site_id))
                    .unwrap_or(true)
        }) || self.active_issue_target.as_ref().is_some_and(|(s, k)| {
            k == key && s.eq_ignore_ascii_case(site_id)
        });
        if cleared_active {
            self.active_issue = None;
            self.active_issue_site_id = None;
            self.active_issue_target = None;
            self.active_issue_detail_updated_at = None;
            self.in_flight_issue_detail = None;
            if notify_toast {
                ui.info(format!("{key} was deleted remotely and removed from cache"));
            }
        }
        self.refresh_scoped_cache_counts();
        self.clamp_selections(ui);
        self.sync_open_item_watch(ui);
    }

    pub(super) fn purge_deleted_page(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        page_id: &str,
        notify_toast: bool,
    ) {
        let _ = self.storage.delete_page_for_site(site_id, page_id);
        for (k, roots) in self.root_pages_by_space.iter_mut() {
            if k.is_site(site_id) {
                roots.retain(|p| p.id != page_id);
            }
        }
        for (k, children) in self.page_children_map.iter_mut() {
            if k.is_site(site_id) {
                children.retain(|p| p.id != page_id);
            }
        }
        self.page_children_map
            .remove(&SiteKey::new(site_id, page_id));
        self.expanded_pages.remove(&SiteKey::new(site_id, page_id));
        self.remove_my_page(site_id, page_id);
        let mut hit_idx = 0usize;
        while hit_idx < self.search_hits.len() {
            let matches_site = self
                .search_hit_site_ids
                .get(hit_idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site
                && self.search_hits[hit_idx].kind == crate::models::SearchHitKind::ConfluencePage
                && self.search_hits[hit_idx].id_or_key == page_id
            {
                self.search_hits.remove(hit_idx);
                if hit_idx < self.search_hit_site_ids.len() {
                    self.search_hit_site_ids.remove(hit_idx);
                }
                if hit_idx < self.search_hit_site_labels.len() {
                    self.search_hit_site_labels.remove(hit_idx);
                }
            } else {
                hit_idx += 1;
            }
        }
        let cleared_active = self.active_page.as_ref().is_some_and(|cur| {
            cur.summary.id == page_id
                && self
                    .active_page_site_id
                    .as_deref()
                    .map(|s| s.eq_ignore_ascii_case(site_id))
                    .unwrap_or(true)
        }) || self.active_page_target.as_ref().is_some_and(|(s, p)| {
            p == page_id && s.eq_ignore_ascii_case(site_id)
        });
        if cleared_active {
            self.active_page = None;
            self.active_page_site_id = None;
            self.active_page_target = None;
            self.active_page_detail_version = None;
            self.in_flight_page_detail = None;
            if notify_toast {
                ui.info(format!(
                    "Page #{page_id} was deleted remotely and removed from cache"
                ));
            }
        }
        self.refresh_scoped_cache_counts();
        self.clamp_selections(ui);
        self.sync_open_item_watch(ui);
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::SyncStarted => {
                self.syncing = true;
            }
            CoreEvent::SyncFinished { summary } => {
                self.syncing = false;
                self.sync_summary = summary.clone();
                self.refresh_scoped_cache_counts();
                self.reload_my_issues_from_storage();
                self.reload_my_pages_from_storage();
                self.maybe_refresh_open_items_after_sync(ui);
                self.clamp_selections(ui);
                ui.tasks.finish("Sync", None);
                ui.success(summary);
            }
            CoreEvent::ProjectsUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.project_site_ids = vec![sid; items.len()];
                    self.projects = items;
                } else {
                    self.reload_projects_all_sites();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::BoardsUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.board_site_ids = vec![sid; items.len()];
                    self.boards = items;
                } else if let Ok(all) = self.storage.list_boards_all_sites(None) {
                    let filtered: Vec<(String, BoardSummary)> = all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.board_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.boards = filtered.into_iter().map(|(_, b)| b).collect();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_issue_identity().and_then(|(open_sid, open_key)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    issues.iter().find(|i| i.key == open_key).and_then(|inc| {
                        self.issue_revision_needs_detail(&effective_sid, inc)
                            .then(|| (open_sid, open_key, inc.updated_at.clone()))
                    })
                });
                let matches_active_board = self.active_board(ui).is_none_or(|b| b.id == board_id)
                    && self
                        .active_board_site_id(ui)
                        .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                        .unwrap_or_else(|| self.site_scope.matches(&effective_sid));
                let selected_sprint_id = self
                    .active_board_sprint_id
                    .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
                let incoming_sprint_id = sprint.as_ref().map(|s| s.id);
                let sprint_matches = match (selected_sprint_id, incoming_sprint_id) {
                    (Some(sel), Some(inc)) => sel == inc,
                    (Some(sel), None) => self
                        .sprints
                        .iter()
                        .find(|s| s.id == sel)
                        .is_none_or(|s| s.state.eq_ignore_ascii_case("active")),
                    (None, _) => true,
                };
                if matches_active_board {
                    if let Some(sp) = sprint {
                        if !self.sprints.iter().any(|s| s.id == sp.id) {
                            self.sprints.push(sp);
                        }
                    }
                    if sprint_matches {
                        self.board_issues = issues;
                        self.board_issue_site_id = Some(effective_sid);
                        if incoming_sprint_id.is_some() {
                            self.active_board_sprint_id = incoming_sprint_id;
                        }
                    }
                    self.clamp_selections(ui);
                }
                if let Some((sid, key, updated_at)) = active_refresh {
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&sid),
                        &key,
                        Some(&updated_at),
                    );
                }
            }
            CoreEvent::IssuesUpdated { site_id, issues } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_issue_identity().and_then(|(open_sid, open_key)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    issues.iter().find(|i| i.key == open_key).and_then(|inc| {
                        self.issue_revision_needs_detail(&effective_sid, inc)
                            .then(|| (open_sid, open_key, inc.updated_at.clone()))
                    })
                });
                if self.site_scope.matches(&effective_sid) {
                    if !self.issues_custom_query {
                        if self.is_multi_site_all_mode() {
                            if let Ok(all) = self.storage.list_issues_all_sites(None, 200) {
                                let filtered: Vec<(String, IssueSummary)> = all
                                    .into_iter()
                                    .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                                    .collect();
                                self.issue_site_ids =
                                    filtered.iter().map(|(s, _)| s.clone()).collect();
                                self.issues = filtered.into_iter().map(|(_, i)| i).collect();
                            }
                        } else {
                            self.issue_site_ids = vec![effective_sid.clone(); issues.len()];
                            self.issues = issues;
                        }
                    } else {
                        // Preserve custom JQL / Saved Filter rows, updating any matching issues in place.
                        for inc in &issues {
                            for (idx, it) in self.issues.iter_mut().enumerate() {
                                let row_matches_site = self
                                    .issue_site_ids
                                    .get(idx)
                                    .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                                    .unwrap_or(true);
                                if row_matches_site && it.key == inc.key {
                                    let prev_comments = it.comment_count;
                                    let prev_atts = it.attachment_count;
                                    *it = inc.clone();
                                    if it.comment_count == 0 && prev_comments > 0 {
                                        it.comment_count = prev_comments;
                                    }
                                    if it.attachment_count == 0 && prev_atts > 0 {
                                        it.attachment_count = prev_atts;
                                    }
                                }
                            }
                        }
                    }
                    self.reload_my_issues_from_storage();
                    self.clamp_selections(ui);
                }
                if let Some((sid, key, updated_at)) = active_refresh {
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&sid),
                        &key,
                        Some(&updated_at),
                    );
                }
            }
            CoreEvent::SpacesUpdated { site_id, items } => {
                let sid = self.event_site_or_effective(site_id);
                if !self.site_scope.matches(&sid) {
                    // Another site's list must not replace the one this scope shows.
                } else if !self.is_multi_site_all_mode() {
                    self.space_site_ids = vec![sid; items.len()];
                    self.spaces = items;
                } else if let Ok(all) = self.storage.list_spaces_all_sites() {
                    let filtered: Vec<(String, SpaceSummary)> = all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.space_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.spaces = filtered.into_iter().map(|(_, s)| s).collect();
                }
                self.clamp_selections(ui);
            }
            CoreEvent::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                let active_refresh = self.current_open_page_identity().and_then(|(open_sid, open_pid)| {
                    if !open_sid.eq_ignore_ascii_case(&effective_sid) {
                        return None;
                    }
                    pages.iter().find(|p| p.id == open_pid).and_then(|inc| {
                        self.page_revision_needs_detail(&effective_sid, inc)
                            .then_some((open_sid, open_pid, inc.version_number))
                    })
                });
                let matches_space_site = self
                    .active_space(ui)
                    .filter(|sp| sp.key == space_key)
                    .and_then(|_| self.active_space_site_id(ui))
                    .map(|s| s.eq_ignore_ascii_case(&effective_sid))
                    .unwrap_or_else(|| self.site_scope.matches(&effective_sid));
                if matches_space_site {
                    self.root_pages_by_space
                        .insert(SiteKey::new(&effective_sid, &space_key), pages);
                    self.auto_expand_space_home(ui, &effective_sid, &space_key);
                }
                if self.site_scope.matches(&effective_sid) {
                    self.reload_my_pages_from_storage();
                }
                self.clamp_selections(ui);
                if let Some((sid, pid, ver)) = active_refresh {
                    self.request_page_detail_for_site_if_needed(ui, Some(&sid), &pid, Some(ver));
                }
            }
            CoreEvent::OpenIssueRevision { site_id, summary } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                if sync::is_open_issue_not_found(&summary) {
                    self.purge_deleted_issue(ui, &effective_sid, &summary.key, true);
                    return;
                }
                let matches_open = self.current_open_issue_identity().is_some_and(|(sid, key)| {
                    sid.eq_ignore_ascii_case(&effective_sid) && key == summary.key
                });
                if !matches_open {
                    // Late response after site/item switch: persist strictly to original site cache only.
                    let _ = self
                        .storage
                        .upsert_issue_revision_for_site(&effective_sid, &summary);
                    return;
                }
                if self.issue_revision_needs_detail(&effective_sid, &summary) {
                    let _ = self
                        .storage
                        .upsert_issue_revision_for_site(&effective_sid, &summary);
                    self.apply_issue_summary_to_lists(&effective_sid, &summary);
                    self.request_issue_detail_for_site_if_needed(
                        ui,
                        Some(&effective_sid),
                        &summary.key,
                        Some(&summary.updated_at),
                    );
                }
            }
            CoreEvent::OpenPageRevision { site_id, summary } => {
                let effective_sid = if site_id.trim().is_empty() {
                    self.effective_site_id_and_client().0
                } else {
                    site_id
                };
                if sync::is_open_page_not_found(&summary) {
                    self.purge_deleted_page(ui, &effective_sid, &summary.id, true);
                    return;
                }
                let matches_open = self.current_open_page_identity().is_some_and(|(sid, pid)| {
                    sid.eq_ignore_ascii_case(&effective_sid) && pid == summary.id
                });
                if !matches_open {
                    // Late response after site/item switch: persist strictly to original site cache only.
                    let _ = self
                        .storage
                        .upsert_authoritative_pages_for_site(
                            &effective_sid,
                            std::slice::from_ref(&summary),
                        );
                    return;
                }
                if self.page_revision_needs_detail(&effective_sid, &summary) {
                    let _ = self
                        .storage
                        .upsert_authoritative_pages_for_site(
                            &effective_sid,
                            std::slice::from_ref(&summary),
                        );
                    self.request_page_detail_for_site_if_needed(
                        ui,
                        Some(&effective_sid),
                        &summary.id,
                        Some(summary.version_number),
                    );
                }
            }
            CoreEvent::SyncError(err) => {
                self.syncing = false;
                ui.tasks.finish("Sync", Some(err.clone()));
                ui.error(format!("Sync error: {err}"));
                self.notify_desktop("Atlassian Sync Error", &err);
            }
        }
    }

}
