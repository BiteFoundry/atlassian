//! Action dispatcher for keyboard, mouse, overlays, and workspace tabs.

use crate::{
    config::{self, ImageProtocolPreference as ConfigImageProtocol},
    models::{CrossRefTarget, SearchHitKind},
    ui::{
        app::{AppState, PaneFocus, UiState, WorkspaceTab},
        hitmap::{ButtonId, HitTarget},
        keymap::Action,
        overlays::{
            self,
            attachments::{AttachmentCommand, AttachmentsOverlay},
            comment::{CommentOverlay, CommentTarget},
            context_menu::{ContextMenuAction, ContextMenuOverlay},
            cross_link::CrossLinkOverlay,
            file_picker::FilePickerOverlay,
            html_preview::{self, HtmlPreviewCommand, HtmlPreviewOverlay},
            image_preview::ImagePreviewOverlay,
            issue_form::IssueFormOverlay,
            links::{LinkCommand, LinksOverlay},
            page_form::PageFormOverlay,
            path_input::{PathInputOverlay, UploadTarget},
            settings::{SettingsCommand, SettingsOverlay},
            site_switcher::SiteSwitcherOverlay,
            Overlay, OverlayOutcome,
        },
    },
};

pub use crate::ui::keymap::Action as UiAction;

/// Dispatches an `Action`; returns `true` when the application should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if action == Action::Quit && !ui.has_overlay() && !ui.accepts_text_input() {
        return true;
    }

    // Handle active overlay first (except mouse clicks outside/on items).
    if !ui.overlays.is_empty() {
        match action {
            Action::MouseClick { row, col } => {
                match ui.hitmap.hit(row, col) {
                    Some(HitTarget::OverlayItem(idx)) => {
                        return handle_overlay_item_click(idx, app, ui);
                    }
                    Some(HitTarget::OverlayClose) => {
                        close_top_overlay_and_save_draft(app, ui);
                        return false;
                    }
                    Some(HitTarget::Button(btn)) => {
                        return dispatch_active_overlay(
                            crate::ui::event::button_to_action(btn),
                            app,
                            ui,
                        );
                    }
                    Some(HitTarget::OverlayBody) => {
                        let cmds = if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                            ov.handle_mouse_click(row, col);
                            ov.take_pending_commands()
                        } else {
                            Vec::new()
                        };
                        apply_html_preview_commands(cmds, app, ui);
                        return false;
                    }
                    _ => {}
                }
                if matches!(
                    ui.overlays.last(),
                    Some(
                        Overlay::ContextMenu(_)
                            | Overlay::ImagePreview(_)
                            | Overlay::HtmlPreview(_)
                            | Overlay::Help
                    )
                ) {
                    ui.pop_overlay();
                }
                return false;
            }
            Action::MouseRightClick { row, col } => {
                if matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_))) {
                    ui.pop_overlay();
                    return handle_mouse_right_click(row, col, app, ui);
                }
                return false;
            }
            Action::PasteClipboard => {
                let image_target = match ui.overlays.last() {
                    Some(Overlay::Comment(ov)) => Some(match &ov.target {
                        CommentTarget::Issue(key) => UploadTarget::Issue(key.clone()),
                        CommentTarget::Page { page_id, .. } => UploadTarget::Page(page_id.clone()),
                    }),
                    Some(Overlay::IssueForm(ov)) => ov
                        .edit_key
                        .clone()
                        .map(UploadTarget::Issue)
                        .or_else(|| app.active_upload_target(ui)),
                    Some(Overlay::PageForm(ov)) => ov
                        .edit_page_id
                        .clone()
                        .map(UploadTarget::Page)
                        .or_else(|| app.active_upload_target(ui)),
                    _ => None,
                };
                if matches!(
                    ui.overlays.last(),
                    Some(Overlay::Comment(_) | Overlay::IssueForm(_) | Overlay::PageForm(_))
                ) && try_paste_clipboard_image(image_target, app, ui)
                {
                    return false;
                }
                if let Some(top) = ui.overlays.last_mut() {
                    if top.paste_from_clipboard() {
                        if let Overlay::Assignee(ov) = top {
                            let key = ov.project_or_issue_key.clone();
                            let query = ov.query.text.clone();
                            app.request_assignable_users(&key, &query);
                        }
                    }
                }
                return false;
            }
            other => return dispatch_active_overlay(other, app, ui),
        }
    }

    // Mouse click dispatch outside overlays.
    if let Action::MouseClick { row, col } = action {
        return handle_mouse_click(row, col, app, ui);
    }
    if let Action::MouseRightClick { row, col } = action {
        return handle_mouse_right_click(row, col, app, ui);
    }

    // Inline text bar input (JQL / Local Filter / Unified Search).
    // Allow mouse wheel / scroll actions to pass through so results still scroll while typing.
    if ui.accepts_text_input()
        && !matches!(
            action,
            Action::MouseScroll { .. } | Action::ScrollUp | Action::ScrollDown
        )
    {
        return handle_inline_text_action(action, app, ui);
    }

    match action {
        Action::Quit => return true,
        Action::SwitchTab(idx) => {
            switch_tab(WorkspaceTab::from_index(idx), app, ui);
        }
        Action::OpenCommandPalette => {
            ui.push_overlay(Overlay::CommandPalette(
                crate::ui::overlays::command_palette::CommandPalette::new(),
            ));
        }
        Action::OpenSiteSwitcher => {
            let entries = app.build_site_switcher_entries();
            ui.push_overlay(Overlay::SiteSwitcher(SiteSwitcherOverlay::new(entries)));
        }
        Action::DiscoverCloudSites => {
            app.request_discover_cloud_sites(ui);
        }
        Action::OpenSettings => {
            ui.push_overlay(Overlay::Settings(SettingsOverlay::from_config(app, ui)));
        }
        Action::OpenContextMenu => {
            let anchor = (ui.term_width / 3, ui.term_height / 3);
            open_context_menu_at(anchor, app, ui);
        }
        Action::OpenAttachments => {
            open_attachments(app, ui, None);
        }
        Action::OpenHelp => {
            ui.push_overlay(Overlay::Help);
        }
        Action::SyncAll => {
            app.request_multi_site_sync(ui);
            if ui.active_tab == WorkspaceTab::MyWork {
                app.request_my_work_refresh(ui);
            }
        }
        Action::CheckUpdate => {
            app.request_check_update(ui);
            ui.info("Checking GitHub releases for updates…");
        }
        Action::ToggleNotify => {
            apply_settings_command(SettingsCommand::ToggleNotifications, app, ui);
        }
        Action::ToggleDensity => {
            apply_settings_command(SettingsCommand::ToggleViewDensity, app, ui);
        }
        Action::CycleTheme => {
            apply_settings_command(SettingsCommand::CycleColorTheme, app, ui);
        }
        Action::CycleImageProtocol => {
            apply_settings_command(SettingsCommand::CycleImageProtocol, app, ui);
        }
        Action::ToggleInlineImages => {
            apply_settings_command(SettingsCommand::ToggleInlineImages, app, ui);
        }
        Action::ToggleNavRail => {
            apply_settings_command(SettingsCommand::ToggleLeftSidebar, app, ui);
        }
        Action::PopNavHistory => {
            if !app.pop_nav_history(ui) {
                ui.info("No previous navigation history");
            }
        }
        Action::OpenCrossLinks => {
            let (label, targets) = app.collect_cross_links(ui.active_tab);
            ui.push_overlay(Overlay::CrossLink(CrossLinkOverlay::new(label, targets)));
        }
        Action::OpenLinks => {
            ui.push_overlay(Overlay::Links(LinksOverlay::from_app(app, ui)));
        }
        Action::ToggleHrefDisplay => {
            ui.show_hrefs = !ui.show_hrefs;
            ui.info(if ui.show_hrefs {
                "Showing inline link URLs (href: On)"
            } else {
                "Hiding inline link URLs (href: Off)"
            });
        }
        Action::OpenFilePicker => open_file_picker(app, ui),
        Action::PasteClipboard => {
            let target = app.active_upload_target(ui);
            if !try_paste_clipboard_image(target, app, ui) {
                ui.info("No image in clipboard to attach");
            }
        }
        Action::FocusNextPane => cycle_focus(ui, true),
        Action::FocusPrevPane => cycle_focus(ui, false),

        // Jira & Confluence Domain Actions
        Action::OpenTransition => open_transition(app, ui),
        Action::AssignToMe => {
            if let Some(key) = app.selected_issue_key(ui) {
                app.request_assign_to_me(ui, &key);
            } else {
                ui.info("No Jira issue selected");
            }
        }
        Action::OpenAssignee => {
            if let Some(key) = app.selected_issue_key(ui) {
                let proj = app
                    .active_issue
                    .as_ref()
                    .map(|d| d.summary.project_key.as_str())
                    .unwrap_or("");
                app.request_open_assignee_picker(ui, &key, proj);
            } else {
                ui.info("No Jira issue selected");
            }
        }
        Action::OpenComment => open_comment(app, ui),
        Action::NewItem => open_new_item(app, ui),
        Action::EditItem => open_edit_item(app, ui),
        Action::UploadAttachment => open_upload_attachment(app, ui),
        Action::DownloadAttachment => download_first_attachment(app, ui),
        Action::OpenImagePreview => open_image_preview(app, ui),
        Action::OpenHtmlPreview => open_html_preview(app, ui),
        Action::OpenInBrowser => open_in_browser(app, ui),
        Action::FocusJql => {
            if ui.active_tab != WorkspaceTab::Issues {
                ui.active_tab = WorkspaceTab::Issues;
            }
            ui.focus = PaneFocus::JqlBar;
            ui.issue_ui.local_filter_active = false;
        }
        Action::FocusFilter => match ui.active_tab {
            WorkspaceTab::Issues => {
                ui.issue_ui.local_filter_active = true;
            }
            WorkspaceTab::Search => {
                ui.focus = PaneFocus::SearchBar;
            }
            _ => {
                ui.active_tab = WorkspaceTab::Search;
                ui.focus = PaneFocus::SearchBar;
            }
        },
        Action::RemoteSearch => {
            ui.active_tab = WorkspaceTab::Search;
            let q = ui.search_ui.query.text.clone();
            app.request_remote_search(ui, &q);
        }
        Action::ToggleDrawerOrToc => match ui.active_tab {
            WorkspaceTab::Board => {
                ui.board_ui.drawer_open = !ui.board_ui.drawer_open;
            }
            WorkspaceTab::Confluence => {
                ui.confluence_ui.toc_open = !ui.confluence_ui.toc_open;
            }
            _ => {
                ui.auto_detail_drawer = !ui.auto_detail_drawer;
            }
        },
        Action::TogglePin => toggle_pin(app, ui),

        // Navigation & scrolling
        Action::MoveUp => move_direction(app, ui, -1, 0),
        Action::MoveDown => move_direction(app, ui, 1, 0),
        Action::MoveLeft => move_direction(app, ui, 0, -1),
        Action::MoveRight => move_direction(app, ui, 0, 1),
        Action::ScrollUp | Action::PageUp => scroll_active_pane(app, ui, -6),
        Action::ScrollDown | Action::PageDown => scroll_active_pane(app, ui, 6),
        Action::MouseScroll { delta, row, col } => handle_mouse_scroll(delta, row, col, app, ui),
        Action::GoTop => scroll_active_pane(app, ui, -10000),
        Action::GoBottom => scroll_active_pane(app, ui, 10000),
        Action::Activate => activate_selection(app, ui),
        Action::ExpandOrToggle => {
            if ui.active_tab == WorkspaceTab::Confluence {
                let tree = app.current_confluence_tree(ui);
                if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                    let page = row.page.clone();
                    app.toggle_expand_page(ui, &page);
                }
            }
        }
        Action::CollapseNode => {
            if ui.active_tab == WorkspaceTab::Confluence {
                let tree = app.current_confluence_tree(ui);
                if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                    if row.expanded {
                        app.expanded_pages.remove(&row.page.id);
                    } else if let Some(pid) = &row.page.parent_id {
                        if let Some(parent_idx) = tree.iter().position(|r| &r.page.id == pid) {
                            ui.confluence_ui.selected_tree_row = parent_idx;
                        }
                    }
                }
            } else {
                move_direction(app, ui, 0, -1);
            }
        }
        _ => {}
    }

    false
}

fn switch_tab(tab: WorkspaceTab, app: &mut AppState, ui: &mut UiState) {
    ui.active_tab = tab;
    ui.issue_ui.local_filter_active = false;
    match tab {
        WorkspaceTab::MyWork => {
            ui.focus = PaneFocus::Main;
            if app.my_issues.is_empty() && app.my_pages.is_empty() {
                app.request_my_work_refresh(ui);
            }
        }
        WorkspaceTab::Search => {
            ui.focus = PaneFocus::SearchBar;
            app.refresh_local_search(&ui.search_ui.query.text);
        }
        WorkspaceTab::Confluence => {
            ui.focus = PaneFocus::Nav;
            if let Some(sp) = app.active_space(ui).cloned() {
                if !app.root_pages_by_space.contains_key(&sp.key) {
                    app.request_space_root_pages(ui, &sp.key);
                }
            }
        }
        _ => {
            ui.focus = PaneFocus::Main;
        }
    }
}

fn cycle_focus(ui: &mut UiState, forward: bool) {
    let order = match ui.active_tab {
        WorkspaceTab::Board => vec![PaneFocus::Nav, PaneFocus::Main, PaneFocus::Detail],
        WorkspaceTab::Issues => vec![PaneFocus::JqlBar, PaneFocus::Nav, PaneFocus::Main],
        WorkspaceTab::Confluence => {
            if ui.confluence_ui.toc_open {
                vec![PaneFocus::Nav, PaneFocus::Main, PaneFocus::Detail]
            } else {
                vec![PaneFocus::Nav, PaneFocus::Main]
            }
        }
        WorkspaceTab::Search => vec![PaneFocus::SearchBar, PaneFocus::Main],
        _ => vec![PaneFocus::Main],
    };
    let pos = order.iter().position(|f| *f == ui.focus).unwrap_or(0);
    let next = if forward {
        (pos + 1) % order.len()
    } else {
        (pos + order.len() - 1) % order.len()
    };
    ui.focus = order[next];
}

fn handle_inline_text_action(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if action == Action::CloseOverlay {
        ui.issue_ui.local_filter_active = false;
        ui.focus = PaneFocus::Main;
        return false;
    }

    if action == Action::PasteClipboard {
        if let Some(text) = overlays::read_clipboard_text() {
            let cleaned = overlays::sanitize_single_line(&text);
            if !cleaned.is_empty() {
                return handle_inline_text_action(Action::Paste(cleaned), app, ui);
            }
        }
        return false;
    }

    if ui.issue_ui.local_filter_active {
        let buf = &mut ui.issue_ui.local_filter;
        match action {
            Action::Input(ch) => buf.insert_char(ch),
            Action::Backspace => buf.backspace(),
            Action::Delete => buf.delete(),
            Action::CursorLeft => buf.move_left(),
            Action::CursorRight => buf.move_right(),
            Action::CursorHome => buf.move_home(),
            Action::CursorEnd => buf.move_end(),
            Action::Paste(s) => buf.insert_str(&s),
            Action::Submit => {
                ui.issue_ui.local_filter_active = false;
                ui.focus = PaneFocus::Nav;
            }
            _ => {}
        }
        ui.issue_ui.selected_issue = 0;
        return false;
    }

    match ui.focus {
        PaneFocus::JqlBar => {
            let buf = &mut ui.issue_ui.jql;
            match action {
                Action::Input(ch) => buf.insert_char(ch),
                Action::Backspace => buf.backspace(),
                Action::Delete => buf.delete(),
                Action::CursorLeft => buf.move_left(),
                Action::CursorRight => buf.move_right(),
                Action::CursorHome => buf.move_home(),
                Action::CursorEnd => buf.move_end(),
                Action::Paste(s) => buf.insert_str(&s),
                Action::Submit => {
                    let jql = buf.text.trim().to_string();
                    if !jql.is_empty() {
                        app.request_jql_search(ui, jql, "Custom JQL".to_string());
                    }
                    ui.focus = PaneFocus::Main;
                }
                _ => {}
            }
        }
        PaneFocus::SearchBar => {
            let buf = &mut ui.search_ui.query;
            match action {
                Action::Input(ch) => {
                    buf.insert_char(ch);
                    ui.search_ui.is_remote = false;
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                    ui.search_ui.selected_hit = 0;
                }
                Action::Backspace => {
                    buf.backspace();
                    ui.search_ui.is_remote = false;
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                    ui.search_ui.selected_hit = 0;
                }
                Action::Delete => {
                    buf.delete();
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                }
                Action::CursorLeft => buf.move_left(),
                Action::CursorRight => buf.move_right(),
                Action::CursorHome => buf.move_home(),
                Action::CursorEnd => buf.move_end(),
                Action::Paste(s) => {
                    buf.insert_str(&s);
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                }
                Action::RemoteSearch => {
                    let q = buf.text.clone();
                    app.request_remote_search(ui, &q);
                }
                Action::Submit => {
                    ui.focus = PaneFocus::Main;
                    activate_selection(app, ui);
                }
                Action::CursorDown => {
                    ui.focus = PaneFocus::Main;
                }
                _ => {}
            }
        }
        _ => {}
    }
    false
}

fn dispatch_active_overlay(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    let outcome = ui.overlays.last_mut().map(|o| o.handle(&action));

    // Intercept SiteSwitcherOverlay pending commands so active_site / enabled / discover / sync
    // are persisted and reflected across federated views.
    if let Some(Overlay::SiteSwitcher(ov)) = ui.overlays.last_mut() {
        if let Some(cmd) = ov.take_pending_command() {
            if matches!(outcome, Some(OverlayOutcome::Close)) {
                ui.pop_overlay();
            }
            app.apply_site_switcher_command(cmd, ui);
            return false;
        }
    }

    // Intercept SettingsOverlay pending commands so config.toml and live UiState stay in sync.
    if let Some(Overlay::Settings(ov)) = ui.overlays.last_mut() {
        if let Some(cmd) = ov.pending_command.take() {
            if cmd == SettingsCommand::OpenKeyboardShortcuts {
                ui.pop_overlay();
                ui.push_overlay(Overlay::Help);
            } else {
                apply_settings_command(cmd, app, ui);
            }
            return false;
        }
    }

    // Intercept LinksOverlay pending commands (open in browser / copy URL).
    if let Some(Overlay::Links(ov)) = ui.overlays.last_mut() {
        if let Some(cmd) = ov.take_pending_command() {
            if matches!(outcome, Some(OverlayOutcome::Close)) {
                ui.pop_overlay();
            }
            match cmd {
                LinkCommand::Open(url) => {
                    if let Err(err) = open::that(&url) {
                        ui.error(format!("Failed to open browser: {err}"));
                    }
                }
                LinkCommand::Copy(url) => match overlays::copy_to_clipboard(&url) {
                    Ok(()) => ui.success(format!("Copied URL: {url}")),
                    Err(err) => ui.error(err),
                },
            }
            return false;
        }
    }

    // Intercept AttachmentsOverlay pending commands before popping so selected index/URL is preserved.
    if let Some(Overlay::Attachments(ov)) = ui.overlays.last_mut() {
        if let Some(cmd) = ov.pending_command.take() {
            match cmd {
                AttachmentCommand::PreviewImage(idx) => {
                    let owner = ov.owner_label.clone();
                    let items = ov.items.clone();
                    ui.pop_overlay();
                    open_image_preview_with(app, ui, owner, &items, Some(idx));
                }
                AttachmentCommand::PreviewHtml(idx) => {
                    let owner = ov.owner_label.clone();
                    let att = ov.items.get(idx).cloned();
                    ui.pop_overlay();
                    if let Some(att) = att {
                        open_html_attachment_preview(app, ui, owner, att);
                    }
                }
                AttachmentCommand::Download(idx) => {
                    if let Some(att) = ov.items.get(idx).cloned() {
                        app.request_download_attachment(ui, att);
                    }
                }
                AttachmentCommand::Upload => {
                    ui.pop_overlay();
                    open_upload_attachment(app, ui);
                }
                AttachmentCommand::Refresh => {
                    app.request_attachments_refresh(ui);
                }
                AttachmentCommand::OpenUrl(url) => {
                    if let Err(err) = open::that(&url) {
                        ui.error(format!("Failed to open browser: {err}"));
                    }
                }
            }
            return false;
        }
    }

    // Drain HtmlPreviewOverlay side-effect commands (native browser serving, links, alerts/toasts).
    if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
        let cmds = ov.take_pending_commands();
        if !cmds.is_empty() {
            apply_html_preview_commands(cmds, app, ui);
        }
    }

    if let Some(out) = outcome {
        match out {
            OverlayOutcome::Handled => return false,
            OverlayOutcome::Close => {
                close_top_overlay_and_save_draft(app, ui);
                return false;
            }
            OverlayOutcome::Dispatch(sub) => {
                return handle_overlay_dispatch(sub, app, ui);
            }
            OverlayOutcome::CloseAndDispatch(sub) => {
                close_top_overlay_and_save_draft(app, ui);
                return dispatch(sub, app, ui);
            }
        }
    }
    false
}

fn close_top_overlay_and_save_draft(app: &mut AppState, ui: &mut UiState) {
    let Some(top) = ui.overlays.pop() else {
        return;
    };
    match top {
        Overlay::Comment(ov) => {
            let key = ov.draft_key();
            if ov.has_unsent_content() {
                app.drafts.set(key, ov.draft_text());
                let _ = app.save_drafts();
                ui.info("Saved comment draft");
            } else if app.drafts.remove(&key).is_some() {
                let _ = app.save_drafts();
            }
        }
        Overlay::IssueForm(ov) => {
            let key = ov.draft_key();
            if ov.has_unsent_content() {
                app.drafts.set(key, ov.to_draft_json());
                let _ = app.save_drafts();
                ui.info("Saved issue draft");
            } else if app.drafts.remove(&key).is_some() {
                let _ = app.save_drafts();
            }
        }
        Overlay::PageForm(ov) => {
            let key = ov.draft_key();
            if ov.has_unsent_content() {
                app.drafts.set(key, ov.to_draft_json());
                let _ = app.save_drafts();
                ui.info("Saved page draft");
            } else if app.drafts.remove(&key).is_some() {
                let _ = app.save_drafts();
            }
        }
        _ => {}
    }
}

fn sync_open_settings_overlay(app: &AppState, ui: &mut UiState) {
    let caps = ui.graphics_caps;
    for overlay in &mut ui.overlays {
        if let Overlay::Settings(ov) = overlay {
            ov.sync_from_caps(app, caps);
        }
    }
}

pub fn apply_settings_command(cmd: SettingsCommand, app: &mut AppState, ui: &mut UiState) {
    let mut mutated = false;
    match cmd {
        SettingsCommand::ToggleNotifications => {
            app.config.notify = !app.config.notify;
            app.sync_ui_from_config(ui);
            mutated = true;
            ui.info(format!(
                "Desktop notifications: {}",
                if app.config.notify { "On" } else { "Off" }
            ));
        }
        SettingsCommand::ToggleLeftSidebar => {
            app.config.show_nav_rail = !app.config.show_nav_rail;
            app.sync_ui_from_config(ui);
            mutated = true;
            ui.info(format!(
                "Left sidebar: {}",
                if ui.show_nav_rail { "Shown" } else { "Hidden" }
            ));
        }
        SettingsCommand::ToggleInlineImages => {
            app.config.inline_images = !app.config.inline_images;
            app.sync_ui_from_config(ui);
            mutated = true;
            ui.info(format!(
                "Inline image previews: {}",
                if ui.inline_images { "On" } else { "Off" }
            ));
        }
        SettingsCommand::CycleImageProtocol => {
            app.config.image_protocol = app.config.image_protocol.next();
            app.sync_ui_from_config(ui);
            mutated = true;
            let detail = match app.config.image_protocol {
                ConfigImageProtocol::Auto => {
                    format!("Auto ({})", ui.graphics_caps.protocol.label())
                }
                explicit => explicit.label().to_string(),
            };
            ui.info(format!("Image protocol: {detail}"));
        }
        SettingsCommand::ToggleViewDensity => {
            app.config.density = app.config.density.toggle();
            app.sync_ui_from_config(ui);
            mutated = true;
            ui.info(format!("View density: {}", app.config.density.label()));
        }
        SettingsCommand::CycleColorTheme => {
            app.config.theme = app.config.theme.next();
            app.sync_ui_from_config(ui);
            mutated = true;
            ui.info(format!("Color theme: {}", app.config.theme.label()));
        }
        SettingsCommand::ShowDownloadFolder => {
            ui.info(format!(
                "Download folder: {}",
                app.config.resolved_download_dir().display()
            ));
        }
        SettingsCommand::OpenKeyboardShortcuts => {
            ui.push_overlay(Overlay::Help);
        }
        SettingsCommand::ShowSiteUrl => {
            ui.info(format!("Site URL: {}", app.site_display()));
        }
        SettingsCommand::ShowDeploymentKind => {
            ui.info(format!("Deployment: {}", app.config.deployment.as_str()));
        }
        SettingsCommand::ShowConfigFilePath => {
            ui.info(format!(
                "Config file: {}",
                config::config_file_path().display()
            ));
        }
    }

    if mutated {
        if let Err(err) = app.save_config() {
            ui.error(format!("Failed to save config.toml: {err}"));
        }
        sync_open_settings_overlay(app, ui);
    }
}

fn handle_overlay_dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    match action {
        Action::ExternalEditor => {
            if let Some(top) = ui.overlays.last() {
                match top {
                    Overlay::IssueForm(f) => ui.editor_request = Some(f.editor_target()),
                    Overlay::PageForm(f) => ui.editor_request = Some(f.editor_target()),
                    Overlay::Comment(c) => ui.editor_request = Some(c.editor_target()),
                    _ => {}
                }
            }
        }
        Action::RemoteSearch => {
            if let Some(Overlay::Assignee(ov)) = ui.overlays.last() {
                app.request_assignable_users(&ov.project_or_issue_key, &ov.query.text);
            }
        }
        Action::LoadPreviewImage => {
            app.request_load_preview_image(ui);
        }
        Action::DownloadPreviewImage => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(att) = ov.current().cloned() {
                    app.request_download_attachment(ui, att);
                }
            }
        }
        Action::OpenInBrowser => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(att) = ov.current() {
                    if !att.content_url.is_empty() {
                        if let Err(err) = open::that(&att.content_url) {
                            ui.error(format!("Failed to open browser: {err}"));
                        }
                    }
                }
            }
        }
        Action::Submit => {
            let Some(top) = ui.overlays.pop() else {
                return false;
            };
            match top {
                Overlay::Transition(ov) => {
                    if let Some(tr) = ov.selected_transition().cloned() {
                        app.request_transition_issue(ui, &ov.issue_key, tr);
                    }
                }
                Overlay::Assignee(ov) => {
                    if let Some(choice) = ov.selected_assignee() {
                        app.request_assign_user(ui, &ov.issue_key, choice.cloned());
                    }
                }
                Overlay::IssueForm(ov) => {
                    let draft_key = ov.draft_key();
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.to_draft_json());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_issue_form(ui, &ov, Some(draft_key));
                }
                Overlay::PageForm(ov) => {
                    let draft_key = ov.draft_key();
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.to_draft_json());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_page_form(ui, &ov, Some(draft_key));
                }
                Overlay::Comment(ov) => {
                    let draft_key = ov.draft_key();
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.draft_text());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_comment(ui, &ov, Some(draft_key));
                }
                Overlay::CrossLink(ov) => {
                    if let Some(target) = ov.selected_target().cloned() {
                        app.jump_to_cross_ref(ui, target);
                    }
                }
                Overlay::PathInput(ov) => {
                    app.request_upload_attachment(ui, &ov);
                }
                Overlay::FilePicker(ov) => {
                    if let Some(path) = ov.selected_file {
                        let fname = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file")
                            .to_string();
                        app.request_upload_file(ui, ov.target, path);
                        ui.info(format!("Uploading {fname}…"));
                    }
                }
                Overlay::ContextMenu(ov) => {
                    if let Some(cmd) = ov.selected_command().cloned() {
                        match cmd {
                            ContextMenuAction::Action(act) => return dispatch(act, app, ui),
                            ContextMenuAction::OpenAttachments => {
                                open_attachments(app, ui, None);
                            }
                            ContextMenuAction::OpenSettings => {
                                ui.push_overlay(Overlay::Settings(SettingsOverlay::from_config(
                                    app, ui,
                                )));
                            }
                            ContextMenuAction::CopyText { label, value } => {
                                if value.trim().is_empty() {
                                    ui.info(format!("{label} is empty"));
                                } else {
                                    match overlays::copy_to_clipboard(&value) {
                                        Ok(()) => ui.success(format!("Copied {label}: {value}")),
                                        Err(err) => ui.error(err),
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        other => return dispatch(other, app, ui),
    }
    false
}

fn handle_overlay_item_click(idx: usize, app: &mut AppState, ui: &mut UiState) -> bool {
    if let Some(top) = ui.overlays.last_mut() {
        top.select(idx);
    }
    dispatch_active_overlay(Action::Activate, app, ui)
}

fn handle_mouse_right_click(row: u16, col: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    if let Some(target) = ui.hitmap.hit(row, col) {
        match target {
            HitTarget::KanbanCard { col: c, row: r } => {
                ui.focus = PaneFocus::Main;
                ui.board_ui.selected_col = c;
                ui.board_ui.selected_row = r;
                let site_id = app.active_board_site_id(ui).map(str::to_string);
                if let Some(issue) = app.selected_board_issue(ui) {
                    app.request_issue_detail_for_site(ui, site_id.as_deref(), &issue.key);
                }
            }
            HitTarget::IssueRow(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.issue_ui.nav_section = 1;
                ui.issue_ui.selected_issue = idx;
                let list = app.filtered_issues_with_sites(ui);
                if let Some((sid, it)) = list.get(idx) {
                    app.request_issue_detail_for_site(ui, Some(sid.as_str()), &it.key);
                }
            }
            HitTarget::MyWorkIssue(idx) => {
                ui.my_work_ui.active_column = 0;
                ui.my_work_ui.selected_issue = idx;
            }
            HitTarget::MyWorkPage(idx) => {
                ui.my_work_ui.active_column = 1;
                ui.my_work_ui.selected_page = idx;
            }
            HitTarget::TreeRow(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.confluence_ui.nav_section = 1;
                ui.confluence_ui.selected_tree_row = idx;
                let site_id = app.active_space_site_id(ui).map(str::to_string);
                let tree = app.current_confluence_tree(ui);
                if let Some(row_item) = tree.get(idx) {
                    let page_id = row_item.page.id.clone();
                    app.request_page_detail_for_site(ui, site_id.as_deref(), &page_id);
                }
            }
            _ => {}
        }
    }
    open_context_menu_at((col, row), app, ui);
    false
}

fn open_context_menu_at(anchor: (u16, u16), app: &AppState, ui: &mut UiState) {
    let menu = ContextMenuOverlay::for_active_context(anchor, app, ui);
    ui.push_overlay(Overlay::ContextMenu(menu));
}

fn open_attachments(app: &mut AppState, ui: &mut UiState, preferred_idx: Option<usize>) {
    let mut ov = AttachmentsOverlay::from_app(app, ui);
    if let Some(idx) = preferred_idx {
        ov.select(idx);
    }
    ui.push_overlay(Overlay::Attachments(ov));
    app.request_attachments_refresh(ui);
}

fn handle_mouse_click(row: u16, col: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    let Some(target) = ui.hitmap.hit(row, col) else {
        return false;
    };
    match target {
        HitTarget::Tab(idx) => switch_tab(WorkspaceTab::from_index(idx), app, ui),
        HitTarget::BoardItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 0;
            ui.board_ui.selected_board = idx;
            if let Some(b) = app.boards.get(idx) {
                app.request_board_load(ui, b.id, None);
            }
        }
        HitTarget::SprintItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 1;
            ui.board_ui.selected_sprint = idx;
            if let (Some(b), Some(sp)) = (app.active_board(ui), app.sprints.get(idx)) {
                app.request_board_load(ui, b.id, Some(sp.id));
            }
        }
        HitTarget::KanbanCard { col: c, row: r } => {
            ui.focus = PaneFocus::Main;
            ui.board_ui.selected_col = c;
            ui.board_ui.selected_row = r;
            ui.board_ui.drawer_open = true;
            let site_id = app.active_board_site_id(ui).map(str::to_string);
            if let Some(issue) = app.selected_board_issue(ui) {
                app.request_issue_detail_for_site(ui, site_id.as_deref(), &issue.key);
            }
        }
        HitTarget::FilterItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.issue_ui.nav_section = 0;
            ui.issue_ui.selected_filter = idx;
            if let Some(f) = app.saved_filters.get(idx).cloned() {
                ui.issue_ui.jql.set_text(f.jql.clone());
                app.request_jql_search(ui, f.jql, f.name);
            }
        }
        HitTarget::IssueRow(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.issue_ui.nav_section = 1;
            ui.issue_ui.selected_issue = idx;
            let list = app.filtered_issues_with_sites(ui);
            if let Some((sid, it)) = list.get(idx) {
                app.request_issue_detail_for_site(ui, Some(sid.as_str()), &it.key);
            }
        }
        HitTarget::MyWorkIssue(idx) => {
            ui.my_work_ui.active_column = 0;
            ui.my_work_ui.selected_issue = idx;
            let site_id = app.my_issue_site_ids.get(idx).cloned();
            if let Some(it) = app.my_issues.get(idx).cloned() {
                app.jump_to_cross_ref_for_site(
                    ui,
                    site_id.as_deref(),
                    CrossRefTarget::JiraIssue {
                        key: it.key,
                        label: Some(it.summary),
                    },
                );
            }
        }
        HitTarget::MyWorkPage(idx) => {
            ui.my_work_ui.active_column = 1;
            ui.my_work_ui.selected_page = idx;
            let site_id = app.my_page_site_ids.get(idx).cloned();
            if let Some(pg) = app.my_pages.get(idx).cloned() {
                app.jump_to_cross_ref_for_site(
                    ui,
                    site_id.as_deref(),
                    CrossRefTarget::ConfluencePage {
                        page_id: pg.id,
                        title: Some(pg.title),
                    },
                );
            }
        }
        HitTarget::SpaceItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 0;
            ui.confluence_ui.selected_space = idx;
            ui.confluence_ui.selected_tree_row = 0;
            let site_id = app.space_site_ids.get(idx).cloned();
            if let Some(sp) = app.spaces.get(idx).cloned() {
                app.request_space_root_pages_for_site(ui, site_id.as_deref(), &sp.key);
            }
        }
        HitTarget::TreeRow(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 1;
            ui.confluence_ui.selected_tree_row = idx;
            let site_id = app.active_space_site_id(ui).map(str::to_string);
            let tree = app.current_confluence_tree(ui);
            if let Some(row) = tree.get(idx) {
                let page = row.page.clone();
                app.request_page_detail_for_site(ui, site_id.as_deref(), &page.id);
                if row.has_children {
                    app.toggle_expand_page(ui, &page);
                }
            }
        }
        HitTarget::TocItem(idx) => {
            ui.confluence_ui.selected_toc = idx;
            if let Some(page) = &app.active_page {
                if let Some(entry) = page.toc.iter().filter(|t| t.level <= 4).nth(idx) {
                    ui.confluence_ui.reader_scroll = entry.line_hint as u16;
                }
            }
        }
        HitTarget::SearchHit(idx) => {
            ui.search_ui.selected_hit = idx;
            activate_selection(app, ui);
        }
        HitTarget::DirectoryProject(idx) => {
            ui.directory_ui.section = 0;
            ui.directory_ui.selected_project = idx;
        }
        HitTarget::DirectorySpace(idx) => {
            ui.directory_ui.section = 1;
            ui.directory_ui.selected_space = idx;
        }
        HitTarget::JqlBar => {
            ui.focus = PaneFocus::JqlBar;
            ui.issue_ui.local_filter_active = false;
        }
        HitTarget::SearchBar => {
            ui.focus = PaneFocus::SearchBar;
        }
        HitTarget::DetailPane => {
            ui.focus = PaneFocus::Main;
        }
        HitTarget::NavPane => {
            ui.focus = PaneFocus::Nav;
        }
        HitTarget::Attachment(idx) => open_attachments(app, ui, Some(idx)),
        HitTarget::SiteGroupHeader(idx) => {
            if let Some(key) = ui.site_group_keys.get(idx).cloned() {
                if !ui.collapsed_site_groups.remove(&key) {
                    ui.collapsed_site_groups.insert(key);
                }
            }
        }
        HitTarget::Button(btn) => {
            if btn != ButtonId::FocusJql {
                ui.issue_ui.local_filter_active = false;
                if matches!(ui.focus, PaneFocus::JqlBar | PaneFocus::SearchBar) {
                    ui.focus = PaneFocus::Main;
                }
            }
            return dispatch(crate::ui::event::button_to_action(btn), app, ui);
        }
        HitTarget::OverlayItem(_) | HitTarget::OverlayBody | HitTarget::OverlayClose => {}
    }
    false
}

fn move_direction(app: &mut AppState, ui: &mut UiState, dy: i32, dx: i32) {
    match ui.active_tab {
        WorkspaceTab::Board => match ui.focus {
            PaneFocus::Nav => {
                if dy != 0 {
                    if ui.board_ui.nav_section == 0 {
                        let len = app.boards.len();
                        ui.board_ui.selected_board = step_index(ui.board_ui.selected_board, dy, len);
                    } else {
                        let len = app.sprints.len();
                        ui.board_ui.selected_sprint = step_index(ui.board_ui.selected_sprint, dy, len);
                    }
                } else if dx != 0 {
                    ui.board_ui.nav_section = if ui.board_ui.nav_section == 0 { 1 } else { 0 };
                }
            }
            PaneFocus::Detail => {
                if dy != 0 {
                    ui.board_ui.detail_scroll =
                        apply_scroll(ui.board_ui.detail_scroll, dy * 2);
                }
            }
            _ => {
                let cols = app.build_kanban_columns(ui);
                if cols.is_empty() {
                    return;
                }
                if dx != 0 {
                    ui.board_ui.selected_col =
                        step_index(ui.board_ui.selected_col, dx, cols.len());
                    let col_len = cols[ui.board_ui.selected_col].issues.len();
                    ui.board_ui.selected_row =
                        ui.board_ui.selected_row.min(col_len.saturating_sub(1));
                }
                if dy != 0 {
                    let col_len = cols[ui.board_ui.selected_col].issues.len();
                    ui.board_ui.selected_row =
                        step_index(ui.board_ui.selected_row, dy, col_len);
                }
            }
        },
        WorkspaceTab::Issues => match ui.focus {
            PaneFocus::Nav => {
                if dy != 0 {
                    if ui.issue_ui.nav_section == 0 {
                        let len = app.saved_filters.len();
                        ui.issue_ui.selected_filter =
                            step_index(ui.issue_ui.selected_filter, dy, len);
                    } else {
                        let len = app.filtered_issues(ui).len();
                        ui.issue_ui.selected_issue =
                            step_index(ui.issue_ui.selected_issue, dy, len);
                    }
                } else if dx != 0 {
                    ui.issue_ui.nav_section = if ui.issue_ui.nav_section == 0 { 1 } else { 0 };
                }
            }
            _ => {
                if dy != 0 {
                    ui.issue_ui.detail_scroll = apply_scroll(ui.issue_ui.detail_scroll, dy * 2);
                }
            }
        },
        WorkspaceTab::MyWork => {
            if dx != 0 {
                ui.my_work_ui.active_column = if ui.my_work_ui.active_column == 0 { 1 } else { 0 };
            }
            if dy != 0 {
                if ui.my_work_ui.active_column == 0 {
                    let len = app.my_issues.len();
                    ui.my_work_ui.selected_issue =
                        step_index(ui.my_work_ui.selected_issue, dy, len);
                } else {
                    let len = app.my_pages.len();
                    ui.my_work_ui.selected_page =
                        step_index(ui.my_work_ui.selected_page, dy, len);
                }
            }
        }
        WorkspaceTab::Confluence => match ui.focus {
            PaneFocus::Nav => {
                if dy != 0 {
                    if ui.confluence_ui.nav_section == 0 {
                        let len = app.spaces.len();
                        ui.confluence_ui.selected_space =
                            step_index(ui.confluence_ui.selected_space, dy, len);
                    } else {
                        let len = app.current_confluence_tree(ui).len();
                        ui.confluence_ui.selected_tree_row =
                            step_index(ui.confluence_ui.selected_tree_row, dy, len);
                    }
                } else if dx != 0 {
                    ui.confluence_ui.nav_section =
                        if ui.confluence_ui.nav_section == 0 { 1 } else { 0 };
                }
            }
            PaneFocus::Detail => {
                if let Some(page) = &app.active_page {
                    let len = page.toc.iter().filter(|t| t.level <= 4).count();
                    ui.confluence_ui.selected_toc =
                        step_index(ui.confluence_ui.selected_toc, dy, len);
                }
            }
            _ => {
                if dy != 0 {
                    ui.confluence_ui.reader_scroll =
                        apply_scroll(ui.confluence_ui.reader_scroll, dy * 2);
                }
            }
        },
        WorkspaceTab::Search => {
            if dy != 0 {
                let len = app.search_hits.len();
                ui.search_ui.selected_hit = step_index(ui.search_ui.selected_hit, dy, len);
            }
        }
        WorkspaceTab::Directory => {
            if dx != 0 {
                ui.directory_ui.section = if ui.directory_ui.section == 0 { 1 } else { 0 };
            }
            if dy != 0 {
                if ui.directory_ui.section == 0 {
                    let len = app.projects.len();
                    ui.directory_ui.selected_project =
                        step_index(ui.directory_ui.selected_project, dy, len);
                } else {
                    let len = app.spaces.len();
                    ui.directory_ui.selected_space =
                        step_index(ui.directory_ui.selected_space, dy, len);
                }
            }
        }
    }
}

fn handle_mouse_scroll(delta: i32, row: u16, col: u16, app: &mut AppState, ui: &mut UiState) {
    let step = if delta < 0 { -1 } else { 1 };
    match ui.hitmap.hit(row, col) {
        Some(
            HitTarget::NavPane
            | HitTarget::BoardItem(_)
            | HitTarget::SprintItem(_)
            | HitTarget::FilterItem(_)
            | HitTarget::IssueRow(_)
            | HitTarget::SpaceItem(_)
            | HitTarget::TreeRow(_)
            | HitTarget::SiteGroupHeader(_),
        ) => {
            let prev_focus = ui.focus;
            ui.focus = PaneFocus::Nav;
            move_direction(app, ui, step, 0);
            ui.focus = prev_focus;
        }
        Some(HitTarget::TocItem(_)) => {
            let prev_focus = ui.focus;
            ui.focus = PaneFocus::Detail;
            move_direction(app, ui, step, 0);
            ui.focus = prev_focus;
        }
        Some(HitTarget::KanbanCard { .. }) => {
            let prev_focus = ui.focus;
            ui.focus = PaneFocus::Main;
            move_direction(app, ui, step, 0);
            ui.focus = prev_focus;
        }
        Some(HitTarget::DetailPane) => {
            scroll_active_pane(app, ui, delta);
        }
        _ => {
            if ui.active_tab == WorkspaceTab::Board && !ui.board_ui.drawer_open {
                let prev_focus = ui.focus;
                ui.focus = PaneFocus::Main;
                move_direction(app, ui, step, 0);
                ui.focus = prev_focus;
            } else {
                scroll_active_pane(app, ui, delta);
            }
        }
    }
}

fn scroll_active_pane(app: &mut AppState, ui: &mut UiState, delta: i32) {
    match ui.active_tab {
        WorkspaceTab::Board => {
            ui.board_ui.detail_scroll = apply_scroll(ui.board_ui.detail_scroll, delta);
        }
        WorkspaceTab::Issues => {
            ui.issue_ui.detail_scroll = apply_scroll(ui.issue_ui.detail_scroll, delta);
        }
        WorkspaceTab::Confluence => {
            ui.confluence_ui.reader_scroll = apply_scroll(ui.confluence_ui.reader_scroll, delta);
        }
        WorkspaceTab::MyWork | WorkspaceTab::Search | WorkspaceTab::Directory => {
            let step = if delta < 0 { -1 } else { 1 };
            move_direction(app, ui, step, 0);
        }
    }
}

fn activate_selection(app: &mut AppState, ui: &mut UiState) {
    match ui.active_tab {
        WorkspaceTab::Board => match ui.focus {
            PaneFocus::Nav => {
                if ui.board_ui.nav_section == 0 {
                    if let Some(b) = app.boards.get(ui.board_ui.selected_board) {
                        app.request_board_load(ui, b.id, None);
                    }
                } else if let (Some(b), Some(sp)) = (
                    app.active_board(ui),
                    app.sprints.get(ui.board_ui.selected_sprint),
                ) {
                    app.request_board_load(ui, b.id, Some(sp.id));
                }
            }
            _ => {
                ui.board_ui.drawer_open = true;
                let site_id = app.active_board_site_id(ui).map(str::to_string);
                if let Some(issue) = app.selected_board_issue(ui) {
                    app.request_issue_detail_for_site(ui, site_id.as_deref(), &issue.key);
                }
            }
        },
        WorkspaceTab::Issues => {
            if ui.issue_ui.nav_section == 0 {
                if let Some(f) = app.saved_filters.get(ui.issue_ui.selected_filter).cloned() {
                    ui.issue_ui.jql.set_text(f.jql.clone());
                    app.request_jql_search(ui, f.jql, f.name);
                }
            } else {
                let list = app.filtered_issues_with_sites(ui);
                if let Some((sid, it)) = list.get(ui.issue_ui.selected_issue) {
                    app.request_issue_detail_for_site(ui, Some(sid.as_str()), &it.key);
                    ui.focus = PaneFocus::Main;
                }
            }
        }
        WorkspaceTab::MyWork => {
            if ui.my_work_ui.active_column == 0 {
                let site_id = app.my_issue_site_ids.get(ui.my_work_ui.selected_issue).cloned();
                if let Some(it) = app.my_issues.get(ui.my_work_ui.selected_issue).cloned() {
                    app.jump_to_cross_ref_for_site(
                        ui,
                        site_id.as_deref(),
                        CrossRefTarget::JiraIssue {
                            key: it.key,
                            label: Some(it.summary),
                        },
                    );
                }
            } else {
                let site_id = app.my_page_site_ids.get(ui.my_work_ui.selected_page).cloned();
                if let Some(pg) = app.my_pages.get(ui.my_work_ui.selected_page).cloned() {
                    app.jump_to_cross_ref_for_site(
                        ui,
                        site_id.as_deref(),
                        CrossRefTarget::ConfluencePage {
                            page_id: pg.id,
                            title: Some(pg.title),
                        },
                    );
                }
            }
        }
        WorkspaceTab::Confluence => match ui.focus {
            PaneFocus::Nav => {
                if ui.confluence_ui.nav_section == 0 {
                    let site_id = app.space_site_ids.get(ui.confluence_ui.selected_space).cloned();
                    if let Some(sp) = app.spaces.get(ui.confluence_ui.selected_space).cloned() {
                        ui.confluence_ui.selected_tree_row = 0;
                        app.request_space_root_pages_for_site(ui, site_id.as_deref(), &sp.key);
                    }
                } else {
                    let site_id = app.active_space_site_id(ui).map(str::to_string);
                    let tree = app.current_confluence_tree(ui);
                    if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                        let page = row.page.clone();
                        app.request_page_detail_for_site(ui, site_id.as_deref(), &page.id);
                        if row.has_children {
                            app.toggle_expand_page(ui, &page);
                        }
                    }
                }
            }
            PaneFocus::Detail => {
                if let Some(page) = &app.active_page {
                    if let Some(entry) = page
                        .toc
                        .iter()
                        .filter(|t| t.level <= 4)
                        .nth(ui.confluence_ui.selected_toc)
                    {
                        ui.confluence_ui.reader_scroll = entry.line_hint as u16;
                        ui.focus = PaneFocus::Main;
                    }
                }
            }
            _ => {}
        },
        WorkspaceTab::Search => {
            let site_id = app.search_hit_site_ids.get(ui.search_ui.selected_hit).cloned();
            if let Some(hit) = app.search_hits.get(ui.search_ui.selected_hit).cloned() {
                let target = match hit.kind {
                    SearchHitKind::JiraIssue => CrossRefTarget::JiraIssue {
                        key: hit.id_or_key,
                        label: Some(hit.title),
                    },
                    SearchHitKind::ConfluencePage => CrossRefTarget::ConfluencePage {
                        page_id: hit.id_or_key,
                        title: Some(hit.title),
                    },
                };
                app.jump_to_cross_ref_for_site(ui, site_id.as_deref(), target);
            }
        }
        WorkspaceTab::Directory => {
            if ui.directory_ui.section == 0 {
                if let Some(proj) = app.projects.get(ui.directory_ui.selected_project).cloned() {
                    ui.active_tab = WorkspaceTab::Issues;
                    let jql = format!("project = {} ORDER BY updated DESC", proj.key);
                    ui.issue_ui.jql.set_text(jql.clone());
                    app.request_jql_search(ui, jql, proj.key);
                }
            } else if let Some(sp) = app.spaces.get(ui.directory_ui.selected_space).cloned() {
                if let Some(pos) = app.spaces.iter().position(|s| s.key == sp.key) {
                    ui.confluence_ui.selected_space = pos;
                }
                ui.active_tab = WorkspaceTab::Confluence;
                app.request_space_root_pages(ui, &sp.key);
            }
        }
    }
}

fn open_transition(app: &AppState, ui: &mut UiState) {
    if let Some(key) = app.selected_issue_key(ui) {
        app.request_open_transitions(ui, &key);
    } else {
        ui.info("Select a Jira issue to transition first");
    }
}

fn open_comment(app: &mut AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        if let Some(page) = &app.active_page {
            let target = CommentTarget::Page {
                page_id: page.summary.id.clone(),
                title: page.summary.title.clone(),
            };
            let overlay = if let Some(saved) = app.drafts.get(&target.draft_key()) {
                ui.info("Restored saved comment draft");
                CommentOverlay::with_draft(target, saved)
            } else {
                CommentOverlay::new(target)
            };
            ui.push_overlay(Overlay::Comment(overlay));
            app.request_comments_refresh(ui);
            return;
        }
    }
    if let Some(key) = app.selected_issue_key(ui) {
        let target = CommentTarget::Issue(key);
        let overlay = if let Some(saved) = app.drafts.get(&target.draft_key()) {
            ui.info("Restored saved comment draft");
            CommentOverlay::with_draft(target, saved)
        } else {
            CommentOverlay::new(target)
        };
        ui.push_overlay(Overlay::Comment(overlay));
        app.request_comments_refresh(ui);
    } else {
        ui.info("Select an issue or page to comment on first");
    }
}

fn open_new_item(app: &AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        let space_key = app
            .active_space(ui)
            .map(|s| s.key.clone())
            .or_else(|| app.config.default_space_key.clone())
            .unwrap_or_default();
        if space_key.is_empty() {
            ui.info("Select a Confluence space first");
            return;
        }
        let parent_id = app.active_page.as_ref().map(|p| p.summary.id.clone());
        let mut form = PageFormOverlay::new_page(space_key, parent_id);
        if let Some(saved) = app.drafts.get(&form.draft_key()) {
            if form.apply_draft_raw(saved) {
                ui.info("Restored saved page draft");
            }
        }
        ui.push_overlay(Overlay::PageForm(form));
    } else {
        let default_proj = app
            .active_board(ui)
            .and_then(|b| b.project_key.clone())
            .or_else(|| app.config.default_project.clone())
            .or_else(|| app.projects.first().map(|p| p.key.clone()))
            .unwrap_or_default();
        let mut form = IssueFormOverlay::new_issue(&default_proj);
        if let Some(saved) = app.drafts.get(&form.draft_key()) {
            if form.apply_draft_raw(saved) {
                ui.info("Restored saved issue draft");
            }
        }
        ui.push_overlay(Overlay::IssueForm(form));
    }
}

fn open_edit_item(app: &AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        if let Some(page) = &app.active_page {
            let mut form = PageFormOverlay::edit_page(page);
            if let Some(saved) = app.drafts.get(&form.draft_key()) {
                if form.apply_draft_raw(saved) {
                    ui.info("Restored saved page edit draft");
                }
            }
            ui.push_overlay(Overlay::PageForm(form));
        } else {
            ui.info("Select a Confluence page to edit first");
        }
    } else if let Some(issue) = &app.active_issue {
        let mut form = IssueFormOverlay::edit_issue(issue);
        if let Some(saved) = app.drafts.get(&form.draft_key()) {
            if form.apply_draft_raw(saved) {
                ui.info("Restored saved issue edit draft");
            }
        }
        ui.push_overlay(Overlay::IssueForm(form));
    } else {
        ui.info("Select a Jira issue to edit first");
    }
}

fn open_upload_attachment(app: &AppState, ui: &mut UiState) {
    if let Some(target) = app.active_upload_target(ui) {
        ui.push_overlay(Overlay::PathInput(PathInputOverlay::new(target)));
    } else {
        ui.info("Select an issue or page to attach a file first");
    }
}

fn open_file_picker(app: &AppState, ui: &mut UiState) {
    if let Some(target) = app.active_upload_target(ui) {
        ui.push_overlay(Overlay::FilePicker(FilePickerOverlay::new(target)));
    } else {
        ui.info("Select an issue or page to attach a file first");
    }
}

fn try_paste_clipboard_image(
    target: Option<UploadTarget>,
    app: &AppState,
    ui: &mut UiState,
) -> bool {
    let Some(res) = overlays::read_clipboard_image_to_temp() else {
        return false;
    };
    match res {
        Ok(path) => {
            let fname = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("pasted.png")
                .to_string();
            if let Some(upload_target) = target {
                let target_label = match &upload_target {
                    UploadTarget::Issue(k) => format!("Issue {k}"),
                    UploadTarget::Page(id) => format!("Page #{id}"),
                };
                app.request_upload_file(ui, upload_target, path);
                ui.info(format!(
                    "Pasted clipboard image ({fname}) — uploading to {target_label}…"
                ));
            } else {
                ui.info(format!(
                    "Saved clipboard image to {} (select an issue or page to attach)",
                    path.display()
                ));
            }
            true
        }
        Err(err) => {
            ui.error(format!("Clipboard image paste failed: {err}"));
            true
        }
    }
}

fn download_first_attachment(app: &AppState, ui: &mut UiState) {
    let (_, atts) = app.current_attachments(ui);
    if let Some(first) = atts.first().cloned() {
        app.request_download_attachment(ui, first);
    } else {
        ui.info("No attachments to download");
    }
}

fn open_image_preview(app: &AppState, ui: &mut UiState) {
    let (label, atts) = app.current_attachments(ui);
    open_image_preview_with(app, ui, label, &atts, None);
}

fn open_image_preview_with(
    app: &AppState,
    ui: &mut UiState,
    label: String,
    atts: &[crate::models::AttachmentItem],
    preferred_idx: Option<usize>,
) {
    if let Some(idx) = preferred_idx {
        if let Some(att) = atts.get(idx) {
            if html_preview::is_html_attachment(&att.mime_type, &att.filename) {
                open_html_attachment_preview(app, ui, label, att.clone());
                return;
            }
        }
    }
    if let Some(mut ov) = ImagePreviewOverlay::from_attachments(label.clone(), atts, preferred_idx) {
        ov.apply_graphics_caps(ui.graphics_caps);
        if app.config.image_protocol != ConfigImageProtocol::Auto {
            ov.protocol = ui.graphics_caps.protocol;
            ov.protocol_overridden = true;
        }
        ui.push_overlay(Overlay::ImagePreview(ov));
        app.request_load_preview_image(ui);
    } else if let Some(html_att) = atts
        .iter()
        .find(|att| html_preview::is_html_attachment(&att.mime_type, &att.filename))
        .cloned()
    {
        open_html_attachment_preview(app, ui, label, html_att);
    } else {
        ui.info("No image attachments to preview");
    }
}

fn open_html_preview(app: &AppState, ui: &mut UiState) {
    let prefer_confluence = ui.active_tab == WorkspaceTab::Confluence
        || (ui.active_tab == WorkspaceTab::MyWork && ui.my_work_ui.active_column == 1);

    if prefer_confluence {
        if let Some(page) = &app.active_page {
            let html = html_preview::page_detail_to_html(page);
            let mut ov = HtmlPreviewOverlay::new(page.summary.title.clone(), html);
            ov.source_url = page.summary.web_url.clone();
            let cmds = ov.take_pending_commands();
            ui.push_overlay(Overlay::HtmlPreview(ov));
            apply_html_preview_commands(cmds, app, ui);
            return;
        }
    }

    if let Some(issue) = &app.active_issue {
        let html = html_preview::issue_detail_to_html(issue);
        let title = format!("{} — {}", issue.summary.key, issue.summary.summary);
        let mut ov = HtmlPreviewOverlay::new(title, html);
        ov.source_url = issue.summary.web_url.clone();
        let cmds = ov.take_pending_commands();
        ui.push_overlay(Overlay::HtmlPreview(ov));
        apply_html_preview_commands(cmds, app, ui);
        return;
    }

    if let Some(page) = &app.active_page {
        let html = html_preview::page_detail_to_html(page);
        let mut ov = HtmlPreviewOverlay::new(page.summary.title.clone(), html);
        ov.source_url = page.summary.web_url.clone();
        let cmds = ov.take_pending_commands();
        ui.push_overlay(Overlay::HtmlPreview(ov));
        apply_html_preview_commands(cmds, app, ui);
        return;
    }

    ui.info("Select a Confluence page or Jira issue to preview as HTML");
}

fn open_html_attachment_preview(
    app: &AppState,
    ui: &mut UiState,
    owner: String,
    att: crate::models::AttachmentItem,
) {
    let title = if owner.trim().is_empty() {
        att.filename.clone()
    } else {
        format!("{owner} — {}", att.filename)
    };
    let ov = HtmlPreviewOverlay::loading(title.clone(), att.content_url.clone());
    ui.push_overlay(Overlay::HtmlPreview(ov));
    app.request_load_html_attachment(ui, title, att);
}

fn apply_html_preview_commands(
    commands: Vec<HtmlPreviewCommand>,
    app: &AppState,
    ui: &mut UiState,
) {
    for cmd in commands {
        match cmd {
            HtmlPreviewCommand::ServeInBrowser { title, html } => {
                app.request_serve_html_in_browser(ui, title, html);
            }
            HtmlPreviewCommand::OpenUrl(url) => {
                if cfg!(test) {
                    ui.info(format!("Open URL: {url}"));
                } else if let Err(err) = open::that(&url) {
                    ui.error(format!("Failed to open browser: {err}"));
                }
            }
            HtmlPreviewCommand::Toast(msg) => {
                ui.info(msg);
            }
        }
    }
}

fn open_in_browser(app: &AppState, ui: &mut UiState) {
    let url = if ui.active_tab == WorkspaceTab::Confluence {
        app.active_page.as_ref().map(|p| p.summary.web_url.clone())
    } else {
        app.active_issue.as_ref().map(|i| i.summary.web_url.clone())
    };
    if let Some(u) = url.filter(|s| !s.trim().is_empty()) {
        if let Err(err) = open::that(&u) {
            ui.error(format!("Failed to open browser: {err}"));
        }
    } else {
        ui.info("No web URL available to open");
    }
}

fn toggle_pin(app: &mut AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Directory {
        if ui.directory_ui.section == 0 {
            if let Some(p) = app.projects.get_mut(ui.directory_ui.selected_project) {
                p.pinned = !p.pinned;
                let _ = app.storage.set_project_pinned(&p.key, p.pinned);
                ui.info(format!(
                    "{} {}",
                    p.key,
                    if p.pinned { "pinned" } else { "unpinned" }
                ));
            }
        } else if let Some(s) = app.spaces.get_mut(ui.directory_ui.selected_space) {
            s.pinned = !s.pinned;
            let _ = app.storage.set_space_pinned(&s.key, s.pinned);
            ui.info(format!(
                "{} {}",
                s.key,
                if s.pinned { "pinned" } else { "unpinned" }
            ));
        }
    }
}

fn step_index(cur: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        cur.saturating_sub((-delta) as usize)
    } else {
        (cur + delta as usize).min(len - 1)
    }
}

fn apply_scroll(cur: u16, delta: i32) -> u16 {
    if delta < 0 {
        cur.saturating_sub((-delta) as u16)
    } else {
        cur.saturating_add(delta as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, DensityMode, DraftStore, Persona, ThemePreset},
        models::{AttachmentItem, IssueDetail, IssueSummary, PageDetail, PageSummary, SpaceSummary},
        storage::Storage,
        sync,
        ui::app::UiMsg,
    };
    use tokio::sync::mpsc;

    fn test_app_and_ui() -> (AppState, UiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            default_project: Some("ATL".into()),
            ..Default::default()
        };
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        app.config_path_override = Some(dir.path().join("config.toml"));
        app.drafts_path_override = Some(dir.path().join("drafts.json"));

        let mut issue = IssueSummary {
            key: "ATL-42".into(),
            project_key: "ATL".into(),
            summary: "Integrate overlays".into(),
            web_url: "https://acme.atlassian.net/browse/ATL-42".into(),
            ..Default::default()
        };
        issue.status.name = "To Do".into();
        app.board_issues.push(issue.clone());
        app.issues.push(issue.clone());
        app.active_issue = Some(IssueDetail {
            summary: issue,
            attachments: vec![
                AttachmentItem {
                    id: "att-img".into(),
                    filename: "arch.png".into(),
                    mime_type: "image/png".into(),
                    size: 1024,
                    author_name: Some("Alice".into()),
                    created_at: String::new(),
                    content_url: "https://acme.atlassian.net/att/1".into(),
                    thumbnail_url: None,
                },
                AttachmentItem {
                    id: "att-pdf".into(),
                    filename: "spec.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size: 4096,
                    author_name: Some("Bob".into()),
                    created_at: String::new(),
                    content_url: "https://acme.atlassian.net/att/2".into(),
                    thumbnail_url: None,
                },
            ],
            ..Default::default()
        });
        app.spaces.push(SpaceSummary {
            id: "s1".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            ..Default::default()
        });
        app.active_page = Some(PageDetail {
            summary: PageSummary {
                id: "900".into(),
                space_key: "ENG".into(),
                title: "Design Doc".into(),
                version_number: 2,
                web_url: "https://acme.atlassian.net/wiki/pages/900".into(),
                ..Default::default()
            },
            body_markdown: "# Intro".into(),
            ..Default::default()
        });

        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui, dir)
    }

    #[tokio::test]
    async fn settings_overlay_mutates_live_ui_and_persists_config_toml() {
        let (mut app, mut ui, dir) = test_app_and_ui();
        dispatch(Action::OpenSettings, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::Settings(_))));

        // Toggle notifications (row 0)
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(!app.config.notify);
        assert!(!ui.notify);

        // Toggle left sidebar (row 1)
        dispatch(Action::MoveDown, &mut app, &mut ui);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(!app.config.show_nav_rail);
        assert!(!ui.show_nav_rail);

        // Toggle inline images (row 2)
        dispatch(Action::MoveDown, &mut app, &mut ui);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(!app.config.inline_images);
        assert!(!ui.inline_images);

        // Cycle image protocol (row 3: Auto -> Kitty)
        dispatch(Action::MoveDown, &mut app, &mut ui);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert_eq!(app.config.image_protocol, ConfigImageProtocol::Kitty);
        assert_eq!(
            ui.graphics_caps.protocol,
            crate::img_preview::TerminalImageProtocol::Kitty
        );

        // Toggle view density (row 4: Cozy -> Compact)
        dispatch(Action::MoveDown, &mut app, &mut ui);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert_eq!(app.config.density, DensityMode::Compact);
        assert_eq!(ui.density, DensityMode::Compact);

        // Cycle color theme (row 5: Auto -> Dark -> Light)
        dispatch(Action::MoveDown, &mut app, &mut ui);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert_eq!(app.config.theme, ThemePreset::Dark);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert_eq!(app.config.theme, ThemePreset::Light);

        // Verify persisted config.toml round-trips all changes
        let saved = AppConfig::load_from(&dir.path().join("config.toml")).unwrap();
        assert!(!saved.notify);
        assert!(!saved.show_nav_rail);
        assert!(!saved.inline_images);
        assert_eq!(saved.image_protocol, ConfigImageProtocol::Kitty);
        assert_eq!(saved.density, DensityMode::Compact);
        assert_eq!(saved.theme, ThemePreset::Light);

        // Open keyboard shortcuts from Settings (row 7) closes Settings and opens Help
        dispatch(Action::MoveDown, &mut app, &mut ui); // Download folder (6)
        dispatch(Action::Activate, &mut app, &mut ui);
        dispatch(Action::MoveDown, &mut app, &mut ui); // Keyboard shortcuts (7)
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::Help)));
    }

    #[tokio::test]
    async fn context_menu_and_attachments_overlays_dispatch_cleanly() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Open context menu via `.`
        dispatch(Action::OpenContextMenu, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_))));

        // Select "Attachments" item (index 4 in JiraIssue menu) and activate
        if let Some(Overlay::ContextMenu(ov)) = ui.overlays.last_mut() {
            ov.select(4);
        }
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::Attachments(_))));

        // Activate row 0 (arch.png) -> closes Attachments and opens ImagePreview
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(!ui.has_overlay());

        // Open attachments directly with `A`, press `u` -> opens PathInputOverlay
        dispatch(Action::OpenAttachments, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::Attachments(_))));
        dispatch(Action::Input('u'), &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::PathInput(_))));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
    }

    #[tokio::test]
    async fn drafts_persist_on_close_restore_on_reopen_and_clear_on_submit() {
        let (mut app, mut ui, dir) = test_app_and_ui();

        // Comment draft lifecycle: preserved on submit failure, cleared only on API success
        dispatch(Action::OpenComment, &mut app, &mut ui);
        dispatch(Action::Paste("WIP comment draft".into()), &mut app, &mut ui);
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(!ui.has_overlay());
        let saved = DraftStore::load_from(&dir.path().join("drafts.json")).unwrap();
        assert_eq!(
            saved.get("comment:issue:ATL-42"),
            Some("WIP comment draft")
        );

        // Reopen restores draft
        dispatch(Action::OpenComment, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::Comment(ov)) => assert_eq!(ov.draft_text(), "WIP comment draft"),
            other => panic!("expected Comment overlay, got {other:?}"),
        }
        // Submit keeps draft until server confirms success
        dispatch(Action::Submit, &mut app, &mut ui);
        assert_eq!(
            app.drafts.get("comment:issue:ATL-42"),
            Some("WIP comment draft")
        );
        // API failure preserves the draft
        app.handle_ui_msg(
            &mut ui,
            UiMsg::CommentAdded {
                site_id: "default".into(),
                issue_key: Some("ATL-42".into()),
                page_id: None,
                optimistic_id: None,
                task_label: "Comment on ATL-42".into(),
                clear_draft_key: Some("comment:issue:ATL-42".into()),
                result: Err("HTTP 503".into()),
            },
        );
        assert_eq!(
            app.drafts.get("comment:issue:ATL-42"),
            Some("WIP comment draft")
        );
        // API success clears the draft
        app.handle_ui_msg(
            &mut ui,
            UiMsg::CommentAdded {
                site_id: "default".into(),
                issue_key: Some("ATL-42".into()),
                page_id: None,
                optimistic_id: None,
                task_label: "Comment on ATL-42".into(),
                clear_draft_key: Some("comment:issue:ATL-42".into()),
                result: Ok(()),
            },
        );
        assert!(app.drafts.get("comment:issue:ATL-42").is_none());

        // Issue form draft lifecycle
        dispatch(Action::NewItem, &mut app, &mut ui);
        dispatch(Action::Paste("Draft issue title".into()), &mut app, &mut ui);
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(app.drafts.get("issue:new:ATL").is_some());

        dispatch(Action::NewItem, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::IssueForm(ov)) => assert_eq!(ov.summary.text, "Draft issue title"),
            other => panic!("expected IssueForm overlay, got {other:?}"),
        }
        dispatch(Action::Submit, &mut app, &mut ui);
        assert!(app.drafts.get("issue:new:ATL").is_some());
        app.handle_ui_msg(&mut ui, UiMsg::IssueActionFailed("HTTP 500".into()));
        assert!(app.drafts.get("issue:new:ATL").is_some());
        let created_issue = app.active_issue.clone().unwrap();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssueMutated {
                site_id: "default".into(),
                detail: created_issue,
                toast: "Created".into(),
                clear_draft_key: Some("issue:new:ATL".into()),
            },
        );
        assert!(app.drafts.get("issue:new:ATL").is_none());

        // Confluence page form draft lifecycle
        ui.active_tab = WorkspaceTab::Confluence;
        dispatch(Action::NewItem, &mut app, &mut ui);
        dispatch(Action::Paste("Draft wiki title".into()), &mut app, &mut ui);
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(app.drafts.get("page:new:ENG:900").is_some());

        dispatch(Action::NewItem, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::PageForm(ov)) => assert_eq!(ov.title.text, "Draft wiki title"),
            other => panic!("expected PageForm overlay, got {other:?}"),
        }
        dispatch(Action::Submit, &mut app, &mut ui);
        assert!(app.drafts.get("page:new:ENG:900").is_some());
        app.handle_ui_msg(&mut ui, UiMsg::PageActionFailed("HTTP 409 Conflict".into()));
        assert!(app.drafts.get("page:new:ENG:900").is_some());
        let created_page = app.active_page.clone().unwrap();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageMutated {
                site_id: "default".into(),
                detail: created_page,
                toast: "Saved".into(),
                clear_draft_key: Some("page:new:ENG:900".into()),
            },
        );
        assert!(app.drafts.get("page:new:ENG:900").is_none());
    }

    #[tokio::test]
    async fn links_overlay_href_toggle_and_file_picker_dispatch_cleanly() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Toggle inline href display with toast feedback
        assert!(!ui.show_hrefs);
        dispatch(Action::ToggleHrefDisplay, &mut app, &mut ui);
        assert!(ui.show_hrefs);
        assert!(!ui.toasts.is_empty());
        dispatch(Action::ToggleHrefDisplay, &mut app, &mut ui);
        assert!(!ui.show_hrefs);

        // Open Link Extractor (`u`) and verify extracted URLs from active issue
        dispatch(Action::OpenLinks, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::Links(ov)) => {
                assert_eq!(ov.source_label, "ATL-42");
                assert!(ov
                    .items
                    .iter()
                    .any(|l| l.url == "https://acme.atlassian.net/browse/ATL-42"));
            }
            other => panic!("expected Links overlay, got {other:?}"),
        }
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(!ui.has_overlay());

        // Open File Picker (`Ctrl+F`) for active issue
        dispatch(Action::OpenFilePicker, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::FilePicker(_))));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(!ui.has_overlay());
    }

    #[tokio::test]
    async fn html_preview_entry_points_pills_attachments_and_click_outside() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Confluence page with storage HTML + interactive button
        if let Some(page) = app.active_page.as_mut() {
            page.body_representation = "storage".into();
            page.body_raw = "<p id=\"msg\">Ready</p><button onclick=\"document.getElementById('msg').textContent='Clicked'\">Run</button>".into();
        }
        ui.active_tab = WorkspaceTab::Confluence;

        // Render Confluence tab and verify clickable `[H: HTML]` pill is registered in HitMap
        let backend = ratatui::backend::TestBackend::new(130, 36);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        let pill_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlPreview))
            .expect("expected clickable [H: HTML] pill in Confluence view");
        dispatch(
            Action::MouseClick {
                row: pill_rect.y,
                col: pill_rect.x + 1,
            },
            &mut app,
            &mut ui,
        );
        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert!(ov.is_js_active());
                assert!(ov.session().unwrap().raw_html().contains("Clicked"));
            }
            other => panic!("expected HtmlPreview overlay, got {other:?}"),
        }

        // Draw overlay so HitMap has OverlayBody, then click outside (0, 0) to close
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        dispatch(Action::MouseClick { row: 0, col: 0 }, &mut app, &mut ui);
        assert!(!ui.has_overlay(), "click outside should close HtmlPreview");

        // Switch to Issues tab, verify `[H: HTML]` pill and `H` shortcut open HtmlPreview
        ui.active_tab = WorkspaceTab::Issues;
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        assert!(
            ui.hitmap
                .region_of(HitTarget::Button(ButtonId::HtmlPreview))
                .is_some(),
            "expected clickable [H: HTML] pill in Jira Issue view"
        );
        dispatch(Action::OpenHtmlPreview, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // Attachments overlay `v` on `.html` attachment routes to HtmlPreview (NOT ImagePreview!)
        if let Some(issue) = app.active_issue.as_mut() {
            issue.attachments.push(AttachmentItem {
                id: "att-html".into(),
                filename: "dashboard.html".into(),
                mime_type: "text/html".into(),
                size: 512,
                author_name: Some("Alice".into()),
                created_at: String::new(),
                content_url: "https://acme.atlassian.net/att/html".into(),
                thumbnail_url: None,
            });
        }
        dispatch(Action::OpenAttachments, &mut app, &mut ui);
        if let Some(Overlay::Attachments(ov)) = ui.overlays.last_mut() {
            ov.select(2);
        }
        dispatch(Action::Input('v'), &mut app, &mut ui);
        assert!(
            matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))),
            "pressing `v` on .html attachment must open HtmlPreview, not ImagePreview"
        );
    }

    #[tokio::test]
    async fn load_preview_image_sniffs_html_bytes_and_auto_routes_to_html_preview() {
        use axum::{routing::get, Router};
        use tokio::net::TcpListener;

        let router = Router::new().route(
            "/fake-image.png",
            get(|| async {
                "<!DOCTYPE html><html><body><h1>SSO Login Gate</h1><button onclick=\"alert('ok')\">Continue</button></body></html>"
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let base_url = format!("http://{addr}");

        let cfg = AppConfig {
            site_url: base_url.clone(),
            image_protocol: ConfigImageProtocol::Halfblock,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            crate::auth::StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "test-token".into(),
                deployment: crate::config::DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        let att = AttachmentItem {
            id: "att-sso".into(),
            filename: "avatar.png".into(),
            mime_type: "image/png".into(),
            size: 128,
            author_name: None,
            created_at: String::new(),
            content_url: format!("{base_url}/fake-image.png"),
            thumbnail_url: None,
        };
        let mut img_ov = ImagePreviewOverlay::from_attachments("ATL-99", &[att], None).unwrap();
        img_ov.protocol = crate::img_preview::TerminalImageProtocol::Halfblock;
        ui.push_overlay(Overlay::ImagePreview(img_ov));

        // Trigger LoadPreviewImage -> downloads HTML bytes -> sniffs ContentKind::Html
        dispatch(Action::LoadPreviewImage, &mut app, &mut ui);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("PreviewImageSniffedAsHtml timed out")
            .expect("ui_rx closed");
        app.handle_ui_msg(&mut ui, msg);

        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert_eq!(ov.title, "avatar.png");
                assert!(ov.is_js_active());
                assert!(ov.session().unwrap().raw_html().contains("SSO Login Gate"));
            }
            other => panic!("expected auto-routed HtmlPreview overlay, got {other:?}"),
        }
    }
}
