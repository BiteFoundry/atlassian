//! Asynchronous Confluence requests: spaces, page trees, page detail, attachments/comments refresh, forms.

use crate::{
    atlassian::client::is_not_found_error,
    models::{OpenItemTarget, OutgoingPage, PageSummary, PageUpdate},
    sync,
    ui::overlays::page_form::PageFormOverlay,
};

use super::{
    helpers::SiteKey,
    state::{UiMsg, UiState, WorkspaceTab},
    AppState, InFlightPageDetail,
};

impl AppState {
    pub fn request_space_root_pages(&mut self, ui: &mut UiState, space_key: &str) {
        self.request_space_root_pages_for_site(ui, None, space_key);
    }

    pub fn request_space_root_pages_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        space_key: &str,
    ) {
        let space_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_space(ui)
                    .filter(|s| s.key == space_key)
                    .and_then(|_| self.active_space_site_id(ui).map(str::to_string))
            })
            .or_else(|| {
                self.spaces
                    .iter()
                    .position(|s| s.key == space_key)
                    .and_then(|idx| self.space_site_ids.get(idx))
                    .cloned()
            });
        let (site_id, client) = self.site_id_and_client_for(space_site.as_deref());
        if let Ok(roots) = self
            .storage
            .list_space_root_pages_for_site(&site_id, space_key)
        {
            if !roots.is_empty() {
                self.root_pages_by_space
                    .insert(SiteKey::new(&site_id, space_key), roots);
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin(format!("Space {space_key}"));
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let sk = space_key.to_string();
        tokio::spawn(async move {
            match sync::load_space_root_pages_for_site(&client, &storage, &site_id, &sk).await {
                Ok(pages) => {
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        site_id,
                        space_key: sk,
                        pages: Ok(pages),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::RootPagesLoaded {
                        site_id,
                        space_key: sk,
                        pages: Err(e.to_string()),
                    });
                }
            }
        });
    }

    /// Fetches direct child pages for `(site_id, page_id)`. When `force` is `true`, bypasses
    /// the in-memory `page_children_map` cache so manual refresh / re-expand sees live changes.
    pub fn request_page_children_for_site(
        &mut self,
        ui: &mut UiState,
        site_id: &str,
        page_id: &str,
        title: &str,
        force: bool,
    ) {
        let (site_id, client) = self.site_id_and_client_for(Some(site_id));
        let node = SiteKey::new(&site_id, page_id);
        if !force && self.page_children_map.contains_key(&node) {
            return;
        }
        if !self.page_children_map.contains_key(&node) {
            if let Ok(cached) = self.storage.list_child_pages_for_site(&site_id, page_id) {
                if !cached.is_empty() {
                    self.page_children_map.insert(node, cached);
                }
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = if title.trim().is_empty() {
            format!("Child pages: #{page_id}")
        } else {
            format!("Child pages: {title}")
        };
        ui.tasks.begin(&task_label);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page_id.to_string();
        tokio::spawn(async move {
            match sync::load_child_pages_for_site(&client, &storage, &site_id, &pid).await {
                Ok(children) => {
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        site_id,
                        parent_id: pid,
                        task_label,
                        children: Ok(children),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ChildPagesLoaded {
                        site_id,
                        parent_id: pid,
                        task_label,
                        children: Err(e.to_string()),
                    });
                }
            }
        });
    }

    pub fn toggle_expand_page(&mut self, ui: &mut UiState, page: &PageSummary) {
        // Same site as `current_confluence_tree`, so the toggled row is the one drawn.
        let tree_site = self.tree_site(ui);
        let (site_id, _) = self.site_id_and_client_for(Some(&tree_site));
        let node = SiteKey::new(&site_id, &page.id);
        if self.expanded_pages.remove(&node) {
            self.user_collapsed_pages.insert(node);
            return;
        }
        self.user_collapsed_pages.remove(&node);
        self.expanded_pages.insert(node);
        // Re-expanding keeps cached children visible immediately while refreshing live children.
        self.request_page_children_for_site(ui, &site_id, &page.id, &page.title, true);
    }

    /// Expands the active space's homepage root (else its sole root page) when its roots arrive,
    /// and loads its first-level children, as Confluence web does. A page the user collapsed in
    /// this session and a confirmed leaf stay collapsed; other spaces are left alone.
    pub(super) fn auto_expand_space_home(&mut self, ui: &mut UiState, site_id: &str, space_key: &str) {
        let Some(space) = self.active_space(ui).filter(|sp| sp.key == space_key) else {
            return;
        };
        if self
            .active_space_site_id(ui)
            .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
        {
            return;
        }
        let homepage_id = space
            .homepage_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let Some(roots) = self.root_pages_by_space.get(&SiteKey::new(site_id, space_key)) else {
            return;
        };
        let target = homepage_id
            .as_deref()
            .and_then(|hid| roots.iter().find(|p| p.id == hid))
            .or(match roots.as_slice() {
                [only] => Some(only),
                _ => None,
            });
        let Some(page) = target.filter(|p| p.has_children != Some(false)) else {
            return;
        };
        let (page_id, title) = (page.id.clone(), page.title.clone());
        let node = SiteKey::new(site_id, &page_id);
        if self.user_collapsed_pages.contains(&node) {
            return;
        }
        self.expanded_pages.insert(node);
        self.request_page_children_for_site(ui, site_id, &page_id, &title, false);
    }

    /// Records a complete, empty child listing on the parent's tree rows so it renders as a leaf.
    pub(super) fn mark_page_childless(&mut self, site_id: &str, page_id: &str) {
        let rows = self
            .root_pages_by_space
            .iter_mut()
            .chain(self.page_children_map.iter_mut())
            .filter(|(k, _)| k.is_site(site_id))
            .flat_map(|(_, pages)| pages.iter_mut());
        for page in rows.filter(|p| p.id == page_id) {
            page.has_children = Some(false);
        }
    }

    /// Toggles expand/collapse on a Confluence tree row (e.g. chevron click) without forcibly
    /// switching `active_page`.
    pub fn toggle_page_expand_only(&mut self, ui: &mut UiState, row_idx: usize) {
        let tree = self.current_confluence_tree(ui);
        if let Some(row) = tree.get(row_idx) {
            let page = row.page.clone();
            ui.confluence_ui.selected_tree_row = row_idx;
            self.toggle_expand_page(ui, &page);
            self.clamp_selections(ui);
        }
    }

    /// Context-aware manual refresh for the currently active workspace tab (called alongside
    /// workspace sync on `r` / `Action::SyncAll`).
    pub fn refresh_active_context(&mut self, ui: &mut UiState) {
        match ui.active_tab {
            WorkspaceTab::Board => {
                if self.active_board_with_site(ui).is_some() {
                    let sprint_id = self
                        .active_board_sprint_id
                        .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
                    self.request_board_load(ui, ui.board_ui.selected_board, sprint_id);
                }
                if ui.board_ui.drawer_open {
                    if let Some((sid, key)) = self.current_open_issue_identity() {
                        self.request_issue_detail_for_site(ui, Some(&sid), &key);
                    }
                }
            }
            WorkspaceTab::Issues => {
                let jql = ui.issue_ui.jql.text.trim().to_string();
                let non_default_jql = !jql.is_empty() && jql != "ORDER BY updated DESC";
                if (self.issues_custom_query || non_default_jql) && !jql.is_empty() {
                    let label = self
                        .active_jql_label
                        .clone()
                        .unwrap_or_else(|| "Custom JQL".to_string());
                    let site = self.active_jql_site_id.clone();
                    self.request_jql_search_for_site(ui, site.as_deref(), jql, label);
                }
                if let Some((sid, key)) = self.current_open_issue_identity() {
                    self.request_issue_detail_for_site(ui, Some(&sid), &key);
                }
            }
            WorkspaceTab::MyWork => {
                self.request_my_work_refresh(ui);
            }
            WorkspaceTab::Confluence => {
                if let Some(sp) = self.active_space(ui).cloned() {
                    let space_site = self.tree_site(ui);
                    self.request_space_root_pages_for_site(ui, Some(&space_site), &sp.key);
                    let expanded_nodes: Vec<(String, String)> = self
                        .current_confluence_tree(ui)
                        .into_iter()
                        .filter(|row| row.expanded)
                        .map(|row| (row.page.id, row.page.title))
                        .collect();
                    for (pid, title) in expanded_nodes {
                        self.request_page_children_for_site(ui, &space_site, &pid, &title, true);
                    }
                }
                if let Some((sid, pid)) = self.current_open_page_identity() {
                    self.request_page_detail_for_site(ui, Some(&sid), &pid);
                }
            }
            WorkspaceTab::Search => {
                let q = ui.search_ui.query.text.clone();
                if ui.search_ui.is_remote {
                    self.request_remote_search(ui, &q);
                } else {
                    self.refresh_local_search(&q);
                    self.clamp_selections(ui);
                }
            }
            WorkspaceTab::Directory => {
                self.refresh_scoped_cache_counts();
            }
        }
    }

    pub fn request_page_detail(&mut self, ui: &mut UiState, page_id: &str) {
        self.request_page_detail_for_site(ui, None, page_id);
    }

    pub fn request_page_detail_for_site(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
    ) {
        self.request_page_detail_internal(ui, explicit_site, page_id, None, true);
    }

    pub fn request_page_detail_for_site_if_needed(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
        expected_version: Option<i32>,
    ) -> bool {
        self.request_page_detail_internal(ui, explicit_site, page_id, expected_version, false)
    }

    pub(super) fn request_page_detail_internal(
        &mut self,
        ui: &mut UiState,
        explicit_site: Option<&str>,
        page_id: &str,
        expected_version: Option<i32>,
        force_open: bool,
    ) -> bool {
        let resolved_site = explicit_site
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.active_page_target
                    .as_ref()
                    .filter(|(_, p)| p == page_id)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                self.active_page
                    .as_ref()
                    .filter(|p| p.summary.id == page_id)
                    .and_then(|_| self.active_page_site_id.clone())
            })
            .or_else(|| self.active_space_site_id(ui).map(str::to_string));
        let (site_id, client) = self.site_id_and_client_for(resolved_site.as_deref());

        if !force_open {
            if let Some(ref inflight) = self.in_flight_page_detail {
                if inflight.site_id.eq_ignore_ascii_case(&site_id) && inflight.page_id == page_id {
                    let already_covers = match (expected_version, inflight.target_version) {
                        (None, _) => true,
                        (Some(inc), Some(cur)) => cur >= inc,
                        (Some(_), None) => false,
                    };
                    if already_covers {
                        return false;
                    }
                }
            }
        }

        let same_open_page = self
            .active_page
            .as_ref()
            .is_some_and(|p| p.summary.id == page_id)
            && self
                .active_page_site_id
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(&site_id))
                .unwrap_or_else(|| {
                    self.effective_site_id_and_client()
                        .0
                        .eq_ignore_ascii_case(&site_id)
                });

        if !same_open_page {
            if let Ok(Some(cached)) = self.storage.get_page_detail_for_site(&site_id, page_id) {
                let detail_ver = self
                    .storage
                    .get_page_detail_version_for_site(&site_id, page_id)
                    .ok()
                    .flatten();
                self.active_page = Some(cached);
                self.active_page_detail_version = detail_ver;
            } else {
                self.active_page = None;
                self.active_page_detail_version = None;
            }
            ui.confluence_ui.reader_scroll = 0;
        }
        self.active_page_site_id = Some(site_id.clone());
        self.active_page_target = Some((site_id.clone(), page_id.to_string()));
        self.page_detail_req_seq = self.page_detail_req_seq.wrapping_add(1);
        let seq = self.page_detail_req_seq;
        let target_ver = expected_version.or_else(|| {
            self.storage
                .get_page_row_version_for_site(&site_id, page_id)
                .ok()
                .flatten()
        });
        self.in_flight_page_detail = Some(InFlightPageDetail {
            site_id: site_id.clone(),
            page_id: page_id.to_string(),
            target_version: target_ver,
            seq,
        });
        if tokio::runtime::Handle::try_current().is_err() {
            return true;
        }
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let pid = page_id.to_string();
        tokio::spawn(async move {
            match client
                .get_page_with_meta_in_cache(&pid, &site_id, &storage)
                .await
            {
                Ok((mut detail, meta)) => {
                    let _ =
                        storage.upsert_page_detail_for_site_with_meta(&site_id, &mut detail, meta);
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        page_id: pid,
                        detail: Ok(detail),
                        not_found: false,
                    });
                }
                Err(e) => {
                    let not_found = is_not_found_error(&e);
                    if not_found {
                        let _ = storage.delete_page_for_site(&site_id, &pid);
                    }
                    let _ = tx.send(UiMsg::PageDetailLoaded {
                        seq: Some(seq),
                        site_id,
                        page_id: pid,
                        detail: Err(e.to_string()),
                        not_found,
                    });
                }
            }
        });
        true
    }

    /// Targeted attachment refresh for the currently open page or issue (used when opening
    /// `AttachmentsOverlay`, pressing `r` inside it, or returning focus while it is visible).
    pub fn request_attachments_refresh(&self, ui: &UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            OpenItemTarget::ConfluencePage { site_id, page_id } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_page_attachments(&page_id).await {
                        Ok(atts) => {
                            let _ = storage.update_page_attachments_for_site(&sid, &page_id, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentsRefreshed {
                        site_id: sid,
                        issue_key: None,
                        page_id: Some(page_id),
                        attachments: res,
                    });
                });
            }
            OpenItemTarget::JiraIssue { site_id, key } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_issue_attachments(&key).await {
                        Ok(atts) => {
                            let _ = storage.update_issue_attachments_for_site(&sid, &key, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentsRefreshed {
                        site_id: sid,
                        issue_key: Some(key),
                        page_id: None,
                        attachments: res,
                    });
                });
            }
        }
    }

    /// Targeted comments refresh for the currently open page or issue (used when opening
    /// `CommentOverlay` or returning focus while it is visible, without touching unsent text).
    pub fn request_comments_refresh(&self, ui: &UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Some(target) = self.current_open_target(ui) else {
            return;
        };
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            OpenItemTarget::ConfluencePage { site_id, page_id } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let res = match client.list_page_comments(&page_id).await {
                        Ok(comments) => {
                            let _ = storage.update_page_comments_for_site(&sid, &page_id, &comments);
                            Ok(comments)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::CommentsRefreshed {
                        site_id: sid,
                        issue_key: None,
                        page_id: Some(page_id),
                        issue_comments: None,
                        issue_comment_total: None,
                        page_comments: Some(res),
                    });
                });
            }
            OpenItemTarget::JiraIssue { site_id, key } => {
                let (sid, client) = self.site_id_and_client_for(Some(&site_id));
                tokio::spawn(async move {
                    let (res, total) = match client.list_issue_comments_collected(&key).await {
                        Ok((collected, total)) => {
                            // The server total keeps comment_count right past the comment cap.
                            let _ = storage.update_issue_comments_for_site_with_total(
                                &sid,
                                &key,
                                &collected.items,
                                total,
                            );
                            (Ok(collected.items), total)
                        }
                        Err(e) => (Err(e.to_string()), None),
                    };
                    let _ = tx.send(UiMsg::CommentsRefreshed {
                        site_id: sid,
                        issue_key: Some(key),
                        page_id: None,
                        issue_comments: Some(res),
                        issue_comment_total: total,
                        page_comments: None,
                    });
                });
            }
        }
    }

    pub fn request_submit_page_form(
        &mut self,
        ui: &mut UiState,
        form: &PageFormOverlay,
        draft_key: Option<String>,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            if let Some(dk) = draft_key.as_deref() {
                self.clear_draft_and_save(dk);
            }
            return;
        }
        let site = form
            .site_id
            .clone()
            .or_else(|| self.active_page_site_id.clone());
        let (site_id, client) = self.site_id_and_client_for(site.as_deref());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        if let Some(pid) = form.edit_page_id.clone() {
            let body_unchanged = form.body.text == form.initial_body;
            if form.title.text.trim() == form.initial_title.trim() && body_unchanged {
                if let Some(key) = draft_key.as_deref() {
                    self.clear_draft_and_save(key);
                }
                return;
            }
            ui.tasks.begin("Update page");
            let patch = PageUpdate {
                title: form.title.text.trim().to_string(),
                body_markdown: form.body.text.clone(),
                body_raw_passthrough: body_unchanged
                    .then(|| form.initial_body_raw.clone())
                    .flatten(),
                body_representation: body_unchanged
                    .then(|| form.initial_body_representation.clone())
                    .flatten(),
                prev_version: form.prev_version,
                version_message: Some("Edited via atlassian-cli TUI".to_string()),
            };
            tokio::spawn(async move {
                match client.update_page(&pid, &patch).await {
                    Ok(detail) => {
                        let _ = storage.upsert_page_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            site_id,
                            detail,
                            toast: "Confluence page saved".to_string(),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed {
                            task_label: "Update page".to_string(),
                            error: format!("Failed to update page (draft preserved): {e}"),
                        });
                    }
                }
            });
        } else {
            ui.tasks.begin("Create page");
            let req = OutgoingPage {
                space_id_or_key: form.space_key.clone(),
                parent_id: form.parent_id.clone(),
                title: form.title.text.trim().to_string(),
                body_markdown: form.body.text.clone(),
            };
            tokio::spawn(async move {
                match client.create_page(&req).await {
                    Ok(detail) => {
                        let _ = storage.upsert_page_detail_for_site(&site_id, &detail);
                        let _ = tx.send(UiMsg::PageMutated {
                            site_id,
                            detail,
                            toast: "Created Confluence page".to_string(),
                            clear_draft_key: draft_key,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiMsg::PageActionFailed {
                            task_label: "Create page".to_string(),
                            error: format!("Failed to create page (draft preserved): {e}"),
                        });
                    }
                }
            });
        }
    }
}
