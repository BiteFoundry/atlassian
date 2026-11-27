//! Asynchronous Jira requests: board/JQL load, issue detail, transitions, assignment, forms, comments.

use std::sync::atomic::Ordering;

use crate::{
    adf,
    atlassian::{client::is_not_found_error, AtlassianClient},
    models::{
        ConfluenceComment, IssueComment, IssueStatus, IssueSummary, IssueTransition,
        OutgoingIssue, UserSummary,
    },
    sync,
    ui::overlays::{
        assignee::AssigneeOverlay,
        comment::{CommentOverlay, CommentTarget},
        issue_form::IssueFormOverlay,
        transition::TransitionOverlay,
        Overlay,
    },
};

use super::{
    helpers::site_matches,
    state::{UiMsg, UiState},
    AppState, InFlightIssueDetail,
};

impl AppState {
    pub fn board_row_target(&self, row: usize) -> Option<(i64, Option<&str>)> {
        let board = self.boards.get(row)?;
        Some((board.id, self.board_site_ids.get(row).map(String::as_str)))
    }

    /// Loads the board shown in BOARDS row `row` (and `sprint_id`, when given) from that row's site.
    pub fn request_board_load(&mut self, ui: &mut UiState, row: usize, sprint_id: Option<i64>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some((board_id, board_site)) = self.board_row_target(row) else {
            return;
        };
        let (site_id, client) = self.site_id_and_client_for(board_site);
        self.board_data_req_seq = self.board_data_req_seq.wrapping_add(1);
        let seq = self.board_data_req_seq;
        ui.tasks.begin(format!("Board #{board_id}"));
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (columns, sprints, issues_res) = if let Some(sid) = sprint_id {
                let (cols_res, sprints_res, issues_res) = tokio::join!(
                    client.get_board_columns(board_id),
                    client.list_board_sprints(board_id),
                    sync::load_board_issues_for_site(
                        &client,
                        &storage,
                        &site_id,
                        board_id,
                        Some(sid),
                    ),
                );
                let sprints = sprints_res.unwrap_or_default();
                if !sprints.is_empty() {
                    let _ = storage.upsert_sprints_for_site(&site_id, &sprints);
                }
                (cols_res.unwrap_or_default(), sprints, issues_res)
            } else {
                let (cols_res, sprints_res) = tokio::join!(
                    client.get_board_columns(board_id),
                    client.list_board_sprints(board_id),
                );
                let sprints = sprints_res.unwrap_or_default();
                if !sprints.is_empty() {
                    let _ = storage.upsert_sprints_for_site(&site_id, &sprints);
                }
                let effective_sprint = sprints
                    .iter()
                    .find(|s| s.state.eq_ignore_ascii_case("active"))
                    .map(|s| s.id);
                let issues_res = sync::load_board_issues_for_site(
                    &client,
                    &storage,
                    &site_id,
                    board_id,
                    effective_sprint,
                )
                .await;
                (cols_res.unwrap_or_default(), sprints, issues_res)
            };
            match issues_res {
                Ok(issues) => {
                    let _ = tx.send(UiMsg::BoardDataLoaded {
                        seq,
                        site_id,
                        board_id,
                        requested_sprint_id: sprint_id,
                        columns,
                        sprints,
                        issues,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::BoardLoadFailed {
                        seq,
                        site_id,
                        board_id,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_jql_search(&mut self, ui: &mut UiState, jql: String, label: String) {
        self.request_jql_search_for_site(ui, None, jql, label);
    }

    pub fn request_jql_search_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        jql: String,
        label: String,
    ) {
        self.issues_custom_query = true;
        self.active_jql_label = Some(label.clone());
        self.active_jql_site_id = explicit_site.map(str::to_string);
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.issues_req_seq = self.issues_req_seq.wrapping_add(1);
        let seq = self.issues_req_seq;
        ui.tasks.begin(format!("JQL: {label}"));
        let (site_id, client) = self.site_id_and_client_for(explicit_site);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match client.search_all_issues_jql(&jql, 200).await {
                Ok(collected) => {
                    let _ = storage.upsert_issues_for_site(&site_id, &collected.items);
                    let _ = tx.send(UiMsg::IssuesLoaded {
                        seq,
                        site_id,
                        issues: collected.items,
                        label,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssuesLoadFailed {
                        seq,
                        label,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_issue_detail(&mut self, ui: &mut UiState, key: &str) {
        self.request_issue_detail_for_site(ui, None, key);
    }

    pub fn request_issue_detail_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
    ) {
        self.request_issue_detail_internal(ui, explicit_site, key, None, true);
    }

    pub fn request_issue_detail_for_site_if_needed(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
        expected_updated_at: Option<&str>,
    ) -> bool {
        self.request_issue_detail_internal(ui, explicit_site, key, expected_updated_at, false)
    }

    pub(super) fn request_issue_detail_internal(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        key: &str,
        expected_updated_at: Option<&str>,
        force_open: bool,
    ) -> bool {
        let resolved_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_issue_target
                    .as_ref()
                    .filter(|(_, k)| k == key)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                self.active_issue
                    .as_ref()
                    .filter(|d| d.summary.key == key)
                    .and_then(|_| self.active_issue_site_id.clone())
            })
            .or_else(|| {
                self.issues
                    .iter()
                    .position(|i| i.key == key)
                    .and_then(|idx| self.issue_site_ids.get(idx))
                    .cloned()
            });
        let (site_id, client) = self.site_id_and_client_for(resolved_site.as_deref());

        if !force_open {
            if let Some(ref inflight) = self.in_flight_issue_detail {
                if inflight.site_id.eq_ignore_ascii_case(&site_id) && inflight.key == key {
                    let already_covers = match (
                        expected_updated_at.map(str::trim).filter(|s| !s.is_empty()),
                        inflight.target_updated_at.as_deref(),
                    ) {
                        (None, _) => true,
                        (Some(inc), Some(cur)) => {
                            crate::storage::jira::compare_updated_at(inc, cur)
                                != std::cmp::Ordering::Greater
                        }
                        (Some(_), None) => false,
                    };
                    if already_covers {
                        return false;
                    }
                }
            }
        }

        let same_open_item = self
            .active_issue
            .as_ref()
            .is_some_and(|d| d.summary.key == key)
            && self
                .active_issue_site_id
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(&site_id))
                .unwrap_or_else(|| {
                    self.effective_site_id_and_client()
                        .0
                        .eq_ignore_ascii_case(&site_id)
                });

        if !same_open_item {
            if let Ok(Some(cached)) = self.storage.get_issue_detail_for_site(&site_id, key) {
                let detail_ts = self
                    .storage
                    .get_issue_detail_updated_at_for_site(&site_id, key)
                    .ok()
                    .flatten();
                self.active_issue = Some(cached);
                self.active_issue_detail_updated_at = detail_ts;
            } else {
                self.active_issue = None;
                self.active_issue_detail_updated_at = None;
            }
            ui.issue_ui.detail_scroll = 0;
            ui.board_ui.detail_scroll = 0;
        }
        self.active_issue_site_id = Some(site_id.clone());
        self.active_issue_target = Some((site_id.clone(), key.to_string()));
        self.issue_detail_req_seq = self.issue_detail_req_seq.wrapping_add(1);
        let seq = self.issue_detail_req_seq;
        let target_updated = expected_updated_at
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.storage
                    .get_issue_row_updated_at_for_site(&site_id, key)
                    .ok()
                    .flatten()
            });
        self.in_flight_issue_detail = Some(InFlightIssueDetail {
            site_id: site_id.clone(),
            key: key.to_string(),
            target_updated_at: target_updated,
            seq,
        });
        if tokio::runtime::Handle::try_current().is_err() {
            return true;
        }
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.get_issue_with_meta(&key_owned).await {
                Ok((mut detail, meta)) => {
                    let _ = storage.upsert_issue_detail_for_site_merging(
                        &site_id,
                        &mut detail,
                        !meta.remote_links_ok,
                    );
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        key: key_owned,
                        detail: Ok(detail),
                        not_found: false,
                    });
                }
                Err(e) => {
                    let not_found = is_not_found_error(&e);
                    if not_found {
                        let _ = storage.delete_issue_for_site(&site_id, &key_owned);
                    }
                    let _ = tx.send(UiMsg::IssueDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        key: key_owned,
                        detail: Err(e.to_string()),
                        not_found,
                    });
                }
            }
        });
        true
    }

    pub fn request_open_transitions(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        issue_key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let mut overlay = TransitionOverlay::new(issue_key);
        overlay.site_id = site.clone();
        ui.push_overlay(Overlay::Transition(overlay));
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let seq = self
            .transitions_req_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let (resolved_site, client) = self.site_id_and_client_for(site.as_deref());
        let tx = self.ui_tx.clone();
        let key = issue_key.to_string();
        tokio::spawn(async move {
            let res = client.list_transitions(&key).await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::TransitionsLoaded {
                seq,
                site_id: resolved_site,
                issue_key: key,
                transitions: res,
            });
        });
    }

    pub fn request_transition_issue(
        &mut self,
        ui: &mut UiState,
        key: &str,
        tr: IssueTransition,
    ) {
        self.request_transition_issue_for_site(ui, None, key, tr);
    }

    pub fn request_transition_issue_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        tr: IssueTransition,
    ) {
        let site = self.issue_write_site(site_id);
        // Capture previous status for clean rollback on API error.
        let prev_status = self.current_issue_status(site.as_deref(), key);
        // 0ms optimistic local column update so Kanban card moves right away!
        self.apply_local_status_update(site.as_deref(), key, &tr.to_status);
        self.clamp_selections(ui);
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} → {}", tr.to_status.name));
            return;
        }
        ui.tasks.begin(format!("Transition {key}"));
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            match client.transition_issue(&key_owned, &tr.id, None).await {
                Ok(_) => {
                    let _ = storage.update_issue_status_for_site(
                        &site_id,
                        &key_owned,
                        &tr.to_status,
                    );
                    let _ = tx.send(UiMsg::IssueTransitioned {
                        site_id: site_id.clone(),
                        key: key_owned.clone(),
                        to_status: tr.to_status.clone(),
                        toast: format!("{key_owned} → {}", tr.to_status.name),
                    });
                    if let Ok((mut detail, meta)) = client.get_issue_with_meta(&key_owned).await {
                        let _ = storage.upsert_issue_detail_for_site_merging(
                            &site_id,
                            &mut detail,
                            !meta.remote_links_ok,
                        );
                        let _ = tx.send(UiMsg::IssueDetailLoaded {
                            seq: None,
                            site_id,
                            key: key_owned,
                            detail: Ok(detail),
                            not_found: false,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueTransitionFailed {
                        site_id,
                        key: key_owned,
                        previous_status: prev_status,
                        error: format!("Transition failed: {e}"),
                    });
                }
            }
        });
    }

    /// Site a write for an issue goes to: the explicit site carried by the
    /// overlay/row, else the site of the opened issue.
    pub(super) fn issue_write_site(&self, explicit: Option<&str>) -> Option<String> {
        explicit
            .map(str::to_string)
            .or_else(|| self.active_issue_site_id.clone())
    }

    /// Finds the locally listed issue `key` on `site_id` (`None` = any site).
    pub(super) fn find_local_issue(&self, site_id: Option<&str>, key: &str) -> Option<&IssueSummary> {
        if let Some(d) = self.active_issue.as_ref() {
            if d.summary.key == key && site_matches(self.active_issue_site_id.as_deref(), site_id) {
                return Some(&d.summary);
            }
        }
        if site_matches(self.board_issue_site_id.as_deref(), site_id) {
            if let Some(it) = self.board_issues.iter().find(|i| i.key == key) {
                return Some(it);
            }
        }
        let in_list = |list: &'_ [IssueSummary], sites: &'_ [String]| {
            list.iter().enumerate().find_map(|(idx, it)| {
                (it.key == key && site_matches(sites.get(idx).map(String::as_str), site_id))
                    .then_some(idx)
            })
        };
        if let Some(idx) = in_list(&self.issues, &self.issue_site_ids) {
            return self.issues.get(idx);
        }
        in_list(&self.my_issues, &self.my_issue_site_ids).and_then(|idx| self.my_issues.get(idx))
    }

    /// Applies `f` to every local copy of issue `key` that belongs to
    /// `site_id` (`None` = any site): board, list, My Work and the opened issue.
    pub(super) fn for_each_local_issue_mut(
        &mut self,
        site_id: Option<&str>,
        key: &str,
        mut f: impl FnMut(&mut IssueSummary),
    ) {
        if site_matches(self.board_issue_site_id.as_deref(), site_id) {
            for it in self.board_issues.iter_mut().filter(|i| i.key == key) {
                f(it);
            }
        }
        for (idx, it) in self.issues.iter_mut().enumerate() {
            if it.key == key
                && site_matches(self.issue_site_ids.get(idx).map(String::as_str), site_id)
            {
                f(it);
            }
        }
        for (idx, it) in self.my_issues.iter_mut().enumerate() {
            if it.key == key
                && site_matches(self.my_issue_site_ids.get(idx).map(String::as_str), site_id)
            {
                f(it);
            }
        }
        if let Some(detail) = &mut self.active_issue {
            if detail.summary.key == key
                && site_matches(self.active_issue_site_id.as_deref(), site_id)
            {
                f(&mut detail.summary);
            }
        }
    }

    pub(super) fn current_issue_status(&self, site_id: Option<&str>, key: &str) -> Option<IssueStatus> {
        self.find_local_issue(site_id, key).map(|i| i.status.clone())
    }

    pub(super) fn current_issue_assignee(
        &self,
        site_id: Option<&str>,
        key: &str,
    ) -> Option<Option<UserSummary>> {
        self.find_local_issue(site_id, key).map(|i| i.assignee.clone())
    }

    pub(super) fn apply_local_status_update(&mut self, site_id: Option<&str>, key: &str, to_status: &IssueStatus) {
        self.for_each_local_issue_mut(site_id, key, |it| it.status = to_status.clone());
    }

    pub(super) fn apply_local_assignee_update(
        &mut self,
        site_id: Option<&str>,
        key: &str,
        assignee: Option<UserSummary>,
    ) {
        self.for_each_local_issue_mut(site_id, key, |it| it.assignee = assignee.clone());
    }

    pub(super) fn bump_local_comment_count(&mut self, site_id: Option<&str>, key: &str, delta: isize) {
        self.for_each_local_issue_mut(site_id, key, |it| {
            if delta >= 0 {
                it.comment_count = it.comment_count.saturating_add(delta as usize);
            } else {
                it.comment_count = it.comment_count.saturating_sub((-delta) as usize);
            }
        });
    }

    pub fn request_assign_to_me(&mut self, ui: &mut UiState, key: &str) {
        self.request_assign_to_me_for_site(ui, None, key);
    }

    /// Identity of the signed-in user on `site_id`: the target client's own
    /// cached profile, the primary profile when the target *is* the primary site,
    /// or the persisted identity stored in SQLite for `site_id`. Account ids
    /// differ per site (ATL-25), so another site's "me" is never sent.
    pub(super) fn known_me_for_site(&self, site_id: &str, client: &AtlassianClient) -> Option<UserSummary> {
        client
            .cached_me()
            .or_else(|| {
                site_id
                    .eq_ignore_ascii_case(self.client.site_id())
                    .then(|| self.me.clone().or_else(|| self.client.cached_me()))
                    .flatten()
            })
            .or_else(|| self.storage.identity_for_site(site_id).ok().flatten())
    }

    pub fn request_assign_to_me_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let known_me = self.known_me_for_site(&site_id, &client);
        let has_runtime = tokio::runtime::Handle::try_current().is_ok();
        if known_me.is_none() && !has_runtime {
            ui.error("Current user profile has not been loaded yet");
            return;
        }
        let prev_assignee = self.current_issue_assignee(Some(&site_id), key);
        if let Some(me) = &known_me {
            self.apply_local_assignee_update(Some(&site_id), key, Some(me.clone()));
        }
        if !has_runtime {
            ui.success(format!("Assigned {key} to you"));
            return;
        }
        let task_label = format!("Assign {key} to me");
        ui.tasks.begin(&task_label);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            let me = match known_me {
                Some(me) => me,
                None => match client.get_myself().await {
                    Ok(me) => {
                        client.set_cached_me(me.clone());
                        let _ = storage.save_identity_for_site(&site_id, &me);
                        me
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueAssignFailed {
                            site_id,
                            key: key_owned,
                            previous_assignee: None,
                            task_label,
                            error: format!("Could not resolve your account on this site: {e}"),
                        });
                        return;
                    }
                },
            };
            match client.assign_issue(&key_owned, Some(&me.id)).await {
                Ok(()) => {
                    let _ = storage.update_issue_assignee_for_site(&site_id, &key_owned, Some(&me));
                    match client.get_issue_with_meta(&key_owned).await {
                        Ok((mut detail, meta)) => {
                            let _ = storage.upsert_issue_detail_for_site_merging(
                                &site_id,
                                &mut detail,
                                !meta.remote_links_ok,
                            );
                            let _ = tx.send(UiMsg::IssueMutated {
                                site_id,
                                detail,
                                toast: format!("Assigned {key_owned} to you"),
                                clear_draft_key: None,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(UiMsg::IssueAssignFailed {
                                site_id,
                                key: key_owned,
                                previous_assignee: None,
                                task_label,
                                error: format!("Assigned issue, but failed to reload detail: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        site_id,
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to assign to me: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_open_assignee_picker(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        project_key: &str,
    ) {
        let site = self.issue_write_site(site_id);
        let mut overlay = AssigneeOverlay::new(key, project_key);
        overlay.site_id = site.clone();
        let lookup_key = overlay.project_or_issue_key.clone();
        ui.push_overlay(Overlay::Assignee(overlay));
        self.request_assignable_users(site.as_deref(), &lookup_key, "");
    }

    pub fn request_assignable_users(
        &self,
        site_id: Option<&str>,
        project_or_issue: &str,
        query: &str,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let seq = self
            .assignable_users_req_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let site = self.issue_write_site(site_id);
        let (resolved_site, client) = self.site_id_and_client_for(site.as_deref());
        let tx = self.ui_tx.clone();
        let key = project_or_issue.to_string();
        let q = query.to_string();
        tokio::spawn(async move {
            let res = client
                .search_assignable_users(&key, &q)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AssignableUsersLoaded {
                seq,
                site_id: resolved_site,
                query: q,
                users: res,
            });
        });
    }

    pub fn request_assign_user(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        key: &str,
        user: Option<UserSummary>,
    ) {
        let site = self.issue_write_site(site_id);
        let prev_assignee = self.current_issue_assignee(site.as_deref(), key);
        let label = user
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| "Unassigned".to_string());
        self.apply_local_assignee_update(site.as_deref(), key, user.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            ui.success(format!("{key} assignee → {label}"));
            return;
        }
        let task_label = format!("Assign {key}");
        ui.tasks.begin(&task_label);
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let key_owned = key.to_string();
        tokio::spawn(async move {
            let id_ref = user.as_ref().map(|u| u.id.as_str());
            match client.assign_issue(&key_owned, id_ref).await {
                Ok(()) => {
                    let _ = storage.update_issue_assignee_for_site(
                        &site_id,
                        &key_owned,
                        user.as_ref(),
                    );
                    match client.get_issue_with_meta(&key_owned).await {
                        Ok((mut detail, meta)) => {
                            let _ = storage.upsert_issue_detail_for_site_merging(
                                &site_id,
                                &mut detail,
                                !meta.remote_links_ok,
                            );
                            let _ = tx.send(UiMsg::IssueMutated {
                                site_id,
                                detail,
                                toast: format!("{key_owned} assignee → {label}"),
                                clear_draft_key: None,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(UiMsg::IssueAssignFailed {
                                site_id,
                                key: key_owned,
                                previous_assignee: None,
                                task_label,
                                error: format!("Updated assignee, but failed to reload detail: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::IssueAssignFailed {
                        site_id,
                        key: key_owned,
                        previous_assignee: prev_assignee,
                        task_label,
                        error: format!("Failed to update assignee: {e}"),
                    });
                }
            }
        });
    }

    pub fn request_submit_issue_form(
        &mut self,
        ui: &mut UiState,
        form: &IssueFormOverlay,
        draft_key: Option<String>,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            return;
        }
        let site = self.issue_write_site(form.site_id.as_deref());
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(key) = form.edit_key.clone() {
            let patch = form.build_edit_patch();
            if patch.summary.is_none()
                && patch.description_markdown.is_none()
                && patch.priority.is_none()
                && patch.assignee_id.is_none()
                && patch.labels.is_none()
            {
                if let Some(dk) = draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                ui.info(format!("{key}: no fields changed"));
                return;
            }
            ui.tasks.begin(format!("Update {key}"));
            tokio::spawn(async move {
                match client.update_issue(&key, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_issue_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            site_id,
                            detail,
                            toast: format!("Updated {key}"),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed {
                            task_label: format!("Update {key}"),
                            error: format!("Failed to update issue (draft preserved): {e}"),
                        });
                    }
                }
            });
        } else {
            ui.tasks.begin("Create issue");
            let req = OutgoingIssue {
                project_key: form.project.text.trim().to_ascii_uppercase(),
                issue_type: form.issue_type.text.trim().to_string(),
                summary: form.summary.text.trim().to_string(),
                description_markdown: form.description.text.clone(),
                priority: (!form.priority.text.trim().is_empty())
                    .then(|| form.priority.text.trim().to_string()),
                assignee_id: None,
                labels: form.parsed_labels(),
                parent_key: None,
            };
            tokio::spawn(async move {
                match client.create_issue(&req).await {
                    Ok(detail) => {
                        let k = detail.summary.key.clone();
                        let _ = storage.upsert_issue_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::IssueMutated {
                            site_id,
                            detail,
                            toast: format!("Created {k}"),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::IssueActionFailed {
                            task_label: "Create issue".to_string(),
                            error: format!("Failed to create issue (draft preserved): {e}"),
                        });
                    }
                }
            });
        }
    }

    pub fn request_submit_comment(
        &mut self,
        ui: &mut UiState,
        overlay: &CommentOverlay,
        draft_key: Option<String>,
    ) {
        self.request_add_comment_with_draft(
            ui,
            overlay.site_id.clone(),
            overlay.target.clone(),
            overlay.body.text.clone(),
            draft_key,
        );
    }

    pub fn request_add_comment(
        &mut self,
        ui: &mut UiState,
        target: CommentTarget,
        body: String,
    ) {
        self.request_add_comment_with_draft(ui, None, target, body, None);
    }

    pub(super) fn request_add_comment_with_draft(
        &mut self,
        ui: &mut UiState,
        site: Option<String>,
        target: CommentTarget,
        body: String,
        draft_key: Option<String>,
    ) {
        let site = match &target {
            CommentTarget::Issue(_) => self.issue_write_site(site.as_deref()),
            CommentTarget::Page { .. } => site.or_else(|| self.active_page_site_id.clone()),
        };
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let optimistic_id = format!("optimistic:{}", crate::new_uuid_v4());
        let now = chrono::Utc::now().to_rfc3339();
        let author = self
            .known_me_for_site(&site_id, &client)
            .unwrap_or_else(|| UserSummary {
                id: "me".to_string(),
                display_name: "You".to_string(),
                ..Default::default()
            });
        let blocks = adf::parse_markdown(&body);

        match &target {
            CommentTarget::Issue(key) => {
                let opened_matches =
                    site_matches(self.active_issue_site_id.as_deref(), Some(&site_id));
                if let Some(detail) = self.active_issue.as_mut().filter(|_| opened_matches) {
                    if detail.summary.key == *key {
                        detail.comments.push(IssueComment {
                            id: optimistic_id.clone(),
                            issue_key: key.clone(),
                            author: author.clone(),
                            body_raw: body.clone(),
                            body_markdown: body.clone(),
                            blocks: blocks.clone(),
                            created_at: now.clone(),
                            updated_at: now.clone(),
                        });
                    }
                }
                self.bump_local_comment_count(Some(&site_id), key, 1);
            }
            CommentTarget::Page { page_id, .. } => {
                let opened_matches =
                    site_matches(self.active_page_site_id.as_deref(), Some(&site_id));
                if let Some(page) = self.active_page.as_mut().filter(|_| opened_matches) {
                    if page.summary.id == *page_id {
                        page.comments.push(ConfluenceComment {
                            id: optimistic_id.clone(),
                            page_id: page_id.clone(),
                            author_name: author.display_name.clone(),
                            body_raw: body.clone(),
                            body_markdown: body.clone(),
                            blocks: blocks.clone(),
                            created_at: now.clone(),
                        });
                    }
                }
            }
        }

        if tokio::runtime::Handle::try_current().is_err() {
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            ui.success("Comment posted");
            return;
        }

        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            CommentTarget::Issue(key) => {
                let task_label = format!("Comment on {key}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = match client.add_issue_comment(&key, &body).await {
                        Ok(comment) => {
                            let _ = storage.append_issue_comment_for_site(&site_id, &key, &comment);
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentAdded {
                        site_id,
                        issue_key: Some(key),
                        page_id: None,
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        clear_draft_key: draft_key,
                        result: res,
                    });
                });
            }
            CommentTarget::Page { page_id, title } => {
                let task_label = format!("Comment on {title}");
                ui.tasks.begin(&task_label);
                tokio::spawn(async move {
                    let res = match client.add_page_comment(&page_id, &body).await {
                        Ok(comment) => {
                            let _ =
                                storage.append_page_comment_for_site(&site_id, &page_id, &comment);
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentAdded {
                        site_id,
                        issue_key: None,
                        page_id: Some(page_id),
                        optimistic_id: Some(optimistic_id),
                        task_label,
                        clear_draft_key: draft_key,
                        result: res,
                    });
                });
            }
        }
    }

}
