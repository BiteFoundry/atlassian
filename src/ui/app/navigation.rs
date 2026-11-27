//! Selection projections, Kanban/tree views, cross-app navigation history, and overlay builders.

use std::{collections::HashSet, time::Instant};

use crate::{
    adf,
    models::{
        AttachmentItem, BoardSummary, CrossRefTarget, IssueSummary, OpenItemTarget, SpaceSummary,
        UnifiedSearchHit,
    },
    storage::DEFAULT_SITE_ID,
    sync,
    ui::{
        filter::space_match_score,
        overlays::{
            attachments::AttachmentsOverlay, comment::CommentTarget,
            context_menu::ContextMenuOverlay, Overlay,
        },
    },
};

use super::{
    helpers::{build_kanban_columns_for_board, flatten_page_tree, scan_issue_keys, SiteKey},
    state::{
        space_group_key, KanbanColumn, PaneFocus, SpacePaneRow, TreeRow, UiState, WorkspaceTab,
    },
    AppState, NavEntry, FOCUS_CHECK_DEBOUNCE,
};

impl AppState {
    pub fn active_board(&self, ui: &UiState) -> Option<&BoardSummary> {
        self.boards.get(ui.board_ui.selected_board)
    }

    pub fn active_board_with_site(&self, ui: &UiState) -> Option<(String, BoardSummary)> {
        let board = self.active_board(ui)?.clone();
        let site_id = self
            .active_board_site_id(ui)
            .map(str::to_string)
            .unwrap_or_else(|| self.effective_site_id_and_client().0);
        Some((site_id, board))
    }

    pub fn active_board_site_id(&self, ui: &UiState) -> Option<&str> {
        self.board_site_ids
            .get(ui.board_ui.selected_board)
            .map(String::as_str)
            .or(self.board_issue_site_id.as_deref())
    }

    pub fn active_space_site_id(&self, ui: &UiState) -> Option<&str> {
        self.space_site_ids
            .get(ui.confluence_ui.selected_space)
            .map(String::as_str)
    }

    /// Identifies the single currently open Jira issue or Confluence page on screen,
    /// including its canonical `site_id`, shared by selection, focus return, and periodic checks.
    pub fn current_open_target(&self, ui: &UiState) -> Option<OpenItemTarget> {
        if let Some(top) = ui.overlays.last() {
            match top {
                Overlay::Comment(ov) => match &ov.target {
                    CommentTarget::Issue(key) => {
                        let sid = self
                            .active_issue_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid,
                            key: key.clone(),
                        });
                    }
                    CommentTarget::Page { page_id, .. } => {
                        let sid = self
                            .active_page_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid,
                            page_id: page_id.clone(),
                        });
                    }
                },
                Overlay::IssueForm(ov) => {
                    if let Some(ref key) = ov.edit_key {
                        let sid = self
                            .active_issue_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid,
                            key: key.clone(),
                        });
                    }
                }
                Overlay::PageForm(ov) => {
                    if let Some(ref page_id) = ov.edit_page_id {
                        let sid = self
                            .active_page_site_id
                            .clone()
                            .unwrap_or_else(|| self.effective_site_id_and_client().0);
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid,
                            page_id: page_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        match ui.active_tab {
            WorkspaceTab::Confluence => {
                if let Some((ref sid, ref pid)) = self.active_page_target {
                    return Some(OpenItemTarget::ConfluencePage {
                        site_id: sid.clone(),
                        page_id: pid.clone(),
                    });
                }
                self.active_page.as_ref().map(|p| {
                    let sid = self
                        .active_page_site_id
                        .clone()
                        .or_else(|| self.active_space_site_id(ui).map(str::to_string))
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    OpenItemTarget::ConfluencePage {
                        site_id: sid,
                        page_id: p.summary.id.clone(),
                    }
                })
            }
            WorkspaceTab::Issues => {
                if let Some((ref sid, ref key)) = self.active_issue_target {
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid.clone(),
                        key: key.clone(),
                    });
                }
                if let Some(ref d) = self.active_issue {
                    let sid = self
                        .active_issue_site_id
                        .clone()
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid,
                        key: d.summary.key.clone(),
                    });
                }
                None
            }
            WorkspaceTab::Board => {
                if !ui.board_ui.drawer_open
                    && self.active_issue.is_none()
                    && self.active_issue_target.is_none()
                {
                    return None;
                }
                if let Some((ref sid, ref key)) = self.active_issue_target {
                    return Some(OpenItemTarget::JiraIssue {
                        site_id: sid.clone(),
                        key: key.clone(),
                    });
                }
                self.active_issue.as_ref().map(|d| {
                    let sid = self
                        .active_issue_site_id
                        .clone()
                        .or_else(|| self.active_board_site_id(ui).map(str::to_string))
                        .unwrap_or_else(|| self.effective_site_id_and_client().0);
                    OpenItemTarget::JiraIssue {
                        site_id: sid,
                        key: d.summary.key.clone(),
                    }
                })
            }
            _ => {
                if matches!(ui.overlays.last(), Some(Overlay::Attachments(_))) {
                    if let Some((ref sid, ref pid)) = self.active_page_target {
                        return Some(OpenItemTarget::ConfluencePage {
                            site_id: sid.clone(),
                            page_id: pid.clone(),
                        });
                    }
                    if let Some((ref sid, ref key)) = self.active_issue_target {
                        return Some(OpenItemTarget::JiraIssue {
                            site_id: sid.clone(),
                            key: key.clone(),
                        });
                    }
                }
                None
            }
        }
    }

    /// Synchronizes the currently open object with the background sync engine's 5s watcher.
    pub fn sync_open_item_watch(&mut self, ui: &UiState) {
        let target = self.current_open_target(ui);
        if target != self.last_watched_open_item {
            self.last_watched_open_item = target.clone();
            let _ = self
                .sync_ctrl
                .request(sync::SyncCommand::WatchOpenItem { target });
        }
    }

    /// Triggered on terminal `FocusGained`: immediately checks the open screen object's revision
    /// (and refreshes comments/attachments if their dedicated panel is currently visible).
    pub fn handle_focus_gained(&mut self, ui: &mut UiState) {
        self.sync_open_item_watch(ui);
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let now = Instant::now();
        if let Some(prev) = self.last_focus_check_at {
            if now.saturating_duration_since(prev) < FOCUS_CHECK_DEBOUNCE
                && self.last_watched_open_item.as_ref() == Some(&target)
            {
                return;
            }
        }
        self.last_focus_check_at = Some(now);
        let _ = self
            .sync_ctrl
            .request(sync::SyncCommand::CheckOpenItemNow { target });

        match ui.overlays.last() {
            Some(Overlay::Attachments(_)) => self.request_attachments_refresh(ui),
            Some(Overlay::Comment(_)) => self.request_comments_refresh(ui),
            _ => {}
        }
    }

    pub(super) fn current_open_issue_identity(&self) -> Option<(String, String)> {
        if let Some((ref sid, ref key)) = self.active_issue_target {
            return Some((sid.clone(), key.clone()));
        }
        self.active_issue.as_ref().map(|d| {
            let sid = self
                .active_issue_site_id
                .clone()
                .unwrap_or_else(|| self.effective_site_id_and_client().0);
            (sid, d.summary.key.clone())
        })
    }

    pub(super) fn current_open_page_identity(&self) -> Option<(String, String)> {
        if let Some((ref sid, ref pid)) = self.active_page_target {
            return Some((sid.clone(), pid.clone()));
        }
        self.active_page.as_ref().map(|p| {
            let sid = self
                .active_page_site_id
                .clone()
                .unwrap_or_else(|| self.effective_site_id_and_client().0);
            (sid, p.summary.id.clone())
        })
    }

    pub fn build_kanban_columns(&self, ui: &UiState) -> Vec<KanbanColumn> {
        build_kanban_columns_for_board(self.active_board(ui), &self.board_issues)
    }

    pub fn selected_board_issue(&self, ui: &UiState) -> Option<IssueSummary> {
        let cols = self.build_kanban_columns(ui);
        cols.get(ui.board_ui.selected_col)
            .and_then(|c| c.issues.get(ui.board_ui.selected_row))
            .cloned()
    }

    pub fn filtered_issues(&self, ui: &UiState) -> Vec<IssueSummary> {
        self.filtered_issues_with_sites(ui)
            .into_iter()
            .map(|(_, issue)| issue)
            .collect()
    }

    pub fn filtered_issues_with_sites(&self, ui: &UiState) -> Vec<(String, IssueSummary)> {
        let q = ui.issue_ui.local_filter.text.trim().to_ascii_lowercase();
        let fallback_site = self
            .site_scope
            .site_id()
            .unwrap_or(DEFAULT_SITE_ID)
            .to_string();
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                q.is_empty()
                    || it.key.to_ascii_lowercase().contains(&q)
                    || it.summary.to_ascii_lowercase().contains(&q)
                    || it.status.name.to_ascii_lowercase().contains(&q)
                    || it
                        .assignee
                        .as_ref()
                        .is_some_and(|a| a.display_name.to_ascii_lowercase().contains(&q))
            })
            .map(|(idx, it)| {
                let sid = self
                    .issue_site_ids
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| fallback_site.clone());
                (sid, it.clone())
            })
            .collect()
    }

    pub fn active_space(&self, ui: &UiState) -> Option<&SpaceSummary> {
        self.spaces.get(ui.confluence_ui.selected_space)
    }

    pub fn current_confluence_tree(&self, ui: &UiState) -> Vec<TreeRow> {
        let Some(space) = self.active_space(ui) else {
            return Vec::new();
        };
        let site = self.tree_site(ui);
        let roots = self
            .root_pages_by_space
            .get(&SiteKey::new(&site, &space.key))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        flatten_page_tree(roots, &site, &self.page_children_map, &self.expanded_pages)
    }

    /// Site the SPACES tree currently shows: the selected space's site, else
    /// the effective site of the current scope.
    pub fn tree_site(&self, ui: &UiState) -> String {
        self.active_space_site_id(ui)
            .map(str::to_string)
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Index of `config.default_space_key` in `spaces` (case-insensitive; the entry on
    /// `config.resolved_site_profile()` wins when several sites have the key), else `0`.
    pub fn resolve_default_space_index(&self) -> usize {
        let Some(key) = self
            .config
            .default_space_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return 0;
        };
        self.default_index_on_site(self.spaces.len(), &self.space_site_ids, |i| {
            self.spaces[i].key.eq_ignore_ascii_case(key)
        })
        .unwrap_or(0)
    }

    /// Index of `config.default_board_id`, else of the first board of
    /// `config.default_project` (the order of the sync's default board), else `0`.
    pub fn resolve_default_board_index(&self) -> usize {
        let by_id = self.config.default_board_id.and_then(|bid| {
            self.default_index_on_site(self.boards.len(), &self.board_site_ids, |i| {
                self.boards[i].id == bid
            })
        });
        let by_project = || {
            let project = self
                .config
                .default_project
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())?;
            self.default_index_on_site(self.boards.len(), &self.board_site_ids, |i| {
                self.boards[i]
                    .project_key
                    .as_deref()
                    .is_some_and(|k| k.eq_ignore_ascii_case(project))
            })
        };
        by_id.or_else(by_project).unwrap_or(0)
    }

    /// First index in `0..len` matching `pred`, preferring one whose site in `site_ids`
    /// is `config.resolved_site_profile()` (keys and ids repeat across sites).
    fn default_index_on_site(
        &self,
        len: usize,
        site_ids: &[String],
        pred: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let preferred = self.config.resolved_site_profile().map(|s| s.id);
        let mut first = None;
        for i in (0..len).filter(|&i| pred(i)) {
            let on_preferred = preferred
                .as_deref()
                .is_some_and(|p| site_ids.get(i).is_some_and(|s| s.eq_ignore_ascii_case(p)));
            if on_preferred {
                return Some(i);
            }
            first.get_or_insert(i);
        }
        first
    }

    /// Points the board and SPACES selections at the configured defaults unless the
    /// user already picked one, so a background reload never yanks the cursor.
    pub fn apply_default_selection(&self, ui: &mut UiState) {
        if !ui.board_ui.user_moved_board {
            ui.board_ui.selected_board = self.resolve_default_board_index();
        }
        if !ui.confluence_ui.user_moved_space {
            ui.confluence_ui.selected_space = self.resolve_default_space_index();
        }
    }

    /// Indices into `spaces` matching the SPACES filter, best match first, then
    /// cached order (pinned first); every space in cached order without a filter.
    fn filtered_space_indices(&self, ui: &UiState) -> Vec<usize> {
        let query = ui.confluence_ui.space_filter.text.trim();
        if query.is_empty() {
            return (0..self.spaces.len()).collect();
        }
        let mut scored: Vec<(u32, usize)> = self
            .spaces
            .iter()
            .enumerate()
            .filter_map(|(i, sp)| space_match_score(query, sp).map(|score| (score, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// SPACES pane rows in display order: the filtered spaces, grouped under
    /// collapsible site headers in multi-site All Sites mode.
    pub fn space_pane_rows(&self, ui: &UiState) -> Vec<SpacePaneRow> {
        let matches = self.filtered_space_indices(ui);
        if !self.is_multi_site_all_mode() {
            return matches.into_iter().map(SpacePaneRow::Space).collect();
        }
        let mut site_order: Vec<String> = Vec::new();
        let enabled = self.config.enabled_sites().into_iter().map(|s| s.id);
        for sid in enabled.chain(self.space_site_ids.iter().cloned()) {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(&sid)) {
                site_order.push(sid);
            }
        }
        let mut rows = Vec::new();
        for site_id in site_order {
            let group: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|&i| {
                    self.space_site_ids
                        .get(i)
                        .is_some_and(|s| s.eq_ignore_ascii_case(&site_id))
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            let collapsed = ui
                .collapsed_site_groups
                .contains(&space_group_key(&site_id));
            rows.push(SpacePaneRow::SiteHeader {
                site_id,
                spaces: group.len(),
                collapsed,
            });
            if !collapsed {
                rows.extend(group.into_iter().map(SpacePaneRow::Space));
            }
        }
        rows
    }

    /// Indices into `spaces` the SPACES pane lists, in display order; navigation
    /// steps over these while `selected_space` stays an index into `spaces`.
    pub fn visible_space_indices(&self, ui: &UiState) -> Vec<usize> {
        self.space_pane_rows(ui)
            .into_iter()
            .filter_map(|row| match row {
                SpacePaneRow::Space(i) => Some(i),
                SpacePaneRow::SiteHeader { .. } => None,
            })
            .collect()
    }

    /// Selects the first listed space when the SPACES filter hides the selected one.
    pub fn keep_space_selection_visible(&self, ui: &mut UiState) {
        let visible = self.visible_space_indices(ui);
        if !visible.contains(&ui.confluence_ui.selected_space) {
            if let Some(&first) = visible.first() {
                ui.confluence_ui.selected_space = first;
            }
        }
    }

    /// Whether personal space `space_key` (`~<account id or username>`) belongs to the
    /// signed-in user on `site_id`; `None` without a cached identity for that site.
    pub fn personal_space_owner_is_me(&self, site_id: &str, space_key: &str) -> Option<bool> {
        let owner = space_key.trim().strip_prefix('~')?.trim();
        let (site, client) = self.site_id_and_client_for(Some(site_id));
        let me = self.known_me_for_site(&site, &client)?;
        let me_id = me.id.trim();
        Some(!me_id.is_empty() && owner.eq_ignore_ascii_case(me_id))
    }

    pub fn selected_issue_key(&self, ui: &UiState) -> Option<String> {
        self.selected_issue_ref(ui).map(|(_, key)| key)
    }

    /// `(site_id, key)` of the issue a write should target: the selected row
    /// together with the site that row was loaded from — never the site of
    /// whichever issue happens to be open in the detail pane (ATL-11).
    pub fn selected_issue_ref(&self, ui: &UiState) -> Option<(String, String)> {
        let fallback_site = || self.effective_site_id_and_client().0;
        let opened = || {
            self.active_issue.as_ref().map(|d| {
                (
                    self.active_issue_site_id
                        .clone()
                        .unwrap_or_else(fallback_site),
                    d.summary.key.clone(),
                )
            })
        };
        match ui.active_tab {
            WorkspaceTab::Board => self
                .selected_board_issue(ui)
                .map(|i| {
                    (
                        self.board_issue_site_id
                            .clone()
                            .unwrap_or_else(fallback_site),
                        i.key,
                    )
                })
                .or_else(opened),
            WorkspaceTab::Issues => self
                .filtered_issues_with_sites(ui)
                .into_iter()
                .nth(ui.issue_ui.selected_issue)
                .map(|(sid, i)| (sid, i.key))
                .or_else(opened),
            WorkspaceTab::MyWork => {
                let idx = ui.my_work_ui.selected_issue;
                self.my_issues.get(idx).map(|i| {
                    (
                        self.my_issue_site_ids
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(fallback_site),
                        i.key.clone(),
                    )
                })
            }
            _ => opened(),
        }
    }

    pub fn current_attachments(&self, ui: &UiState) -> (String, Vec<AttachmentItem>) {
        if ui.active_tab == WorkspaceTab::Confluence
            || (ui.active_tab == WorkspaceTab::MyWork && ui.my_work_ui.active_column == 1)
        {
            if let Some(p) = &self.active_page {
                return (p.summary.title.clone(), p.attachments.clone());
            }
        }
        if let Some(issue) = &self.active_issue {
            return (issue.summary.key.clone(), issue.attachments.clone());
        }
        if let Some(page) = &self.active_page {
            return (page.summary.title.clone(), page.attachments.clone());
        }
        (String::new(), Vec::new())
    }

    /// Site of the issue or page `current_cross_ref(tab)` names.
    pub fn current_item_site(&self, tab: WorkspaceTab) -> Option<String> {
        match tab {
            WorkspaceTab::Confluence => self.active_page_site_id.clone(),
            _ => self.active_issue_site_id.clone(),
        }
    }

    /// Site of the item `current_attachments(ui)` lists the attachments of.
    pub fn current_attachments_site(&self, ui: &UiState) -> Option<String> {
        if (ui.active_tab == WorkspaceTab::Confluence
            || (ui.active_tab == WorkspaceTab::MyWork && ui.my_work_ui.active_column == 1))
            && self.active_page.is_some()
        {
            return self.active_page_site_id.clone();
        }
        if self.active_issue.is_some() {
            return self.active_issue_site_id.clone();
        }
        self.active_page_site_id.clone()
    }

    pub fn current_cross_ref(&self, tab: WorkspaceTab) -> Option<CrossRefTarget> {
        match tab {
            WorkspaceTab::Confluence => self.active_page.as_ref().map(|p| CrossRefTarget::ConfluencePage {
                page_id: p.summary.id.clone(),
                title: Some(p.summary.title.clone()),
            }),
            _ => self.active_issue.as_ref().map(|i| CrossRefTarget::JiraIssue {
                key: i.summary.key.clone(),
                label: Some(i.summary.summary.clone()),
            }),
        }
    }

    pub fn collect_cross_links(&self, tab: WorkspaceTab) -> (String, Vec<CrossRefTarget>) {
        let mut out: Vec<CrossRefTarget> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        let push_unique = |out: &mut Vec<CrossRefTarget>, seen: &mut HashSet<String>, item: CrossRefTarget| {
            let key = match &item {
                CrossRefTarget::JiraIssue { key, .. } => format!("J:{}", key.to_ascii_uppercase()),
                CrossRefTarget::ConfluencePage { page_id, .. } => format!("C:{page_id}"),
            };
            if seen.insert(key) {
                out.push(item);
            }
        };

        if tab == WorkspaceTab::Confluence {
            let Some(page) = &self.active_page else {
                return ("Confluence".to_string(), out);
            };
            for k in &page.linked_issue_keys {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue {
                        key: k.clone(),
                        label: None,
                    },
                );
            }
            for r in adf::extract_cross_refs(&page.blocks, &page.body_markdown) {
                push_unique(&mut out, &mut seen, r);
            }
            for k in scan_issue_keys(&page.body_markdown) {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue { key: k, label: None },
                );
            }
            return (page.summary.title.clone(), out);
        }

        let Some(issue) = &self.active_issue else {
            return ("Jira".to_string(), out);
        };
        let self_key = issue.summary.key.to_ascii_uppercase();
        for lp in &issue.linked_pages {
            push_unique(
                &mut out,
                &mut seen,
                CrossRefTarget::ConfluencePage {
                    page_id: lp.page_id.clone(),
                    title: Some(lp.title.clone()),
                },
            );
        }
        for li in &issue.linked_issues {
            if li.key.to_ascii_uppercase() != self_key {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue {
                        key: li.key.clone(),
                        label: Some(li.summary.clone()),
                    },
                );
            }
        }
        for r in adf::extract_cross_refs(&issue.description_blocks, &issue.description_markdown) {
            if let CrossRefTarget::JiraIssue { ref key, .. } = r {
                if key.to_ascii_uppercase() == self_key {
                    continue;
                }
            }
            push_unique(&mut out, &mut seen, r);
        }
        for k in scan_issue_keys(&issue.description_markdown) {
            if k.to_ascii_uppercase() != self_key {
                push_unique(
                    &mut out,
                    &mut seen,
                    CrossRefTarget::JiraIssue { key: k, label: None },
                );
            }
        }
        for c in &issue.comments {
            for r in adf::extract_cross_refs(&c.blocks, &c.body_markdown) {
                if let CrossRefTarget::JiraIssue { ref key, .. } = r {
                    if key.to_ascii_uppercase() == self_key {
                        continue;
                    }
                }
                push_unique(&mut out, &mut seen, r);
            }
        }
        (issue.summary.key.clone(), out)
    }

    pub fn jump_to_cross_ref(&mut self, ui: &mut UiState, target: CrossRefTarget) {
        self.jump_to_cross_ref_for_site(ui, None, target);
    }

    pub fn jump_to_cross_ref_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: CrossRefTarget,
    ) {
        if let Some(current) = self.current_cross_ref(ui.active_tab) {
            if self.nav_history.last().map(|e| &e.target) != Some(&current) && current != target {
                self.nav_history.push(NavEntry {
                    site_id: self.current_item_site(ui.active_tab),
                    target: current,
                });
            }
        }
        self.open_cross_ref_without_push_for_site(ui, site_id, target);
    }

    pub fn pop_nav_history(&mut self, ui: &mut UiState) -> bool {
        let Some(prev) = self.nav_history.pop() else {
            return false;
        };
        self.open_cross_ref_without_push_for_site(ui, prev.site_id.as_deref(), prev.target);
        true
    }

    pub fn open_cross_ref_without_push(&mut self, ui: &mut UiState, target: CrossRefTarget) {
        self.open_cross_ref_without_push_for_site(ui, None, target);
    }

    pub fn open_cross_ref_without_push_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: CrossRefTarget,
    ) {
        match target {
            CrossRefTarget::JiraIssue { key, .. } => {
                ui.active_tab = WorkspaceTab::Issues;
                ui.focus = PaneFocus::Main;
                let find_in_filtered = |app: &Self, ui: &UiState| {
                    let filtered = app.filtered_issues_with_sites(ui);
                    if let Some(sid) = site_id {
                        filtered
                            .iter()
                            .position(|(s, i)| i.key == key && s.eq_ignore_ascii_case(sid))
                            .or_else(|| filtered.iter().position(|(_, i)| i.key == key))
                    } else {
                        filtered.iter().position(|(_, i)| i.key == key)
                    }
                };
                if let Some(pos) = find_in_filtered(self, ui) {
                    ui.issue_ui.selected_issue = pos;
                } else if self.issues.iter().any(|i| i.key == key)
                    && !ui.issue_ui.local_filter.text.trim().is_empty()
                {
                    ui.issue_ui.local_filter.clear();
                    ui.issue_ui.local_filter_active = false;
                    if let Some(pos) = find_in_filtered(self, ui) {
                        ui.issue_ui.selected_issue = pos;
                    }
                }
                self.request_issue_detail_for_site(ui, site_id, &key);
            }
            CrossRefTarget::ConfluencePage { page_id, .. } => {
                ui.active_tab = WorkspaceTab::Confluence;
                ui.focus = PaneFocus::Main;
                self.request_page_detail_for_site(ui, site_id, &page_id);
            }
        }
    }

    pub fn refresh_local_search(&mut self, query: &str) {
        if let Ok(scoped_hits) =
            self.storage
                .unified_search_scoped(query, self.site_scope.site_id(), 50)
        {
            let filtered: Vec<(String, UnifiedSearchHit)> = scoped_hits
                .into_iter()
                .filter(|(sid, _)| {
                    !self.site_scope.is_all() || self.site_is_enabled_in_config(sid)
                })
                .collect();
            let show_pills = self.site_scope.is_all()
                && (self.config.enabled_sites().len() > 1
                    || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID));
            self.search_hit_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
            self.search_hit_site_labels = filtered
                .iter()
                .map(|(s, _)| show_pills.then(|| self.site_display_label(s)))
                .collect();
            self.search_hits = filtered.into_iter().map(|(_, h)| h).collect();
        }
    }

    /// Builds the attachments modal for the currently selected issue or Confluence page.
    pub fn build_attachments_overlay(&self, ui: &UiState) -> AttachmentsOverlay {
        let (label, items) = self.current_attachments(ui);
        AttachmentsOverlay::new(label, items)
    }

    /// Builds a context menu inferred from the active workspace tab and current selection.
    pub fn build_context_menu(&self, anchor: (u16, u16), ui: &UiState) -> ContextMenuOverlay {
        match ui.active_tab {
            WorkspaceTab::MyWork => {
                if ui.my_work_ui.active_column == 1 {
                    if let Some(page) = self.my_pages.get(ui.my_work_ui.selected_page) {
                        return ContextMenuOverlay::for_confluence_page(
                            page.id.clone(),
                            page.title.clone(),
                            page.web_url.clone(),
                            anchor,
                        );
                    }
                    return ContextMenuOverlay::for_global(anchor);
                }
                if let Some(issue) = self.my_issues.get(ui.my_work_ui.selected_issue) {
                    let url = if !issue.web_url.is_empty() {
                        issue.web_url.clone()
                    } else {
                        let base = self.config.jira_base_url();
                        if base.is_empty() {
                            String::new()
                        } else {
                            format!("{base}/browse/{}", issue.key)
                        }
                    };
                    return ContextMenuOverlay::for_jira_issue(issue.key.clone(), url, anchor);
                }
                return ContextMenuOverlay::for_global(anchor);
            }
            WorkspaceTab::Confluence => {
                if ui.focus == PaneFocus::Nav {
                    if ui.confluence_ui.nav_section == 0 {
                        return ContextMenuOverlay::for_global(anchor);
                    }
                    let tree = self.current_confluence_tree(ui);
                    if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                        return ContextMenuOverlay::for_confluence_page(
                            row.page.id.clone(),
                            row.page.title.clone(),
                            row.page.web_url.clone(),
                            anchor,
                        );
                    }
                }
                if let Some(page) = &self.active_page {
                    return ContextMenuOverlay::for_confluence_page(
                        page.summary.id.clone(),
                        page.summary.title.clone(),
                        page.summary.web_url.clone(),
                        anchor,
                    );
                }
                return ContextMenuOverlay::for_global(anchor);
            }
            WorkspaceTab::Search => {
                if let Some(hit) = self.search_hits.get(ui.search_ui.selected_hit) {
                    return match hit.kind {
                        crate::models::SearchHitKind::JiraIssue => {
                            let url = if !hit.web_url.is_empty() {
                                hit.web_url.clone()
                            } else {
                                let base = self.config.jira_base_url();
                                if base.is_empty() {
                                    String::new()
                                } else {
                                    format!("{base}/browse/{}", hit.id_or_key)
                                }
                            };
                            ContextMenuOverlay::for_jira_issue(hit.id_or_key.clone(), url, anchor)
                        }
                        crate::models::SearchHitKind::ConfluencePage => {
                            ContextMenuOverlay::for_confluence_page(
                                hit.id_or_key.clone(),
                                hit.title.clone(),
                                hit.web_url.clone(),
                                anchor,
                            )
                        }
                    };
                }
                return ContextMenuOverlay::for_global(anchor);
            }
            WorkspaceTab::Directory => {
                return ContextMenuOverlay::for_global(anchor);
            }
            WorkspaceTab::Board => {
                if ui.focus == PaneFocus::Nav {
                    return ContextMenuOverlay::for_global(anchor);
                }
            }
            WorkspaceTab::Issues => {
                if ui.focus == PaneFocus::Nav && ui.issue_ui.nav_section == 0 {
                    return ContextMenuOverlay::for_global(anchor);
                }
            }
        }
        if let Some(key) = self.selected_issue_key(ui) {
            let url = self
                .active_issue
                .as_ref()
                .filter(|d| d.summary.key == key)
                .map(|d| d.summary.web_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| {
                    let base = self.config.jira_base_url();
                    if base.is_empty() {
                        String::new()
                    } else {
                        format!("{base}/browse/{key}")
                    }
                });
            return ContextMenuOverlay::for_jira_issue(key, url, anchor);
        }
        ContextMenuOverlay::for_global(anchor)
    }
}
