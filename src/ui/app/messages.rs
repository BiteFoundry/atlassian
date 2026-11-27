//! Asynchronous `UiMsg` event handler decomposed by domain (Jira, Confluence, Media, Workspace).

use std::{sync::atomic::Ordering, time::Instant};

use crate::{
    models::StatusCategory,
    storage::SiteRecord,
    ui::overlays::{html_preview::HtmlPreviewOverlay, Overlay},
};

use super::{
    helpers::{builtin_filters, keep_known_children_state, site_matches, SiteKey},
    state::{UiMsg, UiState, WorkspaceTab},
    AppState,
};

impl AppState {
    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BoardsLoaded(_)
            | UiMsg::BoardDataLoaded { .. }
            | UiMsg::BoardLoadFailed { .. }
            | UiMsg::IssuesLoaded { .. }
            | UiMsg::IssuesLoadFailed { .. }
            | UiMsg::SavedFiltersLoaded(_)
            | UiMsg::IssueDetailLoaded { .. }
            | UiMsg::IssueMutated { .. }
            | UiMsg::IssueTransitioned { .. }
            | UiMsg::IssueTransitionFailed { .. }
            | UiMsg::IssueAssignFailed { .. }
            | UiMsg::IssueActionFailed { .. }
            | UiMsg::TransitionsLoaded { .. }
            | UiMsg::AssignableUsersLoaded { .. }
            | UiMsg::CommentAdded { .. } => self.handle_jira_ui_msg(ui, msg),

            UiMsg::SpacesLoaded(_)
            | UiMsg::RootPagesLoaded { .. }
            | UiMsg::ChildPagesLoaded { .. }
            | UiMsg::PageDetailLoaded { .. }
            | UiMsg::PageMutated { .. }
            | UiMsg::PageActionFailed { .. } => self.handle_confluence_ui_msg(ui, msg),

            UiMsg::ImageLoaded { .. }
            | UiMsg::HtmlAttachmentLoaded { .. }
            | UiMsg::PreviewImageSniffedAsHtml { .. }
            | UiMsg::HtmlServed(_)
            | UiMsg::AttachmentDownloaded { .. }
            | UiMsg::AttachmentUploaded { .. } => self.handle_media_ui_msg(ui, msg),

            other => self.handle_workspace_ui_msg(ui, other),
        }
    }

    fn handle_jira_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BoardsLoaded(Ok(boards)) => {
                self.boards = boards;
                self.clamp_selections(ui);
            }
            UiMsg::BoardsLoaded(Err(e)) => ui.error(e),
            UiMsg::BoardDataLoaded {
                seq,
                site_id,
                board_id,
                requested_sprint_id,
                columns,
                sprints,
                issues,
            } => {
                ui.tasks.finish(&format!("Board #{board_id}"), None);
                if seq != self.board_data_req_seq || !self.site_scope.matches(&site_id) {
                    return;
                }
                let still_active = self.active_board(ui).is_some_and(|b| b.id == board_id)
                    && self
                        .active_board_site_id(ui)
                        .is_none_or(|s| s.eq_ignore_ascii_case(&site_id));
                if !still_active {
                    return;
                }
                if !columns.is_empty() {
                    if self.board_site_ids.len() == self.boards.len() {
                        if let Some((b, _)) = self
                            .boards
                            .iter_mut()
                            .zip(self.board_site_ids.iter())
                            .find(|(b, b_sid)| {
                                b.id == board_id && b_sid.eq_ignore_ascii_case(&site_id)
                            })
                        {
                            b.columns = columns;
                        }
                    } else if let Some(b) = self.boards.iter_mut().find(|b| b.id == board_id) {
                        b.columns = columns;
                    }
                }
                let effective_sprint = requested_sprint_id.or_else(|| {
                    sprints
                        .iter()
                        .find(|s| s.state.eq_ignore_ascii_case("active"))
                        .map(|s| s.id)
                });
                self.sprints = sprints;
                let prefetch_keys: Vec<String> = issues.iter().map(|i| i.key.clone()).collect();
                self.board_issues = issues;
                self.board_issue_site_id = Some(site_id.clone());
                self.active_board_sprint_id = effective_sprint;
                if let Some(sp_id) = effective_sprint {
                    if let Some(pos) = self.sprints.iter().position(|s| s.id == sp_id) {
                        ui.board_ui.selected_sprint = pos;
                    }
                }
                ui.board_ui.selected_col = 0;
                ui.board_ui.selected_row = 0;
                self.clamp_selections(ui);
                let active_key = self.active_issue.as_ref().map(|d| d.summary.key.clone());
                self.prefetch_uncached_issues(&site_id, &prefetch_keys, active_key.as_deref());
            }
            UiMsg::BoardLoadFailed {
                seq,
                site_id,
                board_id,
                error,
            } => {
                ui.tasks
                    .finish(&format!("Board #{board_id}"), Some(error.clone()));
                if (seq != 0 && seq != self.board_data_req_seq)
                    || !self.site_scope.matches(&site_id)
                {
                    return;
                }
                ui.error(format!("Failed to load board: {error}"));
            }
            UiMsg::IssuesLoaded {
                seq,
                site_id,
                issues,
                label,
            } => {
                ui.tasks.finish(&format!("JQL: {label}"), None);
                if seq != self.issues_req_seq || !self.site_scope.matches(&site_id) {
                    return;
                }
                self.issue_site_ids = vec![site_id.clone(); issues.len()];
                self.issues = issues;
                ui.issue_ui.selected_issue = 0;
                if let Some(first) = self.issues.first().cloned() {
                    self.request_issue_detail_for_site(ui, Some(&site_id), &first.key);
                    let sibling_keys: Vec<String> =
                        self.issues.iter().skip(1).map(|i| i.key.clone()).collect();
                    self.prefetch_uncached_issues(&site_id, &sibling_keys, Some(&first.key));
                } else {
                    self.active_issue = None;
                    self.active_issue_site_id = None;
                    self.active_issue_target = None;
                    self.active_issue_detail_updated_at = None;
                    self.in_flight_issue_detail = None;
                    self.sync_open_item_watch(ui);
                }
                self.clamp_selections(ui);
            }
            UiMsg::IssuesLoadFailed { seq, label, error } => {
                ui.tasks.finish(&format!("JQL: {label}"), Some(error.clone()));
                if seq != 0 && seq != self.issues_req_seq {
                    return;
                }
                ui.error(format!("JQL search failed: {error}"));
            }
            UiMsg::SavedFiltersLoaded(filters) => {
                let mut combined = builtin_filters();
                for f in filters {
                    if !combined.iter().any(|b| b.id == f.id) {
                        combined.push(f);
                    }
                }
                self.saved_filters = combined;
                self.clamp_selections(ui);
            }
            UiMsg::IssueDetailLoaded {
                seq,
                site_id,
                key,
                detail,
                not_found,
            } => {
                if let Some(req_seq) = seq {
                    if self
                        .in_flight_issue_detail
                        .as_ref()
                        .is_some_and(|f| f.seq == req_seq)
                    {
                        self.in_flight_issue_detail = None;
                    }
                    if req_seq != self.issue_detail_req_seq {
                        return;
                    }
                } else if self.active_issue.as_ref().is_some_and(|cur| {
                    cur.summary.key != key
                        || self
                            .active_issue_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(&site_id))
                }) {
                    return;
                }
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                if let Some((ref target_sid, ref target_key)) = self.active_issue_target {
                    if !target_sid.eq_ignore_ascii_case(&site_id) || target_key != &key {
                        return;
                    }
                }
                match detail {
                    Ok(d) => {
                        let detail_ts = d.summary.updated_at.clone();
                        self.apply_issue_summary_to_lists(&site_id, &d.summary);
                        for overlay in &mut ui.overlays {
                            if let Overlay::Attachments(ov) = overlay {
                                if ov.owner_label == d.summary.key {
                                    ov.items = d.attachments.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                        self.active_issue = Some(d);
                        self.active_issue_site_id = Some(site_id.clone());
                        self.active_issue_target = Some((site_id, key));
                        self.active_issue_detail_updated_at = Some(detail_ts);
                    }
                    Err(err) => {
                        if not_found {
                            self.purge_deleted_issue(ui, &site_id, &key, false);
                        }
                        ui.error(format!("Failed to load issue detail: {err}"));
                    }
                }
            }
            UiMsg::IssueMutated {
                site_id,
                detail,
                toast,
                clear_draft_key,
            } => {
                if let Some(dk) = clear_draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                let key = detail.summary.key.clone();
                let summary = detail.summary.summary.clone();
                let is_create = toast.starts_with("Created ")
                    || clear_draft_key
                        .as_deref()
                        .is_some_and(|dk| dk.contains("issue:new"));
                ui.tasks.finish(&format!("Assign {key} to me"), None);
                ui.tasks.finish(&format!("Assign {key}"), None);
                ui.tasks.finish(&format!("Update {key}"), None);
                ui.tasks.finish("Create issue", None);
                if self.site_scope.matches(&site_id) {
                    let mut inserted_at_zero = false;
                    if let Some(pos) = self.issues.iter().enumerate().position(|(idx, i)| {
                        i.key == key
                            && self
                                .issue_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        self.issues[pos] = detail.summary.clone();
                    } else if is_create || !self.issues_custom_query {
                        let had_issues = !self.issues.is_empty();
                        self.issues.insert(0, detail.summary.clone());
                        self.issue_site_ids.insert(0, site_id.clone());
                        inserted_at_zero = had_issues;
                    }

                    if self
                        .board_issue_site_id
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(true)
                    {
                        if let Some(it) = self.board_issues.iter_mut().find(|i| i.key == key) {
                            *it = detail.summary.clone();
                        } else if is_create {
                            let board_matches = self.active_board(ui).is_some_and(|b| {
                                b.project_key.as_deref().is_none_or(|pk| {
                                    pk.eq_ignore_ascii_case(&detail.summary.project_key)
                                })
                            });
                            let sprint_matches = match self.active_board_sprint_id {
                                None => true,
                                Some(sid) => detail.summary.sprint_id == Some(sid),
                            };
                            if board_matches && sprint_matches {
                                self.board_issues.insert(0, detail.summary.clone());
                            }
                        }
                    }

                    let (_, site_client) = self.site_id_and_client_for(Some(&site_id));
                    let known_me = self.known_me_for_site(&site_id, &site_client);
                    let is_open = !matches!(detail.summary.status.category, StatusCategory::Done);
                    let assigned_to_me = known_me.as_ref().map(|me| {
                        detail
                            .summary
                            .assignee
                            .as_ref()
                            .is_some_and(|u| u.id.eq_ignore_ascii_case(&me.id))
                    });
                    if let Some(pos) = self.my_issues.iter().enumerate().position(|(idx, it)| {
                        it.key == key
                            && self
                                .my_issue_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        if !is_open || assigned_to_me == Some(false) {
                            self.remove_my_issue(&site_id, &key);
                        } else {
                            self.my_issues[pos] = detail.summary.clone();
                        }
                    } else if is_open && assigned_to_me == Some(true) {
                        let show_pill = self.is_multi_site_all_mode();
                        let pill = show_pill.then(|| self.site_display_label(&site_id));
                        self.my_issues.insert(0, detail.summary.clone());
                        self.my_issue_site_ids.insert(0, site_id.clone());
                        self.my_issue_site_labels.insert(0, pill);
                    }

                    let should_update_active = is_create
                        || self.active_issue.is_none()
                        || self.active_issue.as_ref().is_some_and(|cur| {
                            cur.summary.key == key
                                && self
                                    .active_issue_site_id
                                    .as_deref()
                                    .map(|s| s.eq_ignore_ascii_case(&site_id))
                                    .unwrap_or(true)
                        });
                    if should_update_active {
                        let detail_ts = detail.summary.updated_at.clone();
                        self.active_issue = Some(detail);
                        self.active_issue_site_id = Some(site_id.clone());
                        self.active_issue_target = Some((site_id, key.clone()));
                        self.active_issue_detail_updated_at = Some(detail_ts);
                        if inserted_at_zero && is_create && ui.active_tab == WorkspaceTab::Issues {
                            ui.issue_ui.selected_issue = 0;
                        } else if inserted_at_zero {
                            ui.issue_ui.selected_issue =
                                ui.issue_ui.selected_issue.saturating_add(1);
                        }
                    } else if inserted_at_zero {
                        ui.issue_ui.selected_issue = ui.issue_ui.selected_issue.saturating_add(1);
                    }
                }
                self.refresh_scoped_cache_counts();
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &summary);
            }
            UiMsg::IssueTransitioned {
                site_id,
                key,
                to_status,
                toast,
            } => {
                ui.tasks.finish(&format!("Transition {key}"), None);
                self.apply_local_status_update(Some(&site_id), &key, &to_status);
                if matches!(to_status.category, StatusCategory::Done) {
                    self.remove_my_issue(&site_id, &key);
                }
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_issue(&key, &format!("Transitioned to {}", to_status.name));
            }
            UiMsg::IssueTransitionFailed {
                site_id,
                key,
                previous_status,
                error,
            } => {
                ui.tasks
                    .finish(&format!("Transition {key}"), Some(error.clone()));
                if let Some(prev) = previous_status {
                    self.apply_local_status_update(Some(&site_id), &key, &prev);
                }
                self.clamp_selections(ui);
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueAssignFailed {
                site_id,
                key,
                previous_assignee,
                task_label,
                error,
            } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                if let Some(prev) = previous_assignee {
                    self.apply_local_assignee_update(Some(&site_id), &key, prev);
                } else {
                    self.reload_my_issues_from_storage();
                }
                self.clamp_selections(ui);
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::IssueActionFailed { task_label, error } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                ui.error(&error);
                self.notify_desktop("Atlassian Jira Error", &error);
            }
            UiMsg::TransitionsLoaded {
                seq,
                site_id,
                issue_key,
                transitions,
            } => {
                if seq != self.transitions_req_seq.load(Ordering::Acquire) {
                    return;
                }
                if let Some(Overlay::Transition(ov)) = ui.overlays.last_mut() {
                    if ov.issue_key == issue_key
                        && site_matches(ov.site_id.as_deref(), Some(&site_id))
                    {
                        match transitions {
                            Ok(list) => ov.set_transitions(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::AssignableUsersLoaded {
                seq,
                site_id,
                query,
                users,
            } => {
                if seq != self.assignable_users_req_seq.load(Ordering::Acquire) {
                    return;
                }
                if let Some(Overlay::Assignee(ov)) = ui.overlays.last_mut() {
                    if ov.query.text == query
                        && site_matches(ov.site_id.as_deref(), Some(&site_id))
                    {
                        match users {
                            Ok(list) => ov.set_users(list),
                            Err(err) => ov.set_error(err),
                        }
                    }
                }
            }
            UiMsg::CommentAdded {
                site_id,
                issue_key,
                page_id,
                optimistic_id,
                task_label,
                clear_draft_key,
                result,
            } => match result {
                Ok(()) => {
                    if let Some(dk) = clear_draft_key.as_deref() {
                        self.clear_draft_and_save(dk);
                    }
                    ui.tasks.finish(&task_label, None);
                    ui.success("Comment posted");
                    if let Some(k) = issue_key {
                        self.notify_desktop_issue(&k, "Comment posted");
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == k
                        }) {
                            self.request_issue_detail_for_site(ui, Some(&site_id), &k);
                        }
                    }
                    if let Some(pid) = page_id {
                        let space = self
                            .active_page
                            .as_ref()
                            .filter(|p| p.summary.id == pid)
                            .map(|p| p.summary.space_key.clone())
                            .unwrap_or_default();
                        let title = self
                            .active_page
                            .as_ref()
                            .filter(|p| p.summary.id == pid)
                            .map(|p| p.summary.title.clone())
                            .unwrap_or_else(|| format!("Page #{pid}"));
                        self.notify_desktop_page(&space, &format!("Comment posted on {title}"));
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            self.request_page_detail_for_site(ui, Some(&site_id), &pid);
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to post comment (draft preserved): {e}");
                    ui.tasks.finish(&task_label, Some(msg.clone()));
                    if let Some(ref opt_id) = optimistic_id {
                        if let Some(ref k) = issue_key {
                            if let Some(detail) = &mut self.active_issue {
                                if detail.summary.key == *k {
                                    detail.comments.retain(|c| c.id != *opt_id);
                                }
                            }
                            self.bump_local_comment_count(Some(&site_id), k, -1);
                        }
                        if let Some(ref pid) = page_id {
                            if let Some(page) = &mut self.active_page {
                                if page.summary.id == *pid {
                                    page.comments.retain(|c| c.id != *opt_id);
                                }
                            }
                        }
                    }
                    ui.error(&msg);
                    self.notify_desktop("Atlassian Comment Error", &msg);
                }
            },
            _ => unreachable!("non-Jira UiMsg routed to handle_jira_ui_msg"),
        }
    }

    fn handle_confluence_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::SpacesLoaded(Ok(spaces)) => {
                self.spaces = spaces;
                self.clamp_selections(ui);
            }
            UiMsg::SpacesLoaded(Err(e)) => ui.error(e),
            UiMsg::RootPagesLoaded {
                site_id,
                space_key,
                pages,
            } => {
                ui.tasks
                    .finish(&format!("Space {space_key}"), pages.as_ref().err().cloned());
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                match pages {
                    Ok(list) => {
                        let matches_space = self
                            .active_space(ui)
                            .filter(|sp| sp.key == space_key)
                            .and_then(|_| self.active_space_site_id(ui))
                            .map(|s| s.eq_ignore_ascii_case(&site_id))
                            .unwrap_or(true);
                        if matches_space {
                            let prefetch_ids: Vec<String> =
                                list.iter().map(|p| p.id.clone()).collect();
                            self.root_pages_by_space
                                .insert(SiteKey::new(&site_id, &space_key), list);
                            self.auto_expand_space_home(ui, &site_id, &space_key);
                            self.clamp_selections(ui);
                            let active_pid = self.active_page.as_ref().map(|p| p.summary.id.clone());
                            self.prefetch_uncached_pages(
                                &site_id,
                                &prefetch_ids,
                                active_pid.as_deref(),
                            );
                        }
                    }
                    Err(e) => ui.error(format!("Failed to load root pages: {e}")),
                }
            }
            UiMsg::ChildPagesLoaded {
                site_id,
                parent_id,
                task_label,
                children,
            } => {
                // Finish before the scope check: a site switch while the
                // request was in flight must not leave the spinner running.
                ui.tasks.finish(&task_label, children.as_ref().err().cloned());
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                match children {
                    Ok(list) => {
                        let prefetch_ids: Vec<String> = list.iter().map(|p| p.id.clone()).collect();
                        if list.is_empty() {
                            self.mark_page_childless(&site_id, &parent_id);
                        }
                        self.page_children_map
                            .insert(SiteKey::new(&site_id, parent_id), list);
                        self.clamp_selections(ui);
                        let active_pid = self.active_page.as_ref().map(|p| p.summary.id.clone());
                        self.prefetch_uncached_pages(
                            &site_id,
                            &prefetch_ids,
                            active_pid.as_deref(),
                        );
                    }
                    Err(e) => ui.error(format!("Failed to load child pages: {e}")),
                }
            }
            UiMsg::PageDetailLoaded {
                seq,
                site_id,
                page_id,
                detail,
                not_found,
            } => {
                if let Some(req_seq) = seq {
                    if self
                        .in_flight_page_detail
                        .as_ref()
                        .is_some_and(|f| f.seq == req_seq)
                    {
                        self.in_flight_page_detail = None;
                    }
                    if req_seq != self.page_detail_req_seq {
                        return;
                    }
                } else if self.active_page.as_ref().is_some_and(|cur| {
                    cur.summary.id != page_id
                        || self
                            .active_page_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(&site_id))
                }) {
                    return;
                }
                if !self.site_scope.matches(&site_id) {
                    return;
                }
                if let Some((ref target_sid, ref target_pid)) = self.active_page_target {
                    if !target_sid.eq_ignore_ascii_case(&site_id) || target_pid != &page_id {
                        return;
                    }
                }
                match detail {
                    Ok(d) => {
                        let detail_ver = d.summary.version_number;
                        // Keep tree rows updated for title/version changes when matching space/parent.
                        if let Some(roots) = self
                            .root_pages_by_space
                            .get_mut(&SiteKey::new(&site_id, &d.summary.space_key))
                        {
                            if let Some(existing) = roots.iter_mut().find(|p| p.id == d.summary.id) {
                                keep_known_children_state(existing, &d.summary);
                            }
                        }
                        if let Some(ref parent_id) = d.summary.parent_id {
                            if let Some(kids) =
                                self.page_children_map.get_mut(&SiteKey::new(&site_id, parent_id))
                            {
                                if let Some(existing) = kids.iter_mut().find(|p| p.id == d.summary.id) {
                                    keep_known_children_state(existing, &d.summary);
                                }
                            }
                        }
                        for overlay in &mut ui.overlays {
                            if let Overlay::Attachments(ov) = overlay {
                                if ov.owner_label == d.summary.title
                                    || self
                                        .active_page
                                        .as_ref()
                                        .is_some_and(|prev| ov.owner_label == prev.summary.title)
                                {
                                    ov.owner_label = d.summary.title.clone();
                                    ov.items = d.attachments.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                        self.active_page = Some(d);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id, page_id));
                        self.active_page_detail_version = Some(detail_ver);
                    }
                    Err(e) => {
                        if not_found {
                            self.purge_deleted_page(ui, &site_id, &page_id, false);
                        }
                        ui.error(format!("Failed to load page: {e}"));
                    }
                }
            }
            UiMsg::PageMutated {
                site_id,
                detail,
                toast,
                clear_draft_key,
            } => {
                if let Some(dk) = clear_draft_key.as_deref() {
                    self.clear_draft_and_save(dk);
                }
                ui.tasks.finish("Update page", None);
                ui.tasks.finish("Create page", None);
                let sk = detail.summary.space_key.clone();
                let title = detail.summary.title.clone();
                let pid_owned = detail.summary.id.clone();
                let mut parent_to_fetch: Option<(String, String)> = None;
                if self.site_scope.matches(&site_id) {
                    if let Some(pid) = &detail.summary.parent_id {
                        let parent = SiteKey::new(&site_id, pid);
                        if let Some(kids) = self.page_children_map.get_mut(&parent) {
                            if let Some(existing) = kids.iter_mut().find(|p| p.id == detail.summary.id) {
                                let prev_has_children = existing.has_children;
                                *existing = detail.summary.clone();
                                if existing.has_children.is_none() {
                                    existing.has_children = prev_has_children;
                                }
                            } else {
                                kids.push(detail.summary.clone());
                            }
                            self.expanded_pages.insert(parent);
                        } else {
                            // Do NOT insert a 1-element Vec when parent wasn't loaded yet, or
                            // pre-existing siblings would be hidden; mark parent & fetch live children.
                            let mut parent_title = String::new();
                            for (k, roots) in self.root_pages_by_space.iter_mut() {
                                if k.is_site(&site_id) {
                                    if let Some(p) = roots.iter_mut().find(|p| p.id == *pid) {
                                        p.has_children = Some(true);
                                        parent_title = p.title.clone();
                                    }
                                }
                            }
                            for (k, kids) in self.page_children_map.iter_mut() {
                                if k.is_site(&site_id) {
                                    if let Some(p) = kids.iter_mut().find(|p| p.id == *pid) {
                                        p.has_children = Some(true);
                                        if parent_title.is_empty() {
                                            parent_title = p.title.clone();
                                        }
                                    }
                                }
                            }
                            self.expanded_pages.insert(parent);
                            parent_to_fetch = Some((pid.clone(), parent_title));
                        }
                    } else {
                        let roots = self
                            .root_pages_by_space
                            .entry(SiteKey::new(&site_id, &sk))
                            .or_default();
                        if let Some(existing) = roots.iter_mut().find(|p| p.id == detail.summary.id) {
                            let prev_has_children = existing.has_children;
                            *existing = detail.summary.clone();
                            if existing.has_children.is_none() {
                                existing.has_children = prev_has_children;
                            }
                        } else {
                            roots.push(detail.summary.clone());
                        }
                    }

                    // Keep My Work recent pages updated.
                    if let Some(pos) = self.my_pages.iter().enumerate().position(|(idx, p)| {
                        p.id == pid_owned
                            && self
                                .my_page_site_ids
                                .get(idx)
                                .map(|s| s.eq_ignore_ascii_case(&site_id))
                                .unwrap_or(true)
                    }) {
                        self.my_pages[pos] = detail.summary.clone();
                    } else {
                        let show_pill = self.is_multi_site_all_mode();
                        let pill = show_pill.then(|| self.site_display_label(&site_id));
                        self.my_pages.insert(0, detail.summary.clone());
                        self.my_page_site_ids.insert(0, site_id.clone());
                        self.my_page_site_labels.insert(0, pill);
                        if self.my_pages.len() > 30 {
                            self.my_pages.truncate(30);
                            self.my_page_site_ids.truncate(30);
                            self.my_page_site_labels.truncate(30);
                        }
                    }

                    let is_create = toast.starts_with("Created ")
                        || clear_draft_key
                            .as_deref()
                            .is_some_and(|dk| dk.contains("page:new"));
                    let active_matches_site = self
                        .active_page_site_id
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(true);
                    if is_create || active_matches_site {
                        let detail_ver = detail.summary.version_number;
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id.clone(), pid_owned));
                        self.active_page_detail_version = Some(detail_ver);
                    }
                }
                if let Some((parent_id, parent_title)) = parent_to_fetch {
                    self.request_page_children_for_site(ui, &site_id, &parent_id, &parent_title, true);
                }
                self.refresh_scoped_cache_counts();
                self.clamp_selections(ui);
                ui.success(&toast);
                self.notify_desktop_page(&sk, &title);
            }
            UiMsg::PageActionFailed { task_label, error } => {
                ui.tasks.finish(&task_label, Some(error.clone()));
                ui.error(&error);
                self.notify_desktop("Atlassian Confluence Error", &error);
            }
            _ => unreachable!("non-Confluence UiMsg routed to handle_confluence_ui_msg"),
        }
    }

    fn handle_media_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::ImageLoaded {
                cache_key,
                rendered,
            } => match rendered {
                Ok(img) => {
                    let visible_key = match ui.overlays.last() {
                        Some(Overlay::ImagePreview(ov)) if !ov.protocol.is_pixel() => {
                            let area = ratatui::layout::Rect::new(0, 0, ui.term_width, ui.term_height);
                            ov.lookup_cached(area, self).map(|(key, _)| key)
                        }
                        _ => None,
                    };
                    let evicted = self.image_cache.insert(cache_key, img);
                    // A fetch started before navigation can complete after the visible image.
                    if visible_key.is_some_and(|key| evicted.contains(&key)) {
                        self.request_load_preview_image(ui);
                    }
                }
                Err(err) => {
                    if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() {
                        ov.error = Some(err);
                    }
                }
            },
            UiMsg::HtmlAttachmentLoaded {
                title,
                source_url,
                task_label,
                content_kind,
                result,
            } => {
                ui.tasks.finish(&task_label, result.as_ref().err().cloned());
                let effective_kind = if content_kind == bitefoundry_html::ContentKind::Markdown {
                    bitefoundry_html::ContentKind::Markdown
                } else if let Ok(ref raw) = result {
                    let sniffed = bitefoundry_html::sniff_content_kind(
                        None,
                        Some(title.as_str()),
                        Some(raw.as_bytes()),
                    );
                    if sniffed == bitefoundry_html::ContentKind::Markdown {
                        bitefoundry_html::ContentKind::Markdown
                    } else {
                        content_kind
                    }
                } else {
                    content_kind
                };
                if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                    if !source_url.is_empty() {
                        ov.source_url = source_url;
                    }
                    ov.set_loaded_with_kind(effective_kind, result);
                } else if let Ok(body) = result {
                    let mut ov = if effective_kind == bitefoundry_html::ContentKind::Markdown {
                        HtmlPreviewOverlay::new_markdown(title, body)
                    } else {
                        HtmlPreviewOverlay::new(title, body)
                    };
                    ov.source_url = source_url;
                    ui.push_overlay(Overlay::HtmlPreview(Box::new(ov)));
                } else if let Err(err) = result {
                    ui.error(format!("Failed to load attachment preview: {err}"));
                }
            }
            UiMsg::PreviewImageSniffedAsHtml {
                title,
                source_url,
                html,
            } => {
                if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                    ui.pop_overlay();
                }
                let mut ov = HtmlPreviewOverlay::new(title, html);
                ov.source_url = source_url;
                ui.push_overlay(Overlay::HtmlPreview(Box::new(ov)));
                ui.info("Detected HTML content — opened in interactive HTML Preview");
            }
            UiMsg::HtmlServed(res) => match res {
                Ok(url) => {
                    self.last_served_html_url = Some(url.clone());
                    if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                        ov.last_served_url = Some(url.clone());
                        if let Some(sess) = ov.session_mut() {
                            sess.last_status = Some(format!("Served at {url}"));
                        }
                    }
                    if !cfg!(test) {
                        if let Err(err) = open::that(&url) {
                            ui.error(format!("Served at {url} (browser open failed: {err})"));
                            return;
                        }
                    }
                    ui.success(format!("Opened HTML in browser: {url}"));
                }
                Err(err) => {
                    ui.error(format!("Failed to serve HTML locally: {err}"));
                }
            },
            UiMsg::AttachmentDownloaded { task_label, result } => match result {
                Ok(file) => {
                    ui.tasks.finish(&task_label, None);
                    ui.success(format!("Saved: {}", file.path.display()));
                    self.notify_desktop("Atlassian Download", &format!("Saved {}", file.filename));
                }
                Err(e) => {
                    ui.tasks.finish(&task_label, Some(e.clone()));
                    ui.error(format!("Download failed: {e}"));
                    self.notify_desktop("Atlassian Download Failed", &e);
                }
            },
            UiMsg::AttachmentUploaded {
                site_id,
                issue_key,
                page_id,
                result,
            } => match result {
                Ok(uploaded) => {
                    ui.tasks.finish("Upload attachment", None);
                    ui.success("Attachment uploaded");
                    self.notify_desktop("Atlassian Upload", "Attachment uploaded");
                    if let Some(k) = issue_key {
                        let added_len = uploaded.len();
                        let mut reconciled_total: Option<usize> = None;
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == k
                        }) {
                            if let Some(ref mut detail) = self.active_issue {
                                for att in &uploaded {
                                    let exists = !att.id.is_empty()
                                        && detail.attachments.iter().any(|e| e.id == att.id);
                                    if !exists {
                                        detail.attachments.push(att.clone());
                                    }
                                }
                                detail.summary.attachment_count = detail.attachments.len();
                                reconciled_total = Some(detail.attachments.len());
                                for overlay in &mut ui.overlays {
                                    if let Overlay::Attachments(ov) = overlay {
                                        if ov.owner_label == detail.summary.key {
                                            ov.items = detail.attachments.clone();
                                        }
                                    }
                                }
                            }
                            self.request_issue_detail_for_site(ui, Some(&site_id), &k);
                        }
                        self.for_each_local_issue_mut(Some(&site_id), &k, |it| {
                            if let Some(tot) = reconciled_total {
                                it.attachment_count = tot;
                            } else {
                                it.attachment_count = it.attachment_count.saturating_add(added_len);
                            }
                        });
                    }
                    if let Some(pid) = page_id {
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            if let Some(ref mut page) = self.active_page {
                                for att in &uploaded {
                                    let exists = !att.id.is_empty()
                                        && page.attachments.iter().any(|e| e.id == att.id);
                                    if !exists {
                                        page.attachments.push(att.clone());
                                    }
                                }
                                for overlay in &mut ui.overlays {
                                    if let Overlay::Attachments(ov) = overlay {
                                        if ov.owner_label == page.summary.title {
                                            ov.items = page.attachments.clone();
                                        }
                                    }
                                }
                            }
                            self.request_page_detail_for_site(ui, Some(&site_id), &pid);
                        }
                    }
                }
                Err(e) => {
                    ui.tasks.finish("Upload attachment", Some(e.clone()));
                    ui.error(format!("Upload failed: {e}"));
                    self.notify_desktop("Atlassian Upload Failed", &e);
                }
            },
            _ => unreachable!("non-Media UiMsg routed to handle_media_ui_msg"),
        }
    }

    fn handle_workspace_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::MeLoaded(Ok(me)) => {
                self.me = Some(me);
            }
            UiMsg::MeLoaded(Err(err)) => {
                ui.error(format!("Failed to verify user profile: {err}"));
            }
            UiMsg::BootstrapFinished(res) => match res {
                Ok(summary) => {
                    self.sync_summary = summary.clone();
                    ui.tasks.finish("Initial sync", None);
                    self.reload_cached_preserving_ui(ui);
                    ui.success(&summary);
                    self.notify_desktop("Atlassian Sync Complete", &summary);
                }
                Err(err) => {
                    ui.tasks.finish("Initial sync", Some(err.clone()));
                    ui.error(format!("Initial sync failed: {err}"));
                    self.notify_desktop("Atlassian Sync Failed", &err);
                }
            },
            UiMsg::MyWorkLoaded {
                seq,
                issues,
                issue_site_ids,
                issue_site_labels,
                pages,
                page_site_ids,
                page_site_labels,
            } => {
                ui.tasks.finish("Loading My Work", None);
                if seq != self.my_work_req_seq {
                    return;
                }
                self.my_issues = issues;
                self.my_issue_site_ids = issue_site_ids;
                self.my_issue_site_labels = issue_site_labels;
                self.my_pages = pages;
                self.my_page_site_ids = page_site_ids;
                self.my_page_site_labels = page_site_labels;
                self.last_my_work_refresh_at = Some(Instant::now());
                self.clamp_selections(ui);
            }
            UiMsg::AttachmentsRefreshed {
                site_id,
                issue_key,
                page_id,
                attachments,
            } => {
                if let Ok(atts) = attachments {
                    if let Some(key) = issue_key {
                        let new_count = atts.len();
                        self.for_each_local_issue_mut(Some(&site_id), &key, |it| {
                            it.attachment_count = new_count;
                        });
                        if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_k == key
                        }) {
                            if let Some(ref mut detail) = self.active_issue {
                                detail.attachments = atts.clone();
                                detail.summary.attachment_count = new_count;
                            }
                            for overlay in &mut ui.overlays {
                                if let Overlay::Attachments(ov) = overlay {
                                    ov.items = atts.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                    } else if let Some(pid) = page_id {
                        if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                            sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                        }) {
                            if let Some(ref mut page) = self.active_page {
                                page.attachments = atts.clone();
                            }
                            for overlay in &mut ui.overlays {
                                if let Overlay::Attachments(ov) = overlay {
                                    ov.items = atts.clone();
                                    if !ov.items.is_empty() && ov.selected >= ov.items.len() {
                                        ov.selected = ov.items.len() - 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            UiMsg::CommentsRefreshed {
                site_id,
                issue_key,
                page_id,
                issue_comments,
                issue_comment_total,
                page_comments,
            } => {
                if let (Some(key), Some(Ok(comments))) = (issue_key, issue_comments) {
                    let count = issue_comment_total.unwrap_or(0).max(comments.len());
                    self.for_each_local_issue_mut(Some(&site_id), &key, |it| {
                        it.comment_count = count;
                    });
                    if self.current_open_issue_identity().is_some_and(|(sid, cur_k)| {
                        sid.eq_ignore_ascii_case(&site_id) && cur_k == key
                    }) {
                        if let Some(ref mut detail) = self.active_issue {
                            detail.comments = comments;
                            detail.summary.comment_count = count;
                        }
                    }
                } else if let (Some(pid), Some(Ok(comments))) = (page_id, page_comments) {
                    if self.current_open_page_identity().is_some_and(|(sid, cur_p)| {
                        sid.eq_ignore_ascii_case(&site_id) && cur_p == pid
                    }) {
                        if let Some(ref mut page) = self.active_page {
                            page.comments = comments;
                        }
                    }
                }
            }
            UiMsg::RemoteSearchLoaded { seq, query, hits } => {
                ui.tasks.finish(
                    &format!("Remote search: {query}"),
                    hits.as_ref().err().cloned(),
                );
                if seq != self.remote_search_req_seq {
                    return;
                }
                if ui.search_ui.query.text.trim() != query.trim() {
                    return;
                }
                match hits {
                    Ok((list, site_ids, labels)) => {
                        self.search_hits = list;
                        self.search_hit_site_ids = site_ids;
                        self.search_hit_site_labels = labels;
                        ui.search_ui.is_remote = true;
                        ui.search_ui.selected_hit = 0;
                        self.clamp_selections(ui);
                    }
                    Err(e) => ui.error(format!("Remote search failed: {e}")),
                }
            }
            UiMsg::CloudSitesDiscovered(res) => {
                ui.tasks.finish("Discover Cloud sites", None);
                match res {
                    Ok(profiles) => {
                        let count = profiles.len();
                        for profile in profiles {
                            let _ = self.storage.upsert_site(&SiteRecord {
                                site_id: profile.id.clone(),
                                name: profile.display_name().to_string(),
                                site_url: profile.site_url.clone(),
                                deployment: profile.deployment.as_str().to_string(),
                                enabled: profile.enabled,
                                last_synced_at: None,
                                last_error: None,
                            });
                            self.config.upsert_site(profile);
                        }
                        self.rebuild_site_pool();
                        let _ = self.save_config();
                        self.reload_cached_preserving_ui(ui);
                        ui.success(format!("Discovered {count} Cloud site(s)"));
                    }
                    Err(e) => {
                        ui.error(format!("Cloud site discovery failed: {e}"));
                    }
                }
            }
            UiMsg::MultiSiteSyncFinished(outcome) => {
                self.syncing = false;
                self.sync_summary = outcome.summary.clone();
                ui.tasks.finish("Sync all sites", None);
                self.reload_cached_preserving_ui(ui);
                self.maybe_refresh_open_items_after_sync(ui);
                ui.success(outcome.summary);
            }
            UiMsg::UpdateChecked(res) => {
                ui.tasks.finish("Check for updates", None);
                match res {
                    Ok(info) => {
                        if info.update_available {
                            let msg = format!(
                                "New version {} available (current {})",
                                info.latest_version, info.current_version
                            );
                            ui.info(&msg);
                            self.notify_desktop("Atlassian Update Available", &msg);
                        } else {
                            ui.success(format!("Already up to date ({})", info.current_version));
                        }
                        ui.directory_ui.last_update = Some(info);
                    }
                    Err(e) => {
                        ui.error(format!("Update check failed: {e}"));
                        self.notify_desktop("Atlassian Update Check Failed", &e);
                    }
                }
            }
            _ => unreachable!("already-routed UiMsg reached handle_workspace_ui_msg"),
        }
    }
}
