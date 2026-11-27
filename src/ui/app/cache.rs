//! Multi-site pool management, site switcher, drafts/config persistence, cache loading, and bootstrap.

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::Ordering,
};

use futures_util::{stream, StreamExt};

use crate::{
    atlassian::{
        collect::SYNC_BOARD_ISSUE_CAP as BOARD_ISSUE_VIEW_CAP, AtlassianClient, SiteClientPool,
    },
    auth::{
        fetch_accessible_resources, run_oauth_loopback_login, AuthManager, OAuthLoopbackOptions,
        StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{DeploymentKind, SiteProfile},
    img_preview,
    models::{
        BoardSummary, IssueSummary, PageSummary, ProjectSummary, SavedFilter, SiteScope,
        SpaceSummary,
    },
    storage::{SiteRecord, DEFAULT_SITE_ID},
    sync::{self, SiteSyncTarget},
    ui::{
        directory_view::ConnectedSiteRow,
        notification,
        overlays::site_switcher::{SiteSwitcherCommand, SiteSwitcherEntry},
        theme::Theme,
    },
};

use super::{
    helpers::{builtin_filters, SiteKey},
    state::{config_protocol_to_img_pref, UiMsg, UiState},
    AppState,
};

impl AppState {
    pub fn effective_site_id_and_client(&self) -> (String, AtlassianClient) {
        if let Some(sid) = self.site_scope.site_id() {
            if sid.eq_ignore_ascii_case(self.client.site_id()) {
                return (self.client.site_id().to_string(), self.client.clone());
            }
            if let Ok(c) = self.site_pool.client_for(sid) {
                return (c.site_id().to_string(), c);
            }
        }
        (self.client.site_id().to_string(), self.client.clone())
    }

    pub(crate) fn site_id_and_client_for(
        &self,
        explicit_site: Option<&str>,
    ) -> (String, AtlassianClient) {
        if let Some(sid) = explicit_site.map(str::trim).filter(|s| !s.is_empty()) {
            if sid.eq_ignore_ascii_case(self.client.site_id()) {
                return (self.client.site_id().to_string(), self.client.clone());
            }
            if let Ok(c) = self.site_pool.client_for(sid) {
                return (c.site_id().to_string(), c);
            }
        }
        self.effective_site_id_and_client()
    }

    /// Site id carried by a list event; an empty one (legacy daemon) means the effective site.
    pub(super) fn event_site_or_effective(&self, site_id: String) -> String {
        if site_id.trim().is_empty() {
            self.effective_site_id_and_client().0
        } else {
            site_id
        }
    }

    pub(super) fn reload_projects_all_sites(&mut self) {
        if let Ok(projs) = self.storage.list_projects_all_sites() {
            let filtered: Vec<(String, ProjectSummary)> = projs
                .into_iter()
                .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                .collect();
            self.project_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
            self.projects = filtered.into_iter().map(|(_, p)| p).collect();
        }
    }

    /// Site of the Directory project row `row`, falling back to the effective site.
    pub fn project_row_site(&self, row: usize) -> String {
        self.project_site_ids
            .get(row)
            .cloned()
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Site of the space row `row` (index into `spaces`), falling back to the effective site.
    pub fn space_row_site(&self, row: usize) -> String {
        self.space_site_ids
            .get(row)
            .cloned()
            .unwrap_or_else(|| self.effective_site_id_and_client().0)
    }

    /// Removes `key` of `site_id` from My Work, keeping the parallel site vectors aligned.
    pub(crate) fn remove_my_issue(&mut self, site_id: &str, key: &str) {
        let mut idx = 0usize;
        while idx < self.my_issues.len() {
            let matches_site = self
                .my_issue_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.my_issues[idx].key == key {
                self.my_issues.remove(idx);
                if idx < self.my_issue_site_ids.len() {
                    self.my_issue_site_ids.remove(idx);
                }
                if idx < self.my_issue_site_labels.len() {
                    self.my_issue_site_labels.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
    }

    /// Removes page `page_id` of `site_id` from My Work, keeping the parallel site vectors aligned.
    pub(crate) fn remove_my_page(&mut self, site_id: &str, page_id: &str) {
        let mut idx = 0usize;
        while idx < self.my_pages.len() {
            let matches_site = self
                .my_page_site_ids
                .get(idx)
                .map(|s| s.eq_ignore_ascii_case(site_id))
                .unwrap_or(true);
            if matches_site && self.my_pages[idx].id == page_id {
                self.my_pages.remove(idx);
                if idx < self.my_page_site_ids.len() {
                    self.my_page_site_ids.remove(idx);
                }
                if idx < self.my_page_site_labels.len() {
                    self.my_page_site_labels.remove(idx);
                }
            } else {
                idx += 1;
            }
        }
    }

    /// Persistent draft key for `base` (an overlay's `draft_key()`). With more than one site
    /// configured the site is prefixed, so two sites' `ENG-1` keep separate drafts; a
    /// single-site config keeps the unprefixed key so existing drafts still restore.
    pub fn scoped_draft_key(&self, site_id: Option<&str>, base: String) -> String {
        match site_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(site) if self.config.sites.len() > 1 => {
                format!("site:{}:{base}", site.to_ascii_lowercase())
            }
            _ => base,
        }
    }

    pub(super) fn clear_draft_and_save(&mut self, key: &str) {
        if self.drafts.remove(key).is_some() {
            let _ = self.save_drafts();
        }
    }

    pub fn save_config(&self) -> anyhow::Result<()> {
        if let Some(ref path) = self.config_path_override {
            return self.config.save_to(path);
        }
        if cfg!(test) && std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.config.save()
    }

    pub fn save_drafts(&self) -> anyhow::Result<()> {
        if let Some(ref path) = self.drafts_path_override {
            return self.drafts.save_to(path);
        }
        if cfg!(test) && std::env::var_os("ATLASSIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.drafts.save()
    }

    pub fn sync_ui_from_config(&self, ui: &mut UiState) {
        ui.theme = Theme::detect(self.config.theme);
        ui.density = self.config.density;
        ui.show_nav_rail = self.config.show_nav_rail;
        ui.inline_images = self.config.inline_images;
        ui.notify = self.config.notify;
        let prev_cell_px = ui.graphics_caps.cell_px;
        let prev_cell_px_exact = ui.graphics_caps.cell_px_exact;
        ui.graphics_caps = img_preview::detect_graphics_caps(config_protocol_to_img_pref(
            self.config.image_protocol,
        ));
        if prev_cell_px_exact && !ui.graphics_caps.cell_px_exact {
            ui.graphics_caps.cell_px = prev_cell_px;
            ui.graphics_caps.cell_px_exact = true;
        }
    }

    pub(super) fn notify_desktop(&self, summary: &str, body: &str) {
        if self.config.notify && !cfg!(test) {
            notification::show(summary, &notification::truncate(body, notification::MAX_BODY_CHARS));
        }
    }

    pub(super) fn notify_desktop_issue(&self, issue_key: &str, summary: &str) {
        if !cfg!(test) {
            notification::notify_issue(self.config.notify, false, false, issue_key, summary);
        }
    }

    pub(super) fn notify_desktop_page(&self, space_key: &str, title: &str) {
        if !cfg!(test) {
            notification::notify_page(self.config.notify, false, false, space_key, title);
        }
    }

    pub fn image_cache_key(att_id: &str, cols: u16, rows: u16) -> String {
        format!("{att_id}:{cols}x{rows}")
    }

    pub fn site_display(&self) -> String {
        if let Some(label) = self.active_scope_label() {
            return label;
        }
        let enabled = self.config.enabled_sites();
        if enabled.len() > 1 {
            return format!("All Sites ({})", enabled.len());
        }
        if let Some(first) = enabled.first() {
            return first.display_name().to_string();
        }
        let raw = self.config.site_url.trim();
        if raw.is_empty() {
            "Not configured".to_string()
        } else {
            raw.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        }
    }

    /// Rebuilds the shared `SiteClientPool` from `self.config`, updates `self.client`
    /// when a specific site scope is active, and pushes updated polling targets to `sync_ctrl`.
    pub fn rebuild_site_pool(&mut self) {
        if let Ok(pool) = SiteClientPool::from_config(&self.config) {
            self.site_pool = pool;
        }
        if let SiteScope::Only(ref id) = self.site_scope {
            if let Ok(scoped_client) = self.site_pool.client_for(id) {
                self.client = scoped_client;
            }
        } else if let Some(def) = self.config.default_site_profile() {
            if let Ok(def_client) = self.site_pool.client_for(&def.id) {
                self.client = def_client;
            }
        }
        let targets = sync::sync_targets(&self.site_pool);
        let _ = self.sync_ctrl.update_targets(targets);
    }

    /// Resolves a human-friendly site label for `site_id` (site `name` or fallback `site_id`).
    pub fn site_display_label(&self, site_id: &str) -> String {
        if let Some(site) = self.config.find_effective_site(site_id) {
            return site.display_name().to_string();
        }
        site_id.to_string()
    }

    /// Returns `None` in `All Sites` federated mode, or `Some(display_name)` when scoped to 1 site.
    pub fn active_scope_label(&self) -> Option<String> {
        self.site_scope
            .site_id()
            .map(|id| self.site_display_label(id))
    }

    /// Label rendered inside the clickable top-header site switcher pill.
    pub fn header_site_pill_label(&self) -> String {
        match &self.site_scope {
            SiteScope::All => {
                let enabled = self.config.enabled_sites();
                if enabled.len() > 1 {
                    format!("All Sites ({})", enabled.len())
                } else if self.config.sites.is_empty() && !self.config.site_url.trim().is_empty() {
                    self.config
                        .site_url
                        .trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .to_string()
                } else {
                    "All Sites".to_string()
                }
            }
            SiteScope::Only(id) => self.site_display_label(id),
        }
    }

    /// True when in `All Sites` mode and multiple enabled sites exist (or multiple distinct sites are loaded).
    pub fn is_multi_site_all_mode(&self) -> bool {
        if !self.site_scope.is_all() {
            return false;
        }
        if self.config.enabled_sites().len() > 1 {
            return true;
        }
        let distinct_boards: HashSet<&str> = self.board_site_ids.iter().map(String::as_str).collect();
        let distinct_issues: HashSet<&str> = self.issue_site_ids.iter().map(String::as_str).collect();
        let distinct_spaces: HashSet<&str> = self.space_site_ids.iter().map(String::as_str).collect();
        distinct_boards.len() > 1 || distinct_issues.len() > 1 || distinct_spaces.len() > 1
    }

    pub(super) fn site_is_enabled_in_config(&self, site_id: &str) -> bool {
        if self.config.sites.is_empty() {
            return true;
        }
        match self.config.find_site(site_id) {
            Some(s) => s.enabled,
            None => true,
        }
    }

    fn site_auth_label(&self, site: &SiteProfile) -> String {
        let mgr = self.auth.with_account(&site.account_id);
        let creds = mgr.load_credentials().ok().flatten().or_else(|| {
            if site.account_id == self.auth.account_id() {
                self.auth.load_credentials().ok().flatten()
            } else {
                None
            }
        });
        match creds {
            Some(StoredCredentials::CloudOAuth { .. }) => "Cloud OAuth".to_string(),
            Some(StoredCredentials::CloudAccountToken { .. }) => "Cloud Token".to_string(),
            Some(StoredCredentials::ApiToken { deployment, .. }) => match deployment {
                DeploymentKind::Cloud => "Cloud Token".to_string(),
                DeploymentKind::DataCenter => "DC Basic".to_string(),
            },
            Some(StoredCredentials::BearerPat { deployment, .. }) => match deployment {
                DeploymentKind::Cloud => "Cloud Bearer".to_string(),
                DeploymentKind::DataCenter => "DC PAT".to_string(),
            },
            None => {
                if site.is_oauth_cloud() {
                    "Cloud OAuth".to_string()
                } else {
                    "Offline".to_string()
                }
            }
        }
    }

    /// Builds `SiteSwitcherOverlay` rows from all effective configured sites and SQLite cache counts.
    pub fn build_site_switcher_entries(&self) -> Vec<SiteSwitcherEntry> {
        let sites = self.config.effective_sites();
        let mut entries = Vec::with_capacity(sites.len());
        for site in sites {
            let counts = self
                .storage
                .cache_counts_for_site(&site.id)
                .unwrap_or_default();
            let deployment_label = match site.deployment {
                DeploymentKind::Cloud => "Cloud".to_string(),
                DeploymentKind::DataCenter => "Data Center".to_string(),
            };
            let auth_label = self.site_auth_label(&site);
            let is_active = self
                .site_scope
                .site_id()
                .is_some_and(|id| id.eq_ignore_ascii_case(&site.id));
            entries.push(SiteSwitcherEntry {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment_label,
                auth_label,
                enabled: site.enabled,
                is_active,
                issue_count: counts.issues,
                page_count: counts.pages,
            });
        }
        entries
    }

    /// Builds **Connected Sites & Accounts** rows for the Directory tab.
    pub fn build_connected_site_rows(&self) -> Vec<ConnectedSiteRow> {
        let sites = self.config.effective_sites();
        let mut rows = Vec::with_capacity(sites.len());
        for site in sites {
            let db_rec = self.storage.get_site(&site.id).ok().flatten();
            let deployment_label = match site.deployment {
                DeploymentKind::Cloud => "Cloud".to_string(),
                DeploymentKind::DataCenter => "Data Center".to_string(),
            };
            let auth_label = self.site_auth_label(&site);
            let is_active = match &self.site_scope {
                SiteScope::All => site.enabled,
                SiteScope::Only(id) => id.eq_ignore_ascii_case(&site.id),
            };
            let sync_status = if !site.enabled {
                "Disabled".to_string()
            } else if self.syncing {
                "Syncing…".to_string()
            } else if let Some(ref rec) = db_rec {
                if let Some(ref err) = rec.last_error {
                    format!("Error: {}", err.chars().take(24).collect::<String>())
                } else if let Some(ref ts) = rec.last_synced_at {
                    format!("Synced {}", ts.chars().take(16).collect::<String>())
                } else {
                    "Idle".to_string()
                }
            } else if auth_label == "Offline" {
                "Offline".to_string()
            } else {
                "Ready".to_string()
            };
            rows.push(ConnectedSiteRow {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment_label,
                auth_label,
                enabled: site.enabled,
                is_active,
                sync_status,
            });
        }
        rows
    }

    fn reset_active_view_for_site_switch(&mut self) {
        self.issue_detail_req_seq = self.issue_detail_req_seq.wrapping_add(1);
        self.page_detail_req_seq = self.page_detail_req_seq.wrapping_add(1);
        self.board_data_req_seq = self.board_data_req_seq.wrapping_add(1);
        self.issues_req_seq = self.issues_req_seq.wrapping_add(1);
        self.my_work_req_seq = self.my_work_req_seq.wrapping_add(1);
        self.remote_search_req_seq = self.remote_search_req_seq.wrapping_add(1);
        self.transitions_req_seq.fetch_add(1, Ordering::AcqRel);
        self.assignable_users_req_seq.fetch_add(1, Ordering::AcqRel);
        self.issues_custom_query = false;
        self.active_jql_label = None;
        self.active_jql_site_id = None;
        self.active_board_sprint_id = None;
        self.last_my_work_refresh_at = None;
        self.in_flight_issue_detail = None;
        self.in_flight_page_detail = None;
        self.active_issue = None;
        self.active_issue_site_id = None;
        self.active_issue_target = None;
        self.active_issue_detail_updated_at = None;
        self.active_page = None;
        self.active_page_site_id = None;
        self.active_page_target = None;
        self.active_page_detail_version = None;
        self.root_pages_by_space.clear();
        self.page_children_map.clear();
        self.expanded_pages.clear();
        self.board_issue_site_id = None;
    }

    fn reset_ui_for_site_switch(&self, ui: &mut UiState) {
        ui.search_ui.is_remote = false;
        ui.search_ui.selected_hit = 0;
        ui.board_ui.selected_sprint = 0;
        ui.board_ui.selected_col = 0;
        ui.board_ui.selected_row = 0;
        ui.board_ui.detail_scroll = 0;
        ui.board_ui.user_moved_board = false;
        ui.issue_ui.selected_filter = 0;
        ui.issue_ui.selected_issue = 0;
        ui.issue_ui.detail_scroll = 0;
        ui.issue_ui.local_filter.clear();
        ui.issue_ui.local_filter_active = false;
        ui.my_work_ui.selected_issue = 0;
        ui.my_work_ui.selected_page = 0;
        ui.confluence_ui.selected_tree_row = 0;
        ui.confluence_ui.selected_toc = 0;
        ui.confluence_ui.reader_scroll = 0;
        ui.confluence_ui.user_moved_space = false;
        ui.confluence_ui.space_filter.clear();
        ui.confluence_ui.space_filter_active = false;
        ui.directory_ui.selected_project = 0;
        ui.directory_ui.selected_space = 0;
    }

    /// Applies a `SiteSwitcherCommand` emitted by `SiteSwitcherOverlay`, persisting
    /// `active_site` / `enabled` to `config.toml` and refreshing federated views.
    pub fn apply_site_switcher_command(&mut self, cmd: SiteSwitcherCommand, ui: &mut UiState) {
        match cmd {
            SiteSwitcherCommand::SelectAllSites => {
                self.site_scope = SiteScope::All;
                self.config.active_site = None;
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
                ui.success("Scope: All Sites (Federated)");
            }
            SiteSwitcherCommand::SelectSite(site_id) => {
                let label = self.site_display_label(&site_id);
                self.site_scope = SiteScope::Only(site_id.clone());
                self.config.active_site = Some(site_id);
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
                ui.success(format!("Active site: {label}"));
            }
            SiteSwitcherCommand::ToggleSiteEnabled(site_id) => {
                if self.config.sites.is_empty() {
                    self.config.sites = self.config.effective_sites();
                }
                let mut now_enabled = None;
                let mut label = site_id.clone();
                for site in &mut self.config.sites {
                    if site.id.eq_ignore_ascii_case(&site_id) {
                        site.enabled = !site.enabled;
                        now_enabled = Some(site.enabled);
                        label = site.display_name().to_string();
                        let _ = self.storage.upsert_site(&SiteRecord {
                            site_id: site.id.clone(),
                            name: site.display_name().to_string(),
                            site_url: site.site_url.clone(),
                            deployment: site.deployment.as_str().to_string(),
                            enabled: site.enabled,
                            last_synced_at: None,
                            last_error: None,
                        });
                        break;
                    }
                }
                let disabled_active_scope = now_enabled == Some(false)
                    && self
                        .site_scope
                        .site_id()
                        .is_some_and(|active_id| active_id.eq_ignore_ascii_case(&site_id));
                if disabled_active_scope {
                    self.site_scope = SiteScope::All;
                    self.config.active_site = None;
                }
                self.rebuild_site_pool();
                self.reset_active_view_for_site_switch();
                self.reset_ui_for_site_switch(ui);
                if let Err(err) = self.save_config() {
                    ui.error(format!("Failed to save config.toml: {err}"));
                }
                self.load_cached_initial();
                self.refresh_local_search(&ui.search_ui.query.text);
                self.apply_default_selection(ui);
                self.clamp_selections(ui);
                self.sync_open_item_watch(ui);
                if let Some(enabled) = now_enabled {
                    ui.info(format!(
                        "{}: {}",
                        label,
                        if enabled { "Enabled" } else { "Disabled" }
                    ));
                }
            }
            SiteSwitcherCommand::DiscoverCloudSites => {
                self.request_discover_cloud_sites(ui);
            }
            SiteSwitcherCommand::SyncAllSites => {
                self.request_multi_site_sync(ui);
            }
        }
    }

    /// Triggers Cloud OAuth 2.0 (3LO) `accessible-resources` site discovery in the background.
    pub fn request_discover_cloud_sites(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.info("Run `atlassian login --oauth` or `atlassian sites discover` to discover Cloud sites");
            return;
        }
        // Check if any configured account already holds Cloud OAuth credentials.
        let mut oauth_account: Option<AuthManager> = None;
        if let Ok(Some(StoredCredentials::CloudOAuth { .. })) = self.auth.load_credentials() {
            oauth_account = Some(self.auth.clone());
        }
        if oauth_account.is_none() {
            for (site, client) in self.site_pool.all_clients() {
                if let Ok(Some(StoredCredentials::CloudOAuth { .. })) =
                    client.auth().load_credentials()
                {
                    oauth_account = Some(client.auth().clone());
                    let _ = site;
                    break;
                }
            }
        }

        if let Some(auth_mgr) = oauth_account {
            ui.tasks.begin("Discover Cloud sites");
            let http = self.site_pool.http_client().clone();
            let tx = self.ui_tx.clone();
            let account_id = auth_mgr.account_id().to_string();
            tokio::spawn(async move {
                let res = async {
                    let fresh = auth_mgr
                        .ensure_fresh_credentials(&http, DEFAULT_OAUTH_AUTH_BASE)
                        .await?;
                    let token = fresh
                        .oauth_access_token()
                        .ok_or_else(|| anyhow::anyhow!("OAuth access token missing"))?;
                    let resources =
                        fetch_accessible_resources(&http, DEFAULT_OAUTH_API_BASE, token).await?;
                    let profiles = resources
                        .into_iter()
                        .map(|r| r.to_site_profile(account_id.clone()))
                        .collect::<Vec<_>>();
                    Ok::<Vec<SiteProfile>, anyhow::Error>(profiles)
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::CloudSitesDiscovered(res));
            });
            return;
        }

        if let Some(client_id) = self.config.effective_oauth_client_id() {
            ui.tasks.begin("Discover Cloud sites");
            let http = self.site_pool.http_client().clone();
            let tx = self.ui_tx.clone();
            let auth_mgr = self.auth.clone();
            let client_secret = self.config.effective_oauth_client_secret();
            tokio::spawn(async move {
                let opts = OAuthLoopbackOptions {
                    client_id,
                    client_secret,
                    ..Default::default()
                };
                let res = async {
                    let (creds, resources) = run_oauth_loopback_login(&http, &opts).await?;
                    auth_mgr.save_credentials(&creds)?;
                    let account_id = auth_mgr.account_id().to_string();
                    let profiles = resources
                        .into_iter()
                        .map(|r| r.to_site_profile(account_id.clone()))
                        .collect::<Vec<_>>();
                    Ok::<Vec<SiteProfile>, anyhow::Error>(profiles)
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::CloudSitesDiscovered(res));
            });
            return;
        }

        ui.info(
            "No OAuth session found. Run `atlassian login --oauth --client-id <ID>` to discover Cloud sites.",
        );
    }

    /// Triggers parallel multi-site sync across all enabled sites (or falls back to single-site sync).
    pub fn request_multi_site_sync(&mut self, ui: &mut UiState) {
        self.syncing = true;
        let enabled = self.site_pool.enabled_clients();
        if enabled.len() > 1 && tokio::runtime::Handle::try_current().is_ok() {
            ui.tasks.begin("Sync all sites");
            let targets: Vec<SiteSyncTarget> = enabled
                .into_iter()
                .map(|(site, client)| SiteSyncTarget {
                    site_id: site.id.clone(),
                    site_name: site.display_name().to_string(),
                    client,
                })
                .collect();
            let storage = self.storage.clone();
            let tx = self.ui_tx.clone();
            tokio::spawn(async move {
                let mut breakers = sync::CircuitBreakerMap::new();
                let outcome = sync::run_multi_site_sync(
                    &targets,
                    &storage,
                    &mut breakers,
                    sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                    None,
                )
                .await;
                let _ = tx.send(UiMsg::MultiSiteSyncFinished(outcome));
            });
            ui.info("Syncing all enabled sites in parallel…");
        } else {
            let _ = self.sync_ctrl.request(sync::SyncCommand::RefreshAll);
            ui.info("Requested full workspace sync");
        }
    }

    /// Returns all known per-site signed-in account IDs (lowercase `site_id -> user.id`)
    /// from SQLite persisted identities, `site_pool` cached profiles, and primary `self.me`.
    pub fn known_me_ids_by_site(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Ok(stored) = self.storage.list_identities() {
            for (sid, user) in stored {
                if !user.id.trim().is_empty() {
                    out.insert(sid.trim().to_ascii_lowercase(), user.id);
                }
            }
        }
        for (site, client) in self.site_pool.all_clients() {
            if let Some(me) = client.cached_me() {
                if !me.id.trim().is_empty() {
                    out.insert(site.id.trim().to_ascii_lowercase(), me.id);
                }
            }
        }
        if let Some(me) = self.me.clone().or_else(|| self.client.cached_me()) {
            if !me.id.trim().is_empty() {
                out.insert(
                    self.client.site_id().trim().to_ascii_lowercase(),
                    me.id,
                );
            }
        }
        out
    }

    /// Re-reads `cache_counts` scoped to the active site (or across all sites in federated mode).
    pub fn refresh_scoped_cache_counts(&mut self) {
        let counts = match self.site_scope.site_id() {
            Some(sid) => self.storage.cache_counts_for_site(sid),
            None => self.storage.cache_counts(),
        };
        if let Ok(c) = counts {
            self.cache_counts = c;
        }
    }

    /// Resets the Issues tab custom JQL / Saved Filter protection flag back to default view mode.
    pub fn reset_issues_custom_query(&mut self) {
        self.issues_custom_query = false;
        self.active_jql_label = None;
        self.active_jql_site_id = None;
    }

    /// Re-reads `my_issues` from SQLite respecting the active `site_scope`, per-site `me_id`,
    /// enabled sites, and excluding `Done` issues.
    pub(super) fn reload_my_issues_from_storage(&mut self) {
        let show_pills = self.site_scope.is_all();
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                let (_, site_client) = self.site_id_and_client_for(Some(site_id));
                let me_id = self.known_me_for_site(site_id, &site_client).map(|m| m.id);
                self.my_issues = self
                    .storage
                    .list_my_work_for_site(site_id, me_id.as_deref(), 40)
                    .unwrap_or_default();
                self.my_issue_site_ids = vec![site_id.clone(); self.my_issues.len()];
                self.my_issue_site_labels = vec![None; self.my_issues.len()];
            }
            None => {
                let me_ids = self.known_me_ids_by_site();
                let my_work_pairs: Vec<(String, IssueSummary)> = self
                    .storage
                    .list_my_work_all_sites_for_users(&me_ids, 40)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                    .collect();
                let has_multi = self.config.enabled_sites().len() > 1
                    || my_work_pairs.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                self.my_issue_site_ids = my_work_pairs.iter().map(|(sid, _)| sid.clone()).collect();
                self.my_issue_site_labels = my_work_pairs
                    .iter()
                    .map(|(sid, _)| {
                        (show_pills && has_multi).then(|| self.site_display_label(sid))
                    })
                    .collect();
                self.my_issues = my_work_pairs.into_iter().map(|(_, i)| i).collect();
            }
        }
    }

    /// Re-reads `my_pages` from SQLite respecting the active `site_scope` and enabled sites.
    pub(super) fn reload_my_pages_from_storage(&mut self) {
        let show_pills = self.site_scope.is_all();
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                if let Ok(pages) = self.storage.list_recent_pages_for_site(site_id, 30) {
                    self.my_pages = pages;
                    self.my_page_site_ids = vec![site_id.clone(); self.my_pages.len()];
                    self.my_page_site_labels = vec![None; self.my_pages.len()];
                }
            }
            None => {
                if let Ok(pages_all) = self.storage.list_recent_pages_all_sites(30) {
                    let filtered: Vec<(String, PageSummary)> = pages_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    let has_multi = self.config.enabled_sites().len() > 1
                        || filtered.iter().any(|(s, _)| s != DEFAULT_SITE_ID);
                    self.my_page_site_ids = filtered.iter().map(|(sid, _)| sid.clone()).collect();
                    self.my_page_site_labels = filtered
                        .iter()
                        .map(|(sid, _)| {
                            (show_pills && has_multi).then(|| self.site_display_label(sid))
                        })
                        .collect();
                    self.my_pages = filtered.into_iter().map(|(_, p)| p).collect();
                }
            }
        }
    }

    /// Clamps every list/tree/column cursor index in `UiState` to current collection lengths.
    pub fn clamp_selections(&self, ui: &mut UiState) {
        let clamp = |idx: &mut usize, len: usize| {
            if len == 0 {
                *idx = 0;
            } else if *idx >= len {
                *idx = len - 1;
            }
        };
        clamp(&mut ui.board_ui.selected_board, self.boards.len());
        clamp(&mut ui.board_ui.selected_sprint, self.sprints.len());
        let cols = self.build_kanban_columns(ui);
        clamp(&mut ui.board_ui.selected_col, cols.len());
        let col_len = cols
            .get(ui.board_ui.selected_col)
            .map(|c| c.issues.len())
            .unwrap_or(0);
        clamp(&mut ui.board_ui.selected_row, col_len);

        clamp(&mut ui.issue_ui.selected_filter, self.saved_filters.len());
        let filtered_len = self.filtered_issues(ui).len();
        clamp(&mut ui.issue_ui.selected_issue, filtered_len);

        clamp(&mut ui.my_work_ui.selected_issue, self.my_issues.len());
        clamp(&mut ui.my_work_ui.selected_page, self.my_pages.len());

        clamp(&mut ui.confluence_ui.selected_space, self.spaces.len());
        let tree_len = self.current_confluence_tree(ui).len();
        clamp(&mut ui.confluence_ui.selected_tree_row, tree_len);
        let toc_len = self
            .active_page
            .as_ref()
            .map(|p| p.toc.iter().filter(|t| t.level <= 4).count())
            .unwrap_or(0);
        clamp(&mut ui.confluence_ui.selected_toc, toc_len);

        clamp(&mut ui.search_ui.selected_hit, self.search_hits.len());

        clamp(&mut ui.directory_ui.selected_project, self.projects.len());
        clamp(&mut ui.directory_ui.selected_space, self.spaces.len());
        let site_count = self.config.effective_sites().len();
        clamp(&mut ui.directory_ui.selected_site, site_count);
    }

    /// Re-sorts `projects` and `spaces` so pinned items float to the top (matching SQLite order),
    /// keeping parallel `project_site_ids` and `space_site_ids` strictly aligned.
    pub fn sort_directory_pinned(&mut self) {
        let multi = self.is_multi_site_all_mode();

        let mut proj_pairs: Vec<(String, ProjectSummary)> = self
            .projects
            .drain(..)
            .enumerate()
            .map(|(i, p)| {
                let sid = self
                    .project_site_ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_SITE_ID.to_string());
                (sid, p)
            })
            .collect();
        proj_pairs.sort_by(|(sa, a), (sb, b)| {
            let site_ord = if multi {
                sa.cmp(sb)
            } else {
                std::cmp::Ordering::Equal
            };
            site_ord
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        self.project_site_ids = proj_pairs.iter().map(|(s, _)| s.clone()).collect();
        self.projects = proj_pairs.into_iter().map(|(_, p)| p).collect();

        let mut space_pairs: Vec<(String, SpaceSummary)> = self
            .spaces
            .drain(..)
            .enumerate()
            .map(|(i, s)| {
                let sid = self
                    .space_site_ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_SITE_ID.to_string());
                (sid, s)
            })
            .collect();
        space_pairs.sort_by(|(sa, a), (sb, b)| {
            let site_ord = if multi {
                sa.cmp(sb)
            } else {
                std::cmp::Ordering::Equal
            };
            site_ord
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        self.space_site_ids = space_pairs.iter().map(|(s, _)| s.clone()).collect();
        self.spaces = space_pairs.into_iter().map(|(_, s)| s).collect();
    }

    /// Reloads SQLite cache while preserving user-selected board/sprint, space, custom JQL
    /// results, and active Search results (never wiping search with an empty query).
    pub fn reload_cached_preserving_ui(&mut self, ui: &mut UiState) {
        let prev_board: Option<(String, i64)> = if ui.board_ui.user_moved_board {
            self.active_board_with_site(ui).map(|(sid, b)| (sid, b.id))
        } else {
            None
        };
        let prev_sprint_id = self
            .active_board_sprint_id
            .or_else(|| self.sprints.get(ui.board_ui.selected_sprint).map(|s| s.id));
        let prev_space: Option<(String, String)> = if ui.confluence_ui.user_moved_space {
            self.active_space(ui).map(|sp| (self.tree_site(ui), sp.key.clone()))
        } else {
            None
        };
        let preserved_custom_issues = if self.issues_custom_query {
            Some((self.issues.clone(), self.issue_site_ids.clone()))
        } else {
            None
        };

        self.load_cached_initial();

        if let Some((saved_issues, saved_site_ids)) = preserved_custom_issues {
            self.issues = saved_issues;
            self.issue_site_ids = saved_site_ids;
        }

        if let Some((prev_sid, prev_bid)) = prev_board {
            if let Some(new_idx) = self
                .boards
                .iter()
                .enumerate()
                .position(|(idx, b)| {
                    b.id == prev_bid
                        && self
                            .board_site_ids
                            .get(idx)
                            .is_some_and(|s| s.eq_ignore_ascii_case(&prev_sid))
                })
            {
                ui.board_ui.selected_board = new_idx;
                let board_sid = self
                    .board_site_ids
                    .get(new_idx)
                    .cloned()
                    .unwrap_or(prev_sid);
                self.board_issue_site_id = Some(board_sid.clone());
                if let Ok(sprints) = self.storage.list_sprints_for_site(&board_sid, prev_bid) {
                    self.sprints = sprints;
                }
                let matched_sprint = prev_sprint_id.and_then(|sid| {
                    self.sprints
                        .iter()
                        .position(|s| s.id == sid)
                        .map(|idx| (idx, sid))
                });
                let effective_sprint = if let Some((sp_idx, sid)) = matched_sprint {
                    ui.board_ui.selected_sprint = sp_idx;
                    Some(sid)
                } else {
                    self.sprints
                        .iter()
                        .position(|s| s.state.eq_ignore_ascii_case("active"))
                        .map(|idx| {
                            ui.board_ui.selected_sprint = idx;
                            self.sprints[idx].id
                        })
                };
                self.active_board_sprint_id = effective_sprint;
                if let Ok(bi) = self.storage.list_board_issues_for_site(
                    &board_sid,
                    prev_bid,
                    effective_sprint,
                    BOARD_ISSUE_VIEW_CAP,
                ) {
                    self.board_issues = bi;
                }
            }
        }

        if let Some((prev_sid, prev_key)) = prev_space {
            if let Some(new_idx) = self
                .spaces
                .iter()
                .enumerate()
                .position(|(idx, sp)| {
                    sp.key.eq_ignore_ascii_case(&prev_key)
                        && self
                            .space_site_ids
                            .get(idx)
                            .is_some_and(|s| s.eq_ignore_ascii_case(&prev_sid))
                })
            {
                ui.confluence_ui.selected_space = new_idx;
                let space_sid = self
                    .space_site_ids
                    .get(new_idx)
                    .cloned()
                    .unwrap_or(prev_sid);
                let space_key = self.spaces[new_idx].key.clone();
                if let Ok(roots) = self
                    .storage
                    .list_space_root_pages_for_site(&space_sid, &space_key)
                {
                    self.root_pages_by_space
                        .insert(SiteKey::new(&space_sid, &space_key), roots);
                }
            }
        }

        self.apply_default_selection(ui);

        if !ui.search_ui.is_remote && !ui.search_ui.query.text.trim().is_empty() {
            let q = ui.search_ui.query.text.clone();
            self.refresh_local_search(&q);
        }

        self.clamp_selections(ui);
    }

    pub fn load_cached_initial(&mut self) {
        match self.site_scope.site_id().map(str::to_string) {
            Some(ref site_id) => {
                if let Ok(projs) = self.storage.list_projects_for_site(site_id) {
                    self.project_site_ids = vec![site_id.clone(); projs.len()];
                    self.projects = projs;
                }
                if let Ok(boards) = self.storage.list_boards_for_site(site_id, None) {
                    self.board_site_ids = vec![site_id.clone(); boards.len()];
                    self.boards = boards;
                    if let Some(default_board) = self.boards.get(self.resolve_default_board_index())
                    {
                        let bid = default_board.id;
                        self.board_issue_site_id = Some(site_id.clone());
                        if let Ok(sprints) = self.storage.list_sprints_for_site(site_id, bid) {
                            self.sprints = sprints;
                        }
                        self.active_board_sprint_id = self
                            .sprints
                            .iter()
                            .find(|s| s.state.eq_ignore_ascii_case("active"))
                            .map(|s| s.id);
                        if let Ok(bi) =
                            self.storage
                                .list_board_issues_for_site(site_id, bid, None, BOARD_ISSUE_VIEW_CAP)
                        {
                            self.board_issues = bi;
                        }
                    } else {
                        self.board_issue_site_id = None;
                        self.sprints.clear();
                        self.board_issues.clear();
                        self.active_board_sprint_id = None;
                    }
                }
                if let Ok(mut filters) = self.storage.list_saved_filters_for_site(site_id) {
                    let mut combined = builtin_filters();
                    for f in filters.drain(..) {
                        if !combined.iter().any(|b| b.id == f.id) {
                            combined.push(f);
                        }
                    }
                    self.saved_filters = combined;
                } else {
                    self.saved_filters = builtin_filters();
                }
                if let Ok(issues) = self.storage.list_issues_for_site(site_id, None, 200) {
                    self.issue_site_ids = vec![site_id.clone(); issues.len()];
                    self.issues = issues.clone();
                    self.reload_my_issues_from_storage();
                    let preserved_key = self
                        .active_issue
                        .as_ref()
                        .filter(|_| {
                            self.active_issue_site_id
                                .as_deref()
                                .map(|s| s.eq_ignore_ascii_case(site_id))
                                .unwrap_or(true)
                        })
                        .map(|d| d.summary.key.clone())
                        .filter(|k| issues.iter().any(|i| &i.key == k));
                    if let Some(key) = preserved_key {
                        if let Ok(Some(detail)) = self.storage.get_issue_detail_for_site(site_id, &key) {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(site_id, &key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(site_id.clone());
                            self.active_issue_target = Some((site_id.clone(), key));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    } else if let Some(first) = issues.first() {
                        if let Ok(Some(detail)) =
                            self.storage.get_issue_detail_for_site(site_id, &first.key)
                        {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(site_id, &first.key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(site_id.clone());
                            self.active_issue_target = Some((site_id.clone(), first.key.clone()));
                            self.active_issue_detail_updated_at = detail_ts;
                        } else if self
                            .active_issue_site_id
                            .as_deref()
                            .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                        {
                            self.active_issue = None;
                            self.active_issue_site_id = None;
                            self.active_issue_target = None;
                            self.active_issue_detail_updated_at = None;
                        }
                    } else if self
                        .active_issue_site_id
                        .as_deref()
                        .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                    {
                        self.active_issue = None;
                        self.active_issue_site_id = None;
                        self.active_issue_target = None;
                        self.active_issue_detail_updated_at = None;
                    }
                }
                if let Some(active_page_id) = self.active_page.as_ref().map(|p| p.summary.id.clone()) {
                    if let Ok(Some(detail)) =
                        self.storage.get_page_detail_for_site(site_id, &active_page_id)
                    {
                        let detail_ver = self
                            .storage
                            .get_page_detail_version_for_site(site_id, &active_page_id)
                            .ok()
                            .flatten();
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(site_id.clone());
                        self.active_page_target = Some((site_id.clone(), active_page_id));
                        self.active_page_detail_version = detail_ver;
                    } else if self
                        .active_page_site_id
                        .as_deref()
                        .is_some_and(|s| !s.eq_ignore_ascii_case(site_id))
                    {
                        self.active_page = None;
                        self.active_page_site_id = None;
                        self.active_page_target = None;
                        self.active_page_detail_version = None;
                    }
                }
                if let Ok(spaces) = self.storage.list_spaces_for_site(site_id) {
                    self.space_site_ids = vec![site_id.clone(); spaces.len()];
                    self.spaces = spaces;
                    if let Some(default_space) = self.spaces.get(self.resolve_default_space_index())
                    {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(site_id, &default_space.key)
                        {
                            self.root_pages_by_space
                                .insert(SiteKey::new(site_id, &default_space.key), roots);
                        }
                    }
                }
                self.reload_my_pages_from_storage();
                if let Ok(counts) = self.storage.cache_counts_for_site(site_id) {
                    self.cache_counts = counts;
                }
            }
            None => {
                self.reload_projects_all_sites();
                if let Ok(boards) = self.storage.list_boards_all_sites(None) {
                    let filtered: Vec<(String, BoardSummary)> = boards
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.board_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.boards = filtered.into_iter().map(|(_, b)| b).collect();
                    let default_idx = self.resolve_default_board_index();
                    if let (Some(default_board), Some(board_sid)) = (
                        self.boards.get(default_idx),
                        self.board_site_ids.get(default_idx),
                    ) {
                        let bid = default_board.id;
                        self.board_issue_site_id = Some(board_sid.clone());
                        if let Ok(sprints) = self.storage.list_sprints_for_site(board_sid, bid) {
                            self.sprints = sprints;
                        }
                        self.active_board_sprint_id = self
                            .sprints
                            .iter()
                            .find(|s| s.state.eq_ignore_ascii_case("active"))
                            .map(|s| s.id);
                        if let Ok(bi) = self.storage.list_board_issues_for_site(
                            board_sid,
                            bid,
                            None,
                            BOARD_ISSUE_VIEW_CAP,
                        ) {
                            self.board_issues = bi;
                        }
                    } else {
                        self.board_issue_site_id = None;
                        self.sprints.clear();
                        self.board_issues.clear();
                        self.active_board_sprint_id = None;
                    }
                }
                {
                    let mut combined = builtin_filters();
                    let filter_site_id = self.client.site_id().to_string();
                    if let Ok(filters) = self.storage.list_saved_filters_for_site(&filter_site_id) {
                        for f in filters {
                            if !combined.iter().any(|b| b.id == f.id) {
                                combined.push(f);
                            }
                        }
                    }
                    if !filter_site_id.eq_ignore_ascii_case(DEFAULT_SITE_ID) {
                        if let Ok(filters) = self.storage.list_saved_filters() {
                            for f in filters {
                                if !combined.iter().any(|b| b.id == f.id) {
                                    combined.push(f);
                                }
                            }
                        }
                    }
                    self.saved_filters = combined;
                }
                if let Ok(issues_all) = self.storage.list_issues_all_sites(None, 200) {
                    let filtered: Vec<(String, IssueSummary)> = issues_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.issue_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.issues = filtered.iter().map(|(_, i)| i.clone()).collect();

                    self.reload_my_issues_from_storage();

                    let preserved = self.active_issue.as_ref().and_then(|d| {
                        let existing_sid = self.active_issue_site_id.as_deref();
                        filtered
                            .iter()
                            .find(|(sid, i)| {
                                i.key == d.summary.key
                                    && existing_sid
                                        .map(|e| e.eq_ignore_ascii_case(sid))
                                        .unwrap_or(true)
                            })
                            .map(|(sid, i)| (sid.clone(), i.key.clone()))
                    });
                    if let Some((sid, key)) = preserved {
                        if let Ok(Some(detail)) = self.storage.get_issue_detail_for_site(&sid, &key) {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(&sid, &key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(sid.clone());
                            self.active_issue_target = Some((sid, key));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    } else if let Some((first_sid, first)) = filtered.first() {
                        if let Ok(Some(detail)) = self
                            .storage
                            .get_issue_detail_for_site(first_sid, &first.key)
                        {
                            let detail_ts = self
                                .storage
                                .get_issue_detail_updated_at_for_site(first_sid, &first.key)
                                .ok()
                                .flatten();
                            self.active_issue = Some(detail);
                            self.active_issue_site_id = Some(first_sid.clone());
                            self.active_issue_target = Some((first_sid.clone(), first.key.clone()));
                            self.active_issue_detail_updated_at = detail_ts;
                        }
                    }
                }
                if let Some(active_page_id) = self.active_page.as_ref().map(|p| p.summary.id.clone()) {
                    let target_sid = self
                        .active_page_site_id
                        .clone()
                        .unwrap_or_else(|| self.client.site_id().to_string());
                    if let Ok(Some(detail)) =
                        self.storage.get_page_detail_for_site(&target_sid, &active_page_id)
                    {
                        let detail_ver = self
                            .storage
                            .get_page_detail_version_for_site(&target_sid, &active_page_id)
                            .ok()
                            .flatten();
                        self.active_page = Some(detail);
                        self.active_page_site_id = Some(target_sid.clone());
                        self.active_page_target = Some((target_sid, active_page_id));
                        self.active_page_detail_version = detail_ver;
                    }
                }
                if let Ok(spaces_all) = self.storage.list_spaces_all_sites() {
                    let filtered: Vec<(String, SpaceSummary)> = spaces_all
                        .into_iter()
                        .filter(|(sid, _)| self.site_is_enabled_in_config(sid))
                        .collect();
                    self.space_site_ids = filtered.iter().map(|(s, _)| s.clone()).collect();
                    self.spaces = filtered.into_iter().map(|(_, s)| s).collect();
                    let default_idx = self.resolve_default_space_index();
                    if let (Some(default_space), Some(space_sid)) = (
                        self.spaces.get(default_idx),
                        self.space_site_ids.get(default_idx),
                    ) {
                        if let Ok(roots) = self
                            .storage
                            .list_space_root_pages_for_site(space_sid, &default_space.key)
                        {
                            self.root_pages_by_space
                                .insert(SiteKey::new(space_sid, &default_space.key), roots);
                        }
                    }
                }
                self.reload_my_pages_from_storage();
                if let Ok(counts) = self.storage.cache_counts() {
                    self.cache_counts = counts;
                }
            }
        }
    }

    pub fn spawn_bootstrap(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let mut targets: Vec<(String, String, AtlassianClient)> = self
            .site_pool
            .enabled_clients()
            .into_iter()
            .filter(|(site, client)| {
                !site.site_url.trim().is_empty() && client.auth().is_logged_in()
            })
            .map(|(site, client)| {
                (
                    site.id.clone(),
                    site.display_name().to_string(),
                    client,
                )
            })
            .collect();
        if targets.is_empty()
            && self.auth.is_logged_in()
            && !self.config.site_url.trim().is_empty()
        {
            targets.push((
                self.client.site_id().to_string(),
                self.site_display_label(self.client.site_id()),
                self.client.clone(),
            ));
        }
        if targets.is_empty() {
            ui.info("Offline cache mode (run `atlassian login` to connect)");
            return;
        }

        ui.tasks.begin("Initial sync");
        let primary_site_id = self.client.site_id().to_string();
        let active_scope = self.site_scope.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let me_targets = targets.clone();
            let filter_targets = targets.clone();
            let me_primary = primary_site_id.clone();
            let me_storage = storage.clone();
            let me_tx = tx.clone();
            let me_fut = async move {
                let primary_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let total_targets = me_targets.len();
                stream::iter(me_targets.into_iter().enumerate())
                    .map(|(idx, (site_id, _site_name, client))| {
                        let primary_sent = primary_sent.clone();
                        let primary_site_id = me_primary.clone();
                        let storage = me_storage.clone();
                        let tx = me_tx.clone();
                        async move {
                            let is_primary = site_id.eq_ignore_ascii_case(&primary_site_id)
                                || (total_targets == 1 && idx == 0);
                            match client.get_myself().await {
                                Ok(me) => {
                                    client.set_cached_me(me.clone());
                                    let _ = storage.save_identity_for_site(&site_id, &me);
                                    if is_primary
                                        && !primary_sent.swap(true, Ordering::AcqRel)
                                    {
                                        let _ = tx.send(UiMsg::MeLoaded(Ok(me)));
                                    }
                                }
                                Err(e) => {
                                    if is_primary
                                        && !primary_sent.swap(true, Ordering::AcqRel)
                                    {
                                        let _ = tx.send(UiMsg::MeLoaded(Err(e.to_string())));
                                    }
                                }
                            }
                        }
                    })
                    .buffer_unordered(sync::DEFAULT_MULTI_SITE_CONCURRENCY)
                    .for_each(|()| async {})
                    .await;
            };

            let sync_storage = storage.clone();
            let sync_tx = tx.clone();
            let sync_fut = async move {
                let summary = if targets.len() == 1 {
                    let (site_id, _, client) = &targets[0];
                    sync::bootstrap_workspace_for_site(site_id, client, &sync_storage)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    let sync_targets: Vec<SiteSyncTarget> = targets
                        .iter()
                        .map(|(site_id, site_name, client)| SiteSyncTarget {
                            site_id: site_id.clone(),
                            site_name: site_name.clone(),
                            client: client.clone(),
                        })
                        .collect();
                    let mut breakers = sync::CircuitBreakerMap::new();
                    let outcome = sync::run_multi_site_sync(
                        &sync_targets,
                        &sync_storage,
                        &mut breakers,
                        sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                        None,
                    )
                    .await;
                    if outcome.succeeded_sites > 0 {
                        Ok(outcome.summary)
                    } else {
                        let first_err = outcome
                            .per_site
                            .iter()
                            .find_map(|r| r.error.clone())
                            .unwrap_or(outcome.summary);
                        Err(first_err)
                    }
                };
                let _ = sync_tx.send(UiMsg::BootstrapFinished(summary));
            };

            let filters_fut = async move {
                let per_site: Vec<(String, Vec<SavedFilter>)> = stream::iter(filter_targets)
                    .map(|(site_id, _, client)| {
                        let storage = storage.clone();
                        async move {
                            match client.list_favourite_filters().await {
                                Ok(filters) => {
                                    let _ = storage.upsert_saved_filters_for_site(&site_id, &filters);
                                    Some((site_id, filters))
                                }
                                Err(_) => None,
                            }
                        }
                    })
                    .buffered(sync::DEFAULT_MULTI_SITE_CONCURRENCY)
                    .filter_map(|item| async move { item })
                    .collect()
                    .await;
                let mut visible_filters = Vec::new();
                for (site_id, filters) in per_site {
                    if active_scope.matches(&site_id) {
                        for f in filters {
                            if !visible_filters
                                .iter()
                                .any(|existing: &SavedFilter| existing.id == f.id)
                            {
                                visible_filters.push(f);
                            }
                        }
                    }
                }
                if !visible_filters.is_empty() {
                    let _ = tx.send(UiMsg::SavedFiltersLoaded(visible_filters));
                }
            };

            tokio::join!(me_fut, sync_fut, filters_fut);
        });
    }

}
