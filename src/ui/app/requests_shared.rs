//! Cross-product async requests: My Work, remote search, sibling prefetch, media/HTML preview, updates.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures_util::{stream, StreamExt};

use crate::{
    atlassian::AtlassianClient,
    img_preview,
    models::{AttachmentItem, IssueSummary, PageSummary, SiteScope, UnifiedSearchHit},
    sync,
    ui::overlays::{
        path_input::{PathInputOverlay, UploadTarget},
        Overlay,
    },
    updater,
};

use super::{
    state::{UiMsg, UiState, WorkspaceTab},
    AppState, SiteTaggedHit, SiteTaggedIssue, SiteTaggedPage,
    INTERACTIVE_SIBLING_PREFETCH_CONCURRENCY, INTERACTIVE_SIBLING_PREFETCH_LIMIT,
};

impl AppState {
    pub fn maybe_refresh_my_work_on_tab_switch(&mut self, ui: &mut UiState) {
        const MY_WORK_STALE_SECS: u64 = 30;
        let is_stale = self
            .last_my_work_refresh_at
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(MY_WORK_STALE_SECS));
        if is_stale || (self.my_issues.is_empty() && self.my_pages.is_empty()) {
            self.request_my_work_refresh(ui);
        }
    }

    pub fn request_my_work_refresh(&mut self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.my_work_req_seq = self.my_work_req_seq.wrapping_add(1);
        let seq = self.my_work_req_seq;
        self.last_my_work_refresh_at = Some(Instant::now());
        ui.tasks.begin("Loading My Work");
        let clients: Vec<(String, String, Option<String>, AtlassianClient)> = match &self.site_scope {
            SiteScope::Only(id) => {
                let c = self
                    .site_pool
                    .client_for(id)
                    .unwrap_or_else(|_| self.client.clone());
                let me_id = self.known_me_for_site(id, &c).map(|m| m.id);
                vec![(id.clone(), self.site_display_label(id), me_id, c)]
            }
            SiteScope::All => {
                let enabled = self.site_pool.enabled_clients();
                if enabled.is_empty() {
                    let sid = self.client.site_id().to_string();
                    let me_id = self.known_me_for_site(&sid, &self.client).map(|m| m.id);
                    vec![(
                        sid.clone(),
                        self.site_display_label(&sid),
                        me_id,
                        self.client.clone(),
                    )]
                } else {
                    enabled
                        .into_iter()
                        .map(|(s, c)| {
                            let me_id = self.known_me_for_site(&s.id, &c).map(|m| m.id);
                            (s.id.clone(), s.display_name().to_string(), me_id, c)
                        })
                        .collect()
                }
            }
        };
        let show_pills = self.site_scope.is_all() && clients.len() > 1;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let per_site: Vec<(Vec<SiteTaggedIssue>, Vec<SiteTaggedPage>)> = stream::iter(clients)
                .map(|(site_id, site_label, me_id, client)| {
                    let storage = storage.clone();
                    async move {
                        let pill = show_pills.then_some(site_label);
                        let (issues_res, pages_res) = tokio::join!(
                            client.search_issues_jql(
                                AtlassianClient::my_open_issues_jql(),
                                40,
                                None,
                                None,
                            ),
                            client.search_confluence_cql(
                                "type = page ORDER BY lastmodified DESC",
                                25,
                            ),
                        );
                        let issues = match issues_res {
                            Ok(p) => {
                                let _ = storage.upsert_issues_for_site(&site_id, &p.issues);
                                p.issues
                            }
                            Err(_) => storage
                                .list_my_work_for_site(&site_id, me_id.as_deref(), 40)
                                .unwrap_or_default(),
                        };
                        let pages = match pages_res {
                            Ok(ps) => {
                                let _ = storage.upsert_pages_for_site(&site_id, &ps);
                                ps
                            }
                            Err(_) => storage
                                .list_recent_pages_for_site(&site_id, 25)
                                .unwrap_or_default(),
                        };
                        let site_issues = issues
                            .into_iter()
                            .map(|issue| (site_id.clone(), pill.clone(), issue))
                            .collect();
                        let site_pages = pages
                            .into_iter()
                            .map(|page| (site_id.clone(), pill.clone(), page))
                            .collect();
                        (site_issues, site_pages)
                    }
                })
                .buffer_unordered(sync::DEFAULT_MULTI_SITE_CONCURRENCY)
                .collect()
                .await;

            let mut all_issues: Vec<(String, Option<String>, IssueSummary)> = Vec::new();
            let mut all_pages: Vec<(String, Option<String>, PageSummary)> = Vec::new();
            for (si, sp) in per_site {
                all_issues.extend(si);
                all_pages.extend(sp);
            }
            all_issues.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            all_pages.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            let mut issue_site_ids = Vec::with_capacity(all_issues.len());
            let mut issue_site_labels = Vec::with_capacity(all_issues.len());
            let mut issues = Vec::with_capacity(all_issues.len());
            for (sid, lbl, item) in all_issues {
                issue_site_ids.push(sid);
                issue_site_labels.push(lbl);
                issues.push(item);
            }
            let mut page_site_ids = Vec::with_capacity(all_pages.len());
            let mut page_site_labels = Vec::with_capacity(all_pages.len());
            let mut pages = Vec::with_capacity(all_pages.len());
            for (sid, lbl, item) in all_pages {
                page_site_ids.push(sid);
                page_site_labels.push(lbl);
                pages.push(item);
            }
            let _ = tx.send(UiMsg::MyWorkLoaded {
                seq,
                issues,
                issue_site_ids,
                issue_site_labels,
                pages,
                page_site_ids,
                page_site_labels,
            });
        });
    }

    pub fn request_remote_search(&mut self, ui: &mut UiState, query: &str) {
        let q = query.trim().to_string();
        if q.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.remote_search_req_seq = self.remote_search_req_seq.wrapping_add(1);
        let seq = self.remote_search_req_seq;
        ui.tasks.begin(format!("Remote search: {q}"));
        let clients: Vec<(String, String, AtlassianClient)> = match &self.site_scope {
            SiteScope::Only(id) => {
                let c = self
                    .site_pool
                    .client_for(id)
                    .unwrap_or_else(|_| self.client.clone());
                vec![(id.clone(), self.site_display_label(id), c)]
            }
            SiteScope::All => {
                let enabled = self.site_pool.enabled_clients();
                if enabled.is_empty() {
                    vec![(
                        self.client.site_id().to_string(),
                        self.site_display_label(self.client.site_id()),
                        self.client.clone(),
                    )]
                } else {
                    enabled
                        .into_iter()
                        .map(|(s, c)| (s.id.clone(), s.display_name().to_string(), c))
                        .collect()
                }
            }
        };
        let show_pills = self.site_scope.is_all() && clients.len() > 1;
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let escaped = q.replace('"', "\\\"");
            let jql = format!("text ~ \"{escaped}\" ORDER BY updated DESC");
            let cql = format!("text ~ \"{escaped}\" AND type = page");
            let per_site: Vec<(Vec<SiteTaggedHit>, Option<String>)> =
                stream::iter(clients)
                    .map(|(site_id, site_label, client)| {
                        let jql = &jql;
                        let cql = &cql;
                        async move {
                            let pill = show_pills.then_some(site_label);
                            let (j_res, c_res) = tokio::join!(
                                client.search_issues_jql(jql, 25, None, None),
                                client.search_confluence_cql(cql, 25),
                            );
                            let mut hits = Vec::new();
                            let mut site_err = None;
                            match j_res {
                                Ok(page) => {
                                    for it in page.issues {
                                        hits.push((
                                            site_id.clone(),
                                            pill.clone(),
                                            UnifiedSearchHit {
                                                kind: crate::models::SearchHitKind::JiraIssue,
                                                id_or_key: it.key,
                                                container: it.project_key,
                                                title: it.summary,
                                                snippet: it.status.name.clone(),
                                                status_or_version: it.status.name,
                                                updated_at: it.updated_at,
                                                web_url: it.web_url,
                                            },
                                        ));
                                    }
                                }
                                Err(e) => {
                                    site_err = Some(e.to_string());
                                }
                            }
                            match c_res {
                                Ok(pages) => {
                                    for pg in pages {
                                        hits.push((
                                            site_id.clone(),
                                            pill.clone(),
                                            UnifiedSearchHit {
                                                kind: crate::models::SearchHitKind::ConfluencePage,
                                                id_or_key: pg.id,
                                                container: pg.space_key,
                                                title: pg.title,
                                                snippet: format!("v{}", pg.version_number),
                                                status_or_version: format!("v{}", pg.version_number),
                                                updated_at: pg.updated_at,
                                                web_url: pg.web_url,
                                            },
                                        ));
                                    }
                                }
                                Err(e) => {
                                    if site_err.is_none() {
                                        site_err = Some(e.to_string());
                                    }
                                }
                            }
                            (hits, site_err)
                        }
                    })
                    .buffered(sync::DEFAULT_MULTI_SITE_CONCURRENCY)
                    .collect()
                    .await;

            let mut combined: Vec<(String, Option<String>, UnifiedSearchHit)> = Vec::new();
            let mut err_msg = None;
            for (hits, site_err) in per_site {
                combined.extend(hits);
                if err_msg.is_none() {
                    err_msg = site_err;
                }
            }
            combined.sort_by(|a, b| b.2.updated_at.cmp(&a.2.updated_at));
            let res = if combined.is_empty() {
                if let Some(err) = err_msg {
                    Err(err)
                } else {
                    Ok((Vec::new(), Vec::new(), Vec::new()))
                }
            } else {
                let mut hits = Vec::with_capacity(combined.len());
                let mut site_ids = Vec::with_capacity(combined.len());
                let mut labels = Vec::with_capacity(combined.len());
                for (sid, lbl, hit) in combined {
                    site_ids.push(sid);
                    labels.push(lbl);
                    hits.push(hit);
                }
                Ok((hits, site_ids, labels))
            };
            let _ = tx.send(UiMsg::RemoteSearchLoaded {
                seq,
                query: q,
                hits: res,
            });
        });
    }

    /// Bounded, budgeted interactive background prefetch of uncached sibling Jira issues.
    /// Called ONLY on interactive TUI list/board loads (never by `sync.rs` polling loops).
    pub(crate) fn prefetch_uncached_issues(
        &self,
        site_id: &str,
        keys: &[String],
        skip_key: Option<&str>,
    ) {
        if keys.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (resolved_site, client) = self.site_id_and_client_for(Some(site_id));
        if client.site_url().trim().is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for key in keys {
            let trimmed = key.trim();
            if trimmed.is_empty()
                || skip_key.is_some_and(|sk| sk.eq_ignore_ascii_case(trimmed))
                || !seen.insert(trimmed.to_ascii_uppercase())
            {
                continue;
            }
            let cached_fresh = self
                .storage
                .get_issue_detail_for_site(&resolved_site, trimmed)
                .ok()
                .flatten()
                .is_some()
                && !self
                    .storage
                    .is_issue_detail_stale_for_site(&resolved_site, trimmed)
                    .unwrap_or(false);
            if !cached_fresh {
                candidates.push(trimmed.to_string());
                if candidates.len() >= INTERACTIVE_SIBLING_PREFETCH_LIMIT {
                    break;
                }
            }
        }
        if candidates.is_empty() {
            return;
        }
        let storage = self.storage.clone();
        tokio::spawn(async move {
            stream::iter(candidates)
                .map(|key| {
                    let client = client.clone();
                    let storage = storage.clone();
                    let site_id = resolved_site.clone();
                    async move {
                        if let Ok((mut detail, meta)) = client.get_issue_with_meta(&key).await {
                            let _ = storage.upsert_issue_detail_for_site_merging(
                                &site_id,
                                &mut detail,
                                !meta.remote_links_ok,
                            );
                        }
                    }
                })
                .buffer_unordered(INTERACTIVE_SIBLING_PREFETCH_CONCURRENCY)
                .for_each(|()| async {})
                .await;
        });
    }

    /// Bounded, budgeted interactive background prefetch of uncached sibling Confluence pages.
    /// Called ONLY on interactive TUI space/tree loads (never by `sync.rs` polling loops).
    pub(crate) fn prefetch_uncached_pages(
        &self,
        site_id: &str,
        page_ids: &[String],
        skip_page_id: Option<&str>,
    ) {
        if page_ids.is_empty() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (resolved_site, client) = self.site_id_and_client_for(Some(site_id));
        if client.confluence_root_url().trim().is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for pid in page_ids {
            let trimmed = pid.trim();
            if trimmed.is_empty()
                || skip_page_id.is_some_and(|sp| sp == trimmed)
                || !seen.insert(trimmed.to_string())
            {
                continue;
            }
            let cached_fresh = self
                .storage
                .get_page_detail_for_site(&resolved_site, trimmed)
                .ok()
                .flatten()
                .is_some()
                && !self
                    .storage
                    .is_page_detail_stale_for_site(&resolved_site, trimmed)
                    .unwrap_or(false);
            if !cached_fresh {
                candidates.push(trimmed.to_string());
                if candidates.len() >= INTERACTIVE_SIBLING_PREFETCH_LIMIT {
                    break;
                }
            }
        }
        if candidates.is_empty() {
            return;
        }
        let storage = self.storage.clone();
        tokio::spawn(async move {
            stream::iter(candidates)
                .map(|pid| {
                    let client = client.clone();
                    let storage = storage.clone();
                    let site_id = resolved_site.clone();
                    async move {
                        if let Ok((mut detail, meta)) = client
                            .get_page_with_meta_in_cache(&pid, &site_id, &storage)
                            .await
                        {
                            let _ = storage.upsert_page_detail_for_site_with_meta(
                                &site_id,
                                &mut detail,
                                meta,
                            );
                        }
                    }
                })
                .buffer_unordered(INTERACTIVE_SIBLING_PREFETCH_CONCURRENCY)
                .for_each(|()| async {})
                .await;
        });
    }

    pub fn active_upload_target(&self, ui: &UiState) -> Option<UploadTarget> {
        self.active_upload_ref(ui).map(|(_, target)| target)
    }

    /// Upload target plus the site it lives on (the selected issue row's site,
    /// or the opened page's site on the Confluence tab).
    pub fn active_upload_ref(&self, ui: &UiState) -> Option<(Option<String>, UploadTarget)> {
        if ui.active_tab == WorkspaceTab::Confluence {
            if let Some(page) = &self.active_page {
                return Some((
                    self.active_page_site_id.clone(),
                    UploadTarget::Page(page.summary.id.clone()),
                ));
            }
        }
        self.selected_issue_ref(ui)
            .map(|(site, key)| (Some(site), UploadTarget::Issue(key)))
    }

    pub fn request_upload_attachment(&self, ui: &mut UiState, overlay: &PathInputOverlay) {
        let path = PathBuf::from(overlay.path.text.trim());
        self.request_upload_file(ui, overlay.site_id.as_deref(), overlay.target.clone(), path);
    }

    pub fn request_upload_file(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        target: UploadTarget,
        path: PathBuf,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Upload attachment");
        let explicit_site = site_id.or(match &target {
            UploadTarget::Issue(_) => self.active_issue_site_id.as_deref(),
            UploadTarget::Page(_) => self.active_page_site_id.as_deref(),
        });
        let (site_id, client) = self.site_id_and_client_for(explicit_site);
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        match target {
            UploadTarget::Issue(key) => {
                tokio::spawn(async move {
                    let res = match client.upload_issue_attachment(&key, &path).await {
                        Ok(atts) => {
                            let _ =
                                storage.append_issue_attachments_for_site(&site_id, &key, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        site_id,
                        issue_key: Some(key),
                        page_id: None,
                        result: res,
                    });
                });
            }
            UploadTarget::Page(id) => {
                tokio::spawn(async move {
                    let res = match client.upload_page_attachment(&id, &path).await {
                        Ok(atts) => {
                            let _ =
                                storage.append_page_attachments_for_site(&site_id, &id, &atts);
                            Ok(atts)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(UiMsg::AttachmentUploaded {
                        site_id,
                        issue_key: None,
                        page_id: Some(id),
                        result: res,
                    });
                });
            }
        }
    }

    pub fn request_download_attachment(&self, ui: &mut UiState, att: AttachmentItem) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let dest = self.config.resolved_download_dir();
        let task_label = format!("Download: {}", att.filename);
        ui.tasks.begin(&task_label);
        let explicit_site = self.current_attachments_site(ui);
        let (_, client) = self.site_id_and_client_for(explicit_site.as_deref());
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = client
                .download_attachment(&att, &dest)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::AttachmentDownloaded {
                task_label,
                result: res,
            });
        });
    }

    pub fn request_load_preview_image(&self, ui: &UiState) {
        let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
            return;
        };
        if ov.protocol.is_pixel() {
            // Pixel protocols (Kitty / iTerm2 / Sixel) are decoded and synchronized by `tui.rs`.
            return;
        }
        let Some(att) = ov.current().cloned() else {
            return;
        };
        let fit = ov.fit_box(ui.term_width, ui.term_height);
        let cache_key = ov.preview_image_cache_key(&att, ui.term_width, ui.term_height);
        if self.image_cache.contains_key(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let (_, client) = self.site_id_and_client_for(ov.site_id.as_deref());
        let tx = self.ui_tx.clone();
        let caps = ov.graphics_caps();
        let bg_rgb = img_preview::color_to_rgb(
            ui.theme.bg_elevated,
            img_preview::DEFAULT_PREVIEW_BG_RGB,
        );
        tokio::spawn(async move {
            match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => {
                    let mime = (!att.mime_type.trim().is_empty()).then_some(att.mime_type.as_str());
                    let fname = (!att.filename.trim().is_empty()).then_some(att.filename.as_str());
                    if bitefoundry_html::sniff_content_kind(mime, fname, Some(&bytes))
                        == bitefoundry_html::ContentKind::Html
                    {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send(UiMsg::PreviewImageSniffedAsHtml {
                            title: att.filename.clone(),
                            source_url: att.content_url.clone(),
                            html,
                        });
                        return;
                    }
                    let res = tokio::task::spawn_blocking(move || {
                        img_preview::decode_and_render_halfblocks_with_cell_px(
                            &bytes,
                            fit,
                            bg_rgb,
                            caps.cell_px,
                        )
                        .map_err(|e| e.to_string())
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("Image decode task failed: {e}")));
                    let _ = tx.send(UiMsg::ImageLoaded {
                        cache_key,
                        rendered: res,
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ImageLoaded {
                        cache_key,
                        rendered: Err(e.to_string()),
                    });
                }
            }
        });
    }

    /// Downloads an `.html`/`.htm`/`.md` attachment asynchronously and feeds it into `Overlay::HtmlPreview`.
    pub fn request_load_html_attachment(
        &self,
        ui: &mut UiState,
        site_id: Option<&str>,
        title: String,
        att: AttachmentItem,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (_, client) = self.site_id_and_client_for(site_id);
        let tx = self.ui_tx.clone();
        let source_url = att.content_url.clone();
        let mime_hint = att.mime_type.clone();
        let file_name = att.filename.clone();
        let is_md = crate::ui::overlays::html_preview::is_markdown_attachment(
            &att.mime_type,
            &att.filename,
        );
        let task_label = if is_md {
            format!("MD: {}", att.filename)
        } else {
            format!("HTML: {}", att.filename)
        };
        ui.tasks.begin(&task_label);
        tokio::spawn(async move {
            let mime = (!mime_hint.trim().is_empty()).then_some(mime_hint.as_str());
            let fname = (!file_name.trim().is_empty()).then_some(file_name.as_str());
            let (content_kind, result) = match client.fetch_attachment_bytes(&att).await {
                Ok(bytes) => {
                    let kind = bitefoundry_html::sniff_content_kind(mime, fname, Some(&bytes));
                    (kind, Ok(String::from_utf8_lossy(&bytes).into_owned()))
                }
                Err(err) => {
                    let kind = bitefoundry_html::sniff_content_kind(mime, fname, None);
                    (kind, Err(err.to_string()))
                }
            };
            let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                title,
                source_url,
                task_label,
                content_kind,
                result,
            });
        });
    }

    /// Lazily starts `NativeHtmlServer` and serves the given HTML document for external browser viewing.
    pub fn request_serve_html_in_browser(
        &self,
        ui: &mut UiState,
        title: String,
        html: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Async runtime unavailable for native HTML server");
            return;
        }
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        ui.info("Serving HTML locally…");
        tokio::spawn(async move {
            let res = server
                .serve_html(&title, &html)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::HtmlServed(res));
        });
    }

    pub fn request_check_update(&self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        ui.tasks.begin("Check for updates");
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = updater::check_or_apply_update(&repo, true, false)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::UpdateChecked(res));
        });
    }

}
