//! Action dispatcher for keyboard, mouse, overlays, and workspace tabs.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::{
    config::{self, ImageProtocolPreference as ConfigImageProtocol},
    models::{CrossRefTarget, SearchHitKind},
    ui::{
        app::{AppState, PaneFocus, SiteKey, UiState, WorkspaceTab},
        filter::is_personal_space,
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
                let hit = ui.hitmap.hit(row, col);
                let is_double_click = hit.is_some_and(|t| record_click(ui, t));
                match hit {
                    Some(HitTarget::OverlayItem(idx)) => {
                        if let Some(Overlay::FilePicker(fp)) = ui.overlays.last_mut() {
                            let was_selected = fp.selected == idx;
                            fp.select(idx);
                            if fp.is_selected_directory() || is_double_click || was_selected {
                                return dispatch_active_overlay(Action::Activate, app, ui);
                            }
                            return false;
                        }
                        return handle_overlay_item_click(idx, app, ui);
                    }
                    Some(HitTarget::OverlayItemToggle(idx)) => {
                        let outcome = ui.overlays.last_mut().map(|o| o.click_toggle(idx));
                        if let Some(Overlay::SiteSwitcher(ov)) = ui.overlays.last_mut() {
                            if let Some(cmd) = ov.take_pending_command() {
                                if matches!(outcome, Some(OverlayOutcome::Close)) {
                                    ui.pop_overlay();
                                }
                                app.apply_site_switcher_command(cmd, ui);
                                return false;
                            }
                        }
                        return false;
                    }
                    Some(HitTarget::OverlayField(idx)) => {
                        if let Some(top) = ui.overlays.last_mut() {
                            top.click_field(idx);
                        }
                        return false;
                    }
                    Some(HitTarget::OverlayClose) => {
                        if let Some(top) = ui.overlays.last_mut() {
                            top.force_close_if_editing();
                        }
                        close_top_overlay_and_save_draft(app, ui);
                        return false;
                    }
                    Some(HitTarget::Button(btn)) => {
                        if btn == ButtonId::OverlayCancel {
                            close_top_overlay_and_save_draft(app, ui);
                            return false;
                        }
                        if btn == ButtonId::OverlayImageProtocol {
                            apply_settings_command(SettingsCommand::CycleImageProtocol, app, ui);
                            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                                app.request_load_preview_image(ui);
                            }
                            return false;
                        }
                        return dispatch_active_overlay(
                            crate::ui::event::button_to_action(btn),
                            app,
                            ui,
                        );
                    }
                    Some(HitTarget::OverlayBody | HitTarget::OverlayScrollArea) => {
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
                // Backdrop click outside any modal closes it (saving drafts if applicable).
                close_top_overlay_and_save_draft(app, ui);
                return false;
            }
            Action::MouseRightClick { row, col } => {
                if matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_))) {
                    ui.pop_overlay();
                    return handle_mouse_right_click(row, col, app, ui);
                }
                return false;
            }
            Action::MouseScroll { delta, .. }
                if matches!(ui.overlays.last(), Some(Overlay::Help)) =>
            {
                let max = overlays::help_max_scroll_for(Rect::new(
                    0,
                    0,
                    ui.term_width,
                    ui.term_height,
                ));
                ui.help_max_scroll = max;
                ui.help_scroll = apply_scroll_clamped(ui.help_scroll, delta, max);
                return false;
            }
            Action::ScrollUp | Action::PageUp
                if matches!(ui.overlays.last(), Some(Overlay::Help)) =>
            {
                let max = overlays::help_max_scroll_for(Rect::new(
                    0,
                    0,
                    ui.term_width,
                    ui.term_height,
                ));
                ui.help_max_scroll = max;
                ui.help_scroll = apply_scroll_clamped(ui.help_scroll, -4, max);
                return false;
            }
            Action::ScrollDown | Action::PageDown
                if matches!(ui.overlays.last(), Some(Overlay::Help)) =>
            {
                let max = overlays::help_max_scroll_for(Rect::new(
                    0,
                    0,
                    ui.term_width,
                    ui.term_height,
                ));
                ui.help_max_scroll = max;
                ui.help_scroll = apply_scroll_clamped(ui.help_scroll, 4, max);
                return false;
            }
            Action::PasteClipboard => {
                let image_target = match ui.overlays.last() {
                    Some(Overlay::Comment(ov)) => Some((
                        ov.site_id.clone(),
                        match &ov.target {
                            CommentTarget::Issue(key) => UploadTarget::Issue(key.clone()),
                            CommentTarget::Page { page_id, .. } => {
                                UploadTarget::Page(page_id.clone())
                            }
                        },
                    )),
                    Some(Overlay::IssueForm(ov)) => ov
                        .edit_key
                        .clone()
                        .map(|key| (ov.site_id.clone(), UploadTarget::Issue(key)))
                        .or_else(|| app.active_upload_ref(ui)),
                    Some(Overlay::PageForm(ov)) => ov
                        .edit_page_id
                        .clone()
                        .map(|id| (ov.site_id.clone(), UploadTarget::Page(id)))
                        .or_else(|| app.active_upload_ref(ui)),
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
                            let site = ov.site_id.clone();
                            let key = ov.project_or_issue_key.clone();
                            let query = ov.query.text.clone();
                            app.request_assignable_users(site.as_deref(), &key, &query);
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
            ui.help_scroll = 0;
            ui.push_overlay(Overlay::Help);
        }
        Action::SyncAll => {
            app.request_multi_site_sync(ui);
            app.refresh_active_context(ui);
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
            let mut ov = CrossLinkOverlay::new(label, targets);
            ov.site_id = app.current_item_site(ui.active_tab);
            ui.push_overlay(Overlay::CrossLink(ov));
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
            let target = app.active_upload_ref(ui);
            if !try_paste_clipboard_image(target, app, ui) {
                ui.info("No image in clipboard to attach");
            }
        }
        Action::FocusNextPane => cycle_focus(ui, true),
        Action::FocusPrevPane => cycle_focus(ui, false),

        // Jira & Confluence Domain Actions
        Action::OpenTransition => open_transition(app, ui),
        Action::AssignToMe => {
            if let Some((site, key)) = app.selected_issue_ref(ui) {
                app.request_assign_to_me_for_site(ui, Some(&site), &key);
            } else {
                ui.info("No Jira issue selected");
            }
        }
        Action::OpenAssignee => {
            if let Some((site, key)) = app.selected_issue_ref(ui) {
                // Only borrow the opened issue's project when it *is* the selected row.
                let proj = app
                    .active_issue
                    .as_ref()
                    .filter(|d| d.summary.key == key)
                    .map(|d| d.summary.project_key.as_str())
                    .unwrap_or("");
                app.request_open_assignee_picker(ui, Some(&site), &key, proj);
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
            WorkspaceTab::Confluence
                if ui.focus == PaneFocus::Nav && ui.confluence_ui.nav_section == 0 =>
            {
                ui.confluence_ui.space_filter_active = true;
            }
            WorkspaceTab::Search => {
                ui.focus = PaneFocus::SearchBar;
            }
            _ => {
                switch_tab(WorkspaceTab::Search, app, ui);
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
        Action::MouseScrollHoriz { delta, .. } => {
            if ui.active_tab == WorkspaceTab::Board {
                let dx = if delta < 0 { -1 } else { 1 };
                let prev_focus = ui.focus;
                ui.focus = PaneFocus::Main;
                move_direction(app, ui, 0, dx);
                ui.focus = prev_focus;
            }
        }
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
                        let node = SiteKey::new(&app.tree_site(ui), &row.page.id);
                        app.expanded_pages.remove(&node);
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
    ui.confluence_ui.space_filter_active = false;
    match tab {
        WorkspaceTab::MyWork => {
            ui.focus = PaneFocus::Main;
            app.maybe_refresh_my_work_on_tab_switch(ui);
        }
        WorkspaceTab::Search => {
            ui.focus = PaneFocus::SearchBar;
            ui.search_ui.is_remote = false;
            app.refresh_local_search(&ui.search_ui.query.text);
        }
        WorkspaceTab::Confluence => {
            ui.focus = PaneFocus::Nav;
            if let Some(sp) = app.active_space(ui).cloned() {
                if !app
                    .root_pages_by_space
                    .contains_key(&SiteKey::new(&app.tree_site(ui), &sp.key))
                {
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
    if action == Action::PasteClipboard {
        if let Some(text) = overlays::read_clipboard_text() {
            let cleaned = overlays::sanitize_single_line(&text);
            if !cleaned.is_empty() {
                return handle_inline_text_action(Action::Paste(cleaned), app, ui);
            }
        }
        return false;
    }

    if ui.active_tab == WorkspaceTab::Confluence && ui.confluence_ui.space_filter_active {
        handle_space_filter_action(action, app, ui);
        return false;
    }

    if action == Action::CloseOverlay {
        ui.issue_ui.local_filter_active = false;
        ui.focus = PaneFocus::Main;
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
                    ui.search_ui.is_remote = false;
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                    ui.search_ui.selected_hit = 0;
                }
                Action::CursorLeft => buf.move_left(),
                Action::CursorRight => buf.move_right(),
                Action::CursorHome => buf.move_home(),
                Action::CursorEnd => buf.move_end(),
                Action::Paste(s) => {
                    buf.insert_str(&s);
                    ui.search_ui.is_remote = false;
                    let q = buf.text.clone();
                    app.refresh_local_search(&q);
                    ui.search_ui.selected_hit = 0;
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

/// Keys while the SPACES filter is active: text edits narrow the list, `Up`/`Down`
/// move within it, `Enter` opens the selected space and keeps the query, `Esc`
/// clears the query first and closes the filter when it is already empty.
fn handle_space_filter_action(action: Action, app: &mut AppState, ui: &mut UiState) {
    let buf = &mut ui.confluence_ui.space_filter;
    let before = buf.text.clone();
    match action {
        Action::CloseOverlay => {
            if buf.text.is_empty() {
                ui.confluence_ui.space_filter_active = false;
            } else {
                buf.clear();
            }
        }
        Action::Input(ch) => buf.insert_char(ch),
        Action::Backspace => buf.backspace(),
        Action::Delete => buf.delete(),
        Action::CursorLeft => buf.move_left(),
        Action::CursorRight => buf.move_right(),
        Action::CursorHome => buf.move_home(),
        Action::CursorEnd => buf.move_end(),
        Action::Paste(s) => buf.insert_str(&overlays::sanitize_single_line(&s)),
        Action::CursorUp => step_space_selection(app, ui, -1),
        Action::CursorDown => step_space_selection(app, ui, 1),
        Action::Submit => {
            ui.confluence_ui.space_filter_active = false;
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 0;
            let selected = ui.confluence_ui.selected_space;
            if app.visible_space_indices(ui).contains(&selected) {
                open_selected_space(app, ui);
            }
        }
        _ => {}
    }
    if ui.confluence_ui.space_filter.text != before {
        let prev = ui.confluence_ui.selected_space;
        app.keep_space_selection_visible(ui);
        if ui.confluence_ui.selected_space != prev {
            ui.confluence_ui.user_moved_space = true;
        }
    }
}

/// Moves the SPACES selection `dy` rows within the listed (filtered) spaces.
fn step_space_selection(app: &AppState, ui: &mut UiState, dy: i32) {
    let visible = app.visible_space_indices(ui);
    if visible.is_empty() {
        return;
    }
    let next = match visible
        .iter()
        .position(|&i| i == ui.confluence_ui.selected_space)
    {
        Some(pos) => step_index(pos, dy, visible.len()),
        None => 0,
    };
    ui.confluence_ui.selected_space = visible[next];
    ui.confluence_ui.user_moved_space = true;
}

/// Loads the page tree of the selected space (`Enter` in the SPACES section).
fn open_selected_space(app: &mut AppState, ui: &mut UiState) {
    let site_id = app
        .space_site_ids
        .get(ui.confluence_ui.selected_space)
        .cloned();
    if let Some(sp) = app.spaces.get(ui.confluence_ui.selected_space).cloned() {
        ui.confluence_ui.selected_tree_row = 0;
        app.request_space_root_pages_for_site(ui, site_id.as_deref(), &sp.key);
    }
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
            let key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
            if ov.has_unsent_content() {
                app.drafts.set(key, ov.draft_text());
                let _ = app.save_drafts();
                ui.info("Saved comment draft");
            } else if app.drafts.remove(&key).is_some() {
                let _ = app.save_drafts();
            }
        }
        Overlay::IssueForm(ov) => {
            let key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
            if ov.has_unsent_content() {
                app.drafts.set(key, ov.to_draft_json());
                let _ = app.save_drafts();
                ui.info("Saved issue draft");
            } else if app.drafts.remove(&key).is_some() {
                let _ = app.save_drafts();
            }
        }
        Overlay::PageForm(ov) => {
            let key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
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
            app.image_cache.clear();
            mutated = true;
            ui.info(format!("Color theme: {}", app.config.theme.label()));
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                app.request_load_preview_image(ui);
            }
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
                app.request_assignable_users(
                    ov.site_id.as_deref(),
                    &ov.project_or_issue_key,
                    &ov.query.text,
                );
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
                        app.request_transition_issue_for_site(
                            ui,
                            ov.site_id.as_deref(),
                            &ov.issue_key,
                            tr,
                        );
                    }
                }
                Overlay::Assignee(ov) => {
                    if let Some(choice) = ov.selected_assignee() {
                        app.request_assign_user(
                            ui,
                            ov.site_id.as_deref(),
                            &ov.issue_key,
                            choice.cloned(),
                        );
                    }
                }
                Overlay::IssueForm(ov) => {
                    let draft_key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.to_draft_json());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_issue_form(ui, &ov, Some(draft_key));
                }
                Overlay::PageForm(ov) => {
                    let draft_key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.to_draft_json());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_page_form(ui, &ov, Some(draft_key));
                }
                Overlay::Comment(ov) => {
                    let draft_key = app.scoped_draft_key(ov.site_id.as_deref(), ov.draft_key());
                    if ov.has_unsent_content() {
                        app.drafts.set(draft_key.clone(), ov.draft_text());
                        let _ = app.save_drafts();
                    }
                    app.request_submit_comment(ui, &ov, Some(draft_key));
                }
                Overlay::CrossLink(ov) => {
                    if let Some(target) = ov.selected_target().cloned() {
                        app.jump_to_cross_ref_for_site(ui, ov.site_id.as_deref(), target);
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
                        app.request_upload_file(ui, ov.site_id.as_deref(), ov.target, path);
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

fn record_click(ui: &mut UiState, target: HitTarget) -> bool {
    const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);
    let now = Instant::now();
    if let Some((prev_time, prev_target)) = ui.last_click {
        if prev_target == target && now.duration_since(prev_time) <= DOUBLE_CLICK_WINDOW {
            ui.last_click = None;
            return true;
        }
    }
    ui.last_click = Some((now, target));
    false
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
            HitTarget::BoardItem(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.board_ui.nav_section = 0;
                ui.board_ui.selected_board = idx;
            }
            HitTarget::SprintItem(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.board_ui.nav_section = 1;
                ui.board_ui.selected_sprint = idx;
            }
            HitTarget::FilterItem(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.issue_ui.nav_section = 0;
                ui.issue_ui.selected_filter = idx;
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
            HitTarget::SpaceItem(idx) => {
                ui.focus = PaneFocus::Nav;
                ui.confluence_ui.nav_section = 0;
                ui.confluence_ui.selected_space = idx;
            }
            HitTarget::TreeRow(idx) | HitTarget::TreeChevron(idx) => {
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
            HitTarget::SearchHit(idx) => {
                ui.focus = PaneFocus::Main;
                ui.search_ui.selected_hit = idx;
            }
            HitTarget::DirectoryProject(idx) => {
                ui.directory_ui.section = 0;
                ui.directory_ui.selected_project = idx;
            }
            HitTarget::DirectorySpace(idx) => {
                ui.directory_ui.section = 1;
                ui.directory_ui.selected_space = idx;
            }
            HitTarget::DetailPane => {
                ui.focus = if ui.active_tab == WorkspaceTab::Board {
                    PaneFocus::Detail
                } else {
                    PaneFocus::Main
                };
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
        ui.issue_ui.local_filter_active = false;
        ui.confluence_ui.space_filter_active = false;
        return false;
    };
    let is_double_click = record_click(ui, target);
    if !matches!(
        target,
        HitTarget::SpaceItem(_)
            | HitTarget::SiteGroupHeader(_)
            | HitTarget::Button(ButtonId::FocusLocalFilter)
    ) {
        ui.confluence_ui.space_filter_active = false;
    }
    if !matches!(target, HitTarget::Button(ButtonId::FocusLocalFilter)) {
        ui.issue_ui.local_filter_active = false;
    }
    match target {
        HitTarget::Tab(idx) => switch_tab(WorkspaceTab::from_index(idx), app, ui),
        HitTarget::BoardNavBoards => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 0;
        }
        HitTarget::BoardNavSprints => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 1;
        }
        HitTarget::BoardItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 0;
            ui.board_ui.selected_board = idx;
            ui.board_ui.selected_sprint = 0;
            ui.board_ui.col_scroll = 0;
            ui.board_ui.user_moved_board = true;
            app.request_board_load(ui, idx, None);
        }
        HitTarget::SprintItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.board_ui.nav_section = 1;
            ui.board_ui.selected_sprint = idx;
            if let Some(sp) = app.sprints.get(idx) {
                app.request_board_load(ui, ui.board_ui.selected_board, Some(sp.id));
            }
        }
        HitTarget::KanbanColumn(c) => {
            ui.focus = PaneFocus::Main;
            ui.board_ui.selected_col = c;
            app.clamp_selections(ui);
        }
        HitTarget::KanbanCard { col: c, row: r } => {
            ui.focus = PaneFocus::Main;
            ui.board_ui.selected_col = c;
            ui.board_ui.selected_row = r;
            ui.board_ui.drawer_open = true;
            let site_id = app.active_board_site_id(ui).map(str::to_string);
            if let Some(issue) = app.selected_board_issue(ui) {
                if is_double_click {
                    let key = issue.key.clone();
                    let label = Some(issue.summary.clone());
                    app.jump_to_cross_ref_for_site(
                        ui,
                        site_id.as_deref(),
                        CrossRefTarget::JiraIssue { key, label },
                    );
                } else {
                    app.request_issue_detail_for_site(ui, site_id.as_deref(), &issue.key);
                }
            }
        }
        HitTarget::IssueNavFilters => {
            ui.focus = PaneFocus::Nav;
            ui.issue_ui.nav_section = 0;
        }
        HitTarget::IssueNavIssues => {
            ui.focus = PaneFocus::Nav;
            ui.issue_ui.nav_section = 1;
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
        HitTarget::MyWorkIssuesPane => {
            ui.focus = PaneFocus::Main;
            ui.my_work_ui.active_column = 0;
        }
        HitTarget::MyWorkPagesPane => {
            ui.focus = PaneFocus::Main;
            ui.my_work_ui.active_column = 1;
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
        HitTarget::WikiNavSpaces => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 0;
        }
        HitTarget::WikiNavTree => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 1;
        }
        HitTarget::SpaceItem(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 0;
            ui.confluence_ui.selected_space = idx;
            ui.confluence_ui.user_moved_space = true;
            ui.confluence_ui.selected_tree_row = 0;
            let site_id = app.space_site_ids.get(idx).cloned();
            if let Some(sp) = app.spaces.get(idx).cloned() {
                app.request_space_root_pages_for_site(ui, site_id.as_deref(), &sp.key);
            }
        }
        HitTarget::TreeChevron(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 1;
            ui.confluence_ui.selected_tree_row = idx;
            app.toggle_page_expand_only(ui, idx);
        }
        HitTarget::TreeRow(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 1;
            ui.confluence_ui.selected_tree_row = idx;
            let site_id = app.active_space_site_id(ui).map(str::to_string);
            let tree = app.current_confluence_tree(ui);
            if let Some(row) = tree.get(idx) {
                let page = row.page.clone();
                let has_children = row.has_children;
                app.request_page_detail_for_site(ui, site_id.as_deref(), &page.id);
                if is_double_click && has_children {
                    app.toggle_expand_page(ui, &page);
                }
            }
        }
        HitTarget::TocPane => {
            ui.focus = PaneFocus::Detail;
        }
        HitTarget::TocItem(idx) => {
            ui.focus = PaneFocus::Detail;
            ui.confluence_ui.selected_toc = idx;
            if let Some(page) = &app.active_page {
                if let Some(entry) = page.toc.iter().filter(|t| t.level <= 4).nth(idx) {
                    let max = ui.confluence_ui.reader_max_scroll;
                    let target_line = entry.line_hint as u16;
                    ui.confluence_ui.reader_scroll =
                        if max > 0 { target_line.min(max) } else { target_line };
                }
            }
        }
        HitTarget::SearchResultsPane => {
            ui.focus = PaneFocus::Main;
        }
        HitTarget::SearchHit(idx) => {
            ui.focus = PaneFocus::Main;
            ui.search_ui.selected_hit = idx;
            activate_selection(app, ui);
        }
        HitTarget::DirectoryProjectsPane => {
            ui.focus = PaneFocus::Main;
            ui.directory_ui.section = 0;
        }
        HitTarget::DirectorySpacesPane => {
            ui.focus = PaneFocus::Main;
            ui.directory_ui.section = 1;
        }
        HitTarget::DirectorySitesPane => {
            ui.focus = PaneFocus::Main;
        }
        HitTarget::DirectorySiteRow(idx) => {
            ui.focus = PaneFocus::Main;
            ui.directory_ui.selected_site = idx;
            if is_double_click {
                let rows = app.build_connected_site_rows();
                if let Some(site_row) = rows.get(idx) {
                    app.apply_site_switcher_command(
                        crate::ui::overlays::site_switcher::SiteSwitcherCommand::SelectSite(
                            site_row.site_id.clone(),
                        ),
                        ui,
                    );
                }
            }
        }
        HitTarget::DirectoryProject(idx) => {
            ui.focus = PaneFocus::Main;
            ui.directory_ui.section = 0;
            ui.directory_ui.selected_project = idx;
            if is_double_click {
                activate_selection(app, ui);
            }
        }
        HitTarget::DirectorySpace(idx) => {
            ui.focus = PaneFocus::Main;
            ui.directory_ui.section = 1;
            ui.directory_ui.selected_space = idx;
            if is_double_click {
                activate_selection(app, ui);
            }
        }
        HitTarget::DetailSubtask(idx) => {
            let site_id = app.active_issue_site_id.clone();
            if let Some(sub) = app
                .active_issue
                .as_ref()
                .and_then(|d| d.subtasks.get(idx))
                .cloned()
            {
                app.jump_to_cross_ref_for_site(
                    ui,
                    site_id.as_deref(),
                    CrossRefTarget::JiraIssue {
                        key: sub.key,
                        label: Some(sub.summary),
                    },
                );
            }
        }
        HitTarget::DetailIssueLink(idx) => {
            let site_id = app.active_issue_site_id.clone();
            if let Some(link) = app
                .active_issue
                .as_ref()
                .and_then(|d| d.linked_issues.get(idx))
                .cloned()
            {
                app.jump_to_cross_ref_for_site(
                    ui,
                    site_id.as_deref(),
                    CrossRefTarget::JiraIssue {
                        key: link.key,
                        label: Some(link.summary),
                    },
                );
            }
        }
        HitTarget::DetailConfluenceLink(idx) => {
            let site_id = app.active_issue_site_id.clone();
            if let Some(link) = app
                .active_issue
                .as_ref()
                .and_then(|d| d.linked_pages.get(idx))
                .cloned()
            {
                if !link.page_id.trim().is_empty() {
                    app.jump_to_cross_ref_for_site(
                        ui,
                        site_id.as_deref(),
                        CrossRefTarget::ConfluencePage {
                            page_id: link.page_id,
                            title: Some(link.title),
                        },
                    );
                } else if !link.web_url.trim().is_empty() {
                    if let Err(err) = open::that(&link.web_url) {
                        ui.error(format!("Failed to open browser: {err}"));
                    }
                }
            }
        }
        HitTarget::WikiBreadcrumb(idx) => {
            let site_id = app.active_page_site_id.clone();
            if let Some(anc) = app
                .active_page
                .as_ref()
                .and_then(|p| p.ancestors.get(idx))
                .cloned()
            {
                app.jump_to_cross_ref_for_site(
                    ui,
                    site_id.as_deref(),
                    CrossRefTarget::ConfluencePage {
                        page_id: anc.id,
                        title: Some(anc.title),
                    },
                );
            }
        }
        HitTarget::JqlBar => {
            ui.focus = PaneFocus::JqlBar;
            ui.issue_ui.local_filter_active = false;
        }
        HitTarget::SearchBar => {
            ui.focus = PaneFocus::SearchBar;
        }
        HitTarget::DetailPane => {
            ui.focus = if ui.active_tab == WorkspaceTab::Board {
                PaneFocus::Detail
            } else {
                PaneFocus::Main
            };
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
                app.clamp_selections(ui);
            }
        }
        HitTarget::Button(btn) => match btn {
            ButtonId::Quit => return true,
            ButtonId::RunJql => {
                ui.issue_ui.local_filter_active = false;
                let jql = ui.issue_ui.jql.text.trim().to_string();
                if !jql.is_empty() {
                    app.request_jql_search(ui, jql, "Custom JQL".to_string());
                }
                ui.focus = PaneFocus::Main;
            }
            ButtonId::FocusLocalFilter => match ui.active_tab {
                WorkspaceTab::Issues => {
                    ui.focus = PaneFocus::Nav;
                    ui.issue_ui.local_filter_active = true;
                }
                WorkspaceTab::Confluence => {
                    ui.focus = PaneFocus::Nav;
                    ui.confluence_ui.nav_section = 0;
                    ui.confluence_ui.space_filter_active = true;
                }
                _ => {
                    switch_tab(WorkspaceTab::Search, app, ui);
                }
            },
            ButtonId::CloseLocalFilter => {
                if ui.active_tab == WorkspaceTab::Issues {
                    ui.issue_ui.local_filter_active = false;
                    ui.issue_ui.local_filter.clear();
                    ui.focus = PaneFocus::Nav;
                    app.clamp_selections(ui);
                } else if ui.active_tab == WorkspaceTab::Confluence {
                    ui.confluence_ui.space_filter_active = false;
                    ui.confluence_ui.space_filter.clear();
                    app.keep_space_selection_visible(ui);
                }
            }
            ButtonId::BoardColPrev => {
                let prev_focus = ui.focus;
                ui.focus = PaneFocus::Main;
                move_direction(app, ui, 0, -1);
                ui.focus = prev_focus;
            }
            ButtonId::BoardColNext => {
                let prev_focus = ui.focus;
                ui.focus = PaneFocus::Main;
                move_direction(app, ui, 0, 1);
                ui.focus = prev_focus;
            }
            other => {
                if other != ButtonId::FocusJql {
                    ui.issue_ui.local_filter_active = false;
                    ui.confluence_ui.space_filter_active = false;
                    if matches!(ui.focus, PaneFocus::JqlBar | PaneFocus::SearchBar) {
                        ui.focus = PaneFocus::Main;
                    }
                }
                return dispatch(crate::ui::event::button_to_action(other), app, ui);
            }
        },
        HitTarget::OverlayItem(_)
        | HitTarget::OverlayItemToggle(_)
        | HitTarget::OverlayField(_)
        | HitTarget::OverlayScrollArea
        | HitTarget::OverlayBody
        | HitTarget::OverlayClose => {}
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
                        ui.board_ui.user_moved_board = true;
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
                    ui.board_ui.detail_scroll = apply_scroll_clamped(
                        ui.board_ui.detail_scroll,
                        dy * 2,
                        ui.board_ui.detail_max_scroll,
                    );
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
                    ui.issue_ui.detail_scroll = apply_scroll_clamped(
                        ui.issue_ui.detail_scroll,
                        dy * 2,
                        ui.issue_ui.detail_max_scroll,
                    );
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
                        step_space_selection(app, ui, dy);
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
                    ui.confluence_ui.reader_scroll = apply_scroll_clamped(
                        ui.confluence_ui.reader_scroll,
                        dy * 2,
                        ui.confluence_ui.reader_max_scroll,
                    );
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
        // Hover-targeted left-nav scrolling for all 2-section navs.
        Some(HitTarget::BoardNavBoards | HitTarget::BoardItem(_)) => {
            let len = app.boards.len();
            ui.board_ui.selected_board = step_index(ui.board_ui.selected_board, step, len);
            ui.board_ui.user_moved_board = true;
        }
        Some(HitTarget::BoardNavSprints | HitTarget::SprintItem(_)) => {
            let len = app.sprints.len();
            ui.board_ui.selected_sprint = step_index(ui.board_ui.selected_sprint, step, len);
        }
        Some(HitTarget::IssueNavFilters | HitTarget::FilterItem(_)) => {
            let len = app.saved_filters.len();
            ui.issue_ui.selected_filter = step_index(ui.issue_ui.selected_filter, step, len);
        }
        Some(HitTarget::IssueNavIssues | HitTarget::IssueRow(_)) => {
            let len = app.filtered_issues(ui).len();
            ui.issue_ui.selected_issue = step_index(ui.issue_ui.selected_issue, step, len);
        }
        Some(HitTarget::WikiNavSpaces | HitTarget::SpaceItem(_)) => {
            step_space_selection(app, ui, step);
        }
        Some(HitTarget::WikiNavTree | HitTarget::TreeRow(_) | HitTarget::TreeChevron(_)) => {
            let len = app.current_confluence_tree(ui).len();
            ui.confluence_ui.selected_tree_row =
                step_index(ui.confluence_ui.selected_tree_row, step, len);
        }
        Some(HitTarget::SiteGroupHeader(_)) => match ui.active_tab {
            WorkspaceTab::Confluence => step_space_selection(app, ui, step),
            WorkspaceTab::Board => {
                let len = app.boards.len();
                ui.board_ui.selected_board = step_index(ui.board_ui.selected_board, step, len);
                ui.board_ui.user_moved_board = true;
            }
            WorkspaceTab::Issues => {
                let len = app.filtered_issues(ui).len();
                ui.issue_ui.selected_issue = step_index(ui.issue_ui.selected_issue, step, len);
            }
            _ => {}
        },
        Some(HitTarget::NavPane) => {
            let prev_focus = ui.focus;
            ui.focus = PaneFocus::Nav;
            move_direction(app, ui, step, 0);
            ui.focus = prev_focus;
        }
        // Hover-targeted Kanban column scrolling (scrolls the hovered column, never the drawer).
        Some(HitTarget::KanbanColumn(c) | HitTarget::KanbanCard { col: c, .. }) => {
            let cols = app.build_kanban_columns(ui);
            if let Some(col_data) = cols.get(c) {
                ui.board_ui.selected_col = c;
                ui.board_ui.selected_row = step_index(
                    ui.board_ui.selected_row.min(col_data.issues.len().saturating_sub(1)),
                    step,
                    col_data.issues.len(),
                );
            }
        }
        // Hover-targeted TOC drawer scrolling.
        Some(HitTarget::TocPane | HitTarget::TocItem(_)) => {
            if let Some(page) = &app.active_page {
                let len = page.toc.iter().filter(|t| t.level <= 4).count();
                ui.confluence_ui.selected_toc =
                    step_index(ui.confluence_ui.selected_toc, step, len);
            }
        }
        // Hover-targeted My Work column scrolling.
        Some(HitTarget::MyWorkIssuesPane | HitTarget::MyWorkIssue(_)) => {
            let len = app.my_issues.len();
            ui.my_work_ui.selected_issue = step_index(ui.my_work_ui.selected_issue, step, len);
        }
        Some(HitTarget::MyWorkPagesPane | HitTarget::MyWorkPage(_)) => {
            let len = app.my_pages.len();
            ui.my_work_ui.selected_page = step_index(ui.my_work_ui.selected_page, step, len);
        }
        // Hover-targeted Search results scrolling.
        Some(HitTarget::SearchResultsPane | HitTarget::SearchHit(_)) => {
            let len = app.search_hits.len();
            ui.search_ui.selected_hit = step_index(ui.search_ui.selected_hit, step, len);
        }
        // Hover-targeted Directory section scrolling.
        Some(HitTarget::DirectoryProjectsPane | HitTarget::DirectoryProject(_)) => {
            let len = app.projects.len();
            ui.directory_ui.selected_project =
                step_index(ui.directory_ui.selected_project, step, len);
        }
        Some(HitTarget::DirectorySpacesPane | HitTarget::DirectorySpace(_)) => {
            let len = app.spaces.len();
            ui.directory_ui.selected_space = step_index(ui.directory_ui.selected_space, step, len);
        }
        Some(HitTarget::DirectorySitesPane | HitTarget::DirectorySiteRow(_)) => {
            let len = app.build_connected_site_rows().len();
            ui.directory_ui.selected_site = step_index(ui.directory_ui.selected_site, step, len);
        }
        // Detail / reader pane and clickable content inside it.
        Some(
            HitTarget::DetailPane
            | HitTarget::Attachment(_)
            | HitTarget::DetailSubtask(_)
            | HitTarget::DetailIssueLink(_)
            | HitTarget::DetailConfluenceLink(_)
            | HitTarget::WikiBreadcrumb(_),
        ) => {
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
            ui.board_ui.detail_scroll = apply_scroll_clamped(
                ui.board_ui.detail_scroll,
                delta,
                ui.board_ui.detail_max_scroll,
            );
        }
        WorkspaceTab::Issues => {
            ui.issue_ui.detail_scroll = apply_scroll_clamped(
                ui.issue_ui.detail_scroll,
                delta,
                ui.issue_ui.detail_max_scroll,
            );
        }
        WorkspaceTab::Confluence => {
            ui.confluence_ui.reader_scroll = apply_scroll_clamped(
                ui.confluence_ui.reader_scroll,
                delta,
                ui.confluence_ui.reader_max_scroll,
            );
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
                    app.request_board_load(ui, ui.board_ui.selected_board, None);
                } else if let Some(sp) = app.sprints.get(ui.board_ui.selected_sprint) {
                    app.request_board_load(ui, ui.board_ui.selected_board, Some(sp.id));
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
                    open_selected_space(app, ui);
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
                let row = ui.directory_ui.selected_project;
                if let Some(proj) = app.projects.get(row).cloned() {
                    let site_id = app.project_row_site(row);
                    ui.active_tab = WorkspaceTab::Issues;
                    let jql = format!("project = {} ORDER BY updated DESC", proj.key);
                    ui.issue_ui.jql.set_text(jql.clone());
                    app.request_jql_search_for_site(ui, Some(&site_id), jql, proj.key);
                }
            } else {
                // Directory and SPACES list the same `spaces` rows, so the row index (not the
                // key, which two sites can share) carries the selection over.
                let row = ui.directory_ui.selected_space;
                if let Some(sp) = app.spaces.get(row).cloned() {
                    let site_id = app.space_row_site(row);
                    ui.confluence_ui.selected_space = row;
                    ui.confluence_ui.user_moved_space = true;
                    ui.active_tab = WorkspaceTab::Confluence;
                    app.request_space_root_pages_for_site(ui, Some(&site_id), &sp.key);
                }
            }
        }
    }
}

fn open_transition(app: &AppState, ui: &mut UiState) {
    if let Some((site, key)) = app.selected_issue_ref(ui) {
        app.request_open_transitions(ui, Some(&site), &key);
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
            let site = app.active_page_site_id.clone();
            let key = app.scoped_draft_key(site.as_deref(), target.draft_key());
            let overlay = if let Some(saved) = app.drafts.get(&key) {
                ui.info("Restored saved comment draft");
                CommentOverlay::with_draft(target, saved)
            } else {
                CommentOverlay::new(target)
            }
            .with_site(site);
            ui.push_overlay(Overlay::Comment(overlay));
            app.request_comments_refresh(ui);
            return;
        }
    }
    if let Some((site, key)) = app.selected_issue_ref(ui) {
        let target = CommentTarget::Issue(key);
        let draft_key = app.scoped_draft_key(Some(&site), target.draft_key());
        let overlay = if let Some(saved) = app.drafts.get(&draft_key) {
            ui.info("Restored saved comment draft");
            CommentOverlay::with_draft(target, saved)
        } else {
            CommentOverlay::new(target)
        }
        .with_site(Some(site));
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
        // The opened page is only a valid parent when it lives in the selected
        // space on the selected space's site; otherwise create at the space root.
        let space_site = app.tree_site(ui);
        let parent_id = app
            .active_page
            .as_ref()
            .filter(|p| p.summary.space_key.eq_ignore_ascii_case(&space_key))
            .filter(|_| {
                app.active_page_site_id
                    .as_deref()
                    .is_none_or(|s| s.eq_ignore_ascii_case(&space_site))
            })
            .map(|p| p.summary.id.clone());
        let mut form = PageFormOverlay::new_page(space_key, parent_id);
        // Personal space warning (ENT-PERSONAL): the selected space's name and type,
        // and whether it is the signed-in user's own space on that site.
        let selected = app
            .active_space(ui)
            .filter(|s| s.key.eq_ignore_ascii_case(&form.space_key));
        form.space_name = selected
            .map(|s| s.name.trim().to_string())
            .filter(|n| !n.is_empty());
        form.personal_space |= selected.is_some_and(is_personal_space);
        if form.personal_space {
            form.personal_owner_is_me =
                app.personal_space_owner_is_me(&space_site, &form.space_key);
        }
        form.site_id = Some(space_site);
        let draft_key = app.scoped_draft_key(form.site_id.as_deref(), form.draft_key());
        if let Some(saved) = app.drafts.get(&draft_key) {
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
        form.site_id = app.selected_issue_ref(ui).map(|(site, _)| site);
        let draft_key = app.scoped_draft_key(form.site_id.as_deref(), form.draft_key());
        if let Some(saved) = app.drafts.get(&draft_key) {
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
            form.site_id = app.active_page_site_id.clone();
            let draft_key = app.scoped_draft_key(form.site_id.as_deref(), form.draft_key());
            if let Some(saved) = app.drafts.get(&draft_key) {
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
        form.site_id = app.active_issue_site_id.clone();
        let draft_key = app.scoped_draft_key(form.site_id.as_deref(), form.draft_key());
        if let Some(saved) = app.drafts.get(&draft_key) {
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
    if let Some((site, target)) = app.active_upload_ref(ui) {
        let mut overlay = PathInputOverlay::new(target);
        overlay.site_id = site;
        ui.push_overlay(Overlay::PathInput(overlay));
    } else {
        ui.info("Select an issue or page to attach a file first");
    }
}

fn open_file_picker(app: &AppState, ui: &mut UiState) {
    if let Some((site, target)) = app.active_upload_ref(ui) {
        let mut overlay = FilePickerOverlay::new(target);
        overlay.site_id = site;
        ui.push_overlay(Overlay::FilePicker(overlay));
    } else {
        ui.info("Select an issue or page to attach a file first");
    }
}

fn try_paste_clipboard_image(
    target: Option<(Option<String>, UploadTarget)>,
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
            if let Some((site, upload_target)) = target {
                let target_label = match &upload_target {
                    UploadTarget::Issue(k) => format!("Issue {k}"),
                    UploadTarget::Page(id) => format!("Page #{id}"),
                };
                app.request_upload_file(ui, site.as_deref(), upload_target, path);
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
            if html_preview::is_previewable_markup_attachment(&att.mime_type, &att.filename) {
                open_html_attachment_preview(app, ui, label, att.clone());
                return;
            }
        }
    }
    if let Some(mut ov) = ImagePreviewOverlay::from_attachments(label.clone(), atts, preferred_idx) {
        ov.site_id = app.current_attachments_site(ui);
        ov.apply_graphics_caps(ui.graphics_caps);
        if app.config.image_protocol != ConfigImageProtocol::Auto {
            ov.protocol = ui.graphics_caps.protocol;
            ov.protocol_overridden = true;
        }
        ui.push_overlay(Overlay::ImagePreview(ov));
        app.request_load_preview_image(ui);
    } else if let Some(markup_att) = atts
        .iter()
        .find(|att| html_preview::is_previewable_markup_attachment(&att.mime_type, &att.filename))
        .cloned()
    {
        open_html_attachment_preview(app, ui, label, markup_att);
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
    let site = app.current_attachments_site(ui);
    app.request_load_html_attachment(ui, site.as_deref(), title, att);
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
            let row = ui.directory_ui.selected_project;
            let site_id = app.project_row_site(row);
            let toggled_key = if let Some(p) = app.projects.get_mut(row) {
                p.pinned = !p.pinned;
                let _ = app.storage.set_project_pinned_for_site(&site_id, &p.key, p.pinned);
                ui.info(format!(
                    "{} {}",
                    p.key,
                    if p.pinned { "pinned" } else { "unpinned" }
                ));
                Some(p.key.clone())
            } else {
                None
            };
            if let Some(key) = toggled_key {
                app.sort_directory_pinned();
                if let Some(new_idx) = app
                    .projects
                    .iter()
                    .enumerate()
                    .position(|(i, p)| p.key == key && app.project_row_site(i) == site_id)
                {
                    ui.directory_ui.selected_project = new_idx;
                }
                app.clamp_selections(ui);
            }
        } else {
            let row = ui.directory_ui.selected_space;
            let site_id = app.space_row_site(row);
            let Some(s) = app.spaces.get_mut(row) else {
                return;
            };
            s.pinned = !s.pinned;
            let key = s.key.clone();
            let _ = app.storage.set_space_pinned_for_site(&site_id, &key, s.pinned);
            ui.info(format!(
                "{} {}",
                key,
                if s.pinned { "pinned" } else { "unpinned" }
            ));
            app.sort_directory_pinned();
            if let Some(new_idx) = app
                .spaces
                .iter()
                .enumerate()
                .position(|(i, sp)| sp.key == key && app.space_row_site(i) == site_id)
            {
                ui.directory_ui.selected_space = new_idx;
            }
            app.clamp_selections(ui);
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

fn apply_scroll_clamped(cur: u16, delta: i32, max_scroll: u16) -> u16 {
    let next = apply_scroll(cur, delta);
    if max_scroll > 0 {
        next.min(max_scroll)
    } else {
        next
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
        app.handle_ui_msg(&mut ui, UiMsg::IssueActionFailed {
                task_label: "Create issue".into(),
                error: "HTTP 500".into(),
            });
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
        app.handle_ui_msg(&mut ui, UiMsg::PageActionFailed {
                task_label: "Create page".into(),
                error: "HTTP 409 Conflict".into(),
            });
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

    #[tokio::test]
    async fn markdown_attachment_end_to_end_preview_renders_and_toggles_raw() {
        use axum::{routing::get, Router};
        use bitefoundry_html::SourceKind;
        use ratatui::{backend::TestBackend, Terminal};
        use tokio::net::TcpListener;

        let router = Router::new().route(
            "/readme.md",
            get(|| async { "# Title\n\n**bold** | [link](https://example.com)\n" }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let base_url = format!("http://{addr}");

        let cfg = AppConfig {
            site_url: base_url.clone(),
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

        let mut issue = IssueDetail::default();
        issue.summary.key = "ATL-42".into();
        issue.attachments = vec![AttachmentItem {
            id: "att-md".into(),
            filename: "README.md".into(),
            mime_type: "application/octet-stream".into(),
            size: 64,
            author_name: Some("Alice".into()),
            created_at: String::new(),
            content_url: format!("{base_url}/readme.md"),
            thumbnail_url: None,
        }];
        app.active_issue = Some(issue);
        ui.active_tab = WorkspaceTab::Issues;

        // Open Attachments overlay and verify [MD] badge
        dispatch(Action::OpenAttachments, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::Attachments(ov)) => {
                let rows = ov.rows();
                assert_eq!(rows.len(), 1);
                assert!(rows[0].is_markdown);
                assert_eq!(rows[0].icon(), "[MD]");
            }
            other => panic!("expected Attachments overlay, got {other:?}"),
        }

        // Press `v` -> dispatches HtmlPreview loading overlay and fetches bytes
        dispatch(Action::Input('v'), &mut app, &mut ui);
        assert!(
            matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))),
            "pressing `v` on README.md must open Overlay::HtmlPreview"
        );

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let msg = tokio::time::timeout_at(deadline, ui_rx.recv())
                .await
                .expect("HtmlAttachmentLoaded timed out")
                .expect("ui_rx closed");
            let is_loaded = matches!(msg, crate::ui::app::UiMsg::HtmlAttachmentLoaded { .. });
            app.handle_ui_msg(&mut ui, msg);
            if is_loaded {
                break;
            }
        }

        let backend = TestBackend::new(96, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        let rendered_screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                let sess = ov.session().expect("expected loaded Markdown session");
                assert_eq!(sess.source_kind(), SourceKind::Markdown);
                assert!(!ov.is_js_active());
                assert!(
                    sess.serve_html().contains("<!DOCTYPE html>"),
                    "serve_html() should produce standalone HTML for `o`"
                );
            }
            other => panic!("expected HtmlPreview overlay, got {other:?}"),
        }
        assert!(rendered_screen.contains("Title"), "expected heading:\n{rendered_screen}");
        assert!(rendered_screen.contains("bold"), "expected bold text:\n{rendered_screen}");
        assert!(rendered_screen.contains("link"), "expected link text:\n{rendered_screen}");
        assert!(
            !rendered_screen.contains("**"),
            "raw `**` leaked into rendered Markdown preview:\n{rendered_screen}"
        );

        // Press `r` to toggle Raw Markdown mode -> shows raw `# Title`
        dispatch(Action::Input('r'), &mut app, &mut ui);
        assert!(
            matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(ov)) if ov.is_raw_mode()),
            "pressing `r` must toggle raw mode"
        );
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        let raw_screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            raw_screen.contains("# Title"),
            "expected raw `# Title` after pressing `r`:\n{raw_screen}"
        );
    }

    fn key_action(ui: &UiState, code: crossterm::event::KeyCode) -> Option<Action> {
        crate::ui::event::key_to_action(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
    }

    /// Confluence tab, SPACES section focused, three spaces (one personal).
    fn spaces_section(app: &mut AppState, ui: &mut UiState) {
        let sp = |key: &str, name: &str, space_type: &str| SpaceSummary {
            id: format!("id-{key}"),
            key: key.into(),
            name: name.into(),
            space_type: space_type.into(),
            ..Default::default()
        };
        app.spaces = vec![
            sp("ENG", "Engineering", "global"),
            sp("OPS", "Operations", "global"),
            sp("~jdoe", "John Doe", "personal"),
        ];
        ui.active_tab = WorkspaceTab::Confluence;
        ui.focus = PaneFocus::Nav;
        ui.confluence_ui.nav_section = 0;
        ui.confluence_ui.selected_space = 0;
    }

    #[tokio::test]
    async fn slash_activates_space_filter_only_in_spaces_section() {
        use crossterm::event::KeyCode;
        let (mut app, mut ui, _dir) = test_app_and_ui();
        spaces_section(&mut app, &mut ui);

        // Page tree section: `/` keeps opening unified search.
        ui.confluence_ui.nav_section = 1;
        let slash = key_action(&ui, KeyCode::Char('/')).unwrap();
        assert_eq!(slash, Action::FocusFilter);
        dispatch(slash, &mut app, &mut ui);
        assert!(!ui.confluence_ui.space_filter_active);
        assert_eq!(ui.active_tab, WorkspaceTab::Search);

        // Reader focus with the SPACES section remembered: still unified search.
        spaces_section(&mut app, &mut ui);
        ui.focus = PaneFocus::Main;
        dispatch(Action::FocusFilter, &mut app, &mut ui);
        assert!(!ui.confluence_ui.space_filter_active);
        assert_eq!(ui.active_tab, WorkspaceTab::Search);

        // SPACES section focused: `/` opens the inline space filter.
        spaces_section(&mut app, &mut ui);
        dispatch(
            key_action(&ui, KeyCode::Char('/')).unwrap(),
            &mut app,
            &mut ui,
        );
        assert!(ui.confluence_ui.space_filter_active);
        assert!(ui.accepts_text_input());
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);
        assert_eq!(ui.focus, PaneFocus::Nav);

        // Leaving the tab (a tab click) closes it.
        switch_tab(WorkspaceTab::Board, &mut app, &mut ui);
        assert!(!ui.confluence_ui.space_filter_active);
        assert!(!ui.accepts_text_input());
    }

    #[tokio::test]
    async fn esc_clears_then_closes_space_filter() {
        use crossterm::event::KeyCode;
        let (mut app, mut ui, _dir) = test_app_and_ui();
        spaces_section(&mut app, &mut ui);
        dispatch(Action::FocusFilter, &mut app, &mut ui);
        for ch in "ops".chars() {
            dispatch(
                key_action(&ui, KeyCode::Char(ch)).unwrap(),
                &mut app,
                &mut ui,
            );
        }
        assert_eq!(app.visible_space_indices(&ui), vec![1]);
        assert_eq!(
            ui.confluence_ui.selected_space, 1,
            "filtered-out selection moves to the match"
        );

        let esc = key_action(&ui, KeyCode::Esc).unwrap();
        assert_eq!(esc, Action::CloseOverlay);
        dispatch(esc, &mut app, &mut ui);
        assert_eq!(
            ui.confluence_ui.space_filter.text, "",
            "first Esc clears the query"
        );
        assert!(
            ui.confluence_ui.space_filter_active,
            "and keeps the filter open"
        );
        assert_eq!(app.visible_space_indices(&ui), vec![0, 1, 2]);

        dispatch(key_action(&ui, KeyCode::Esc).unwrap(), &mut app, &mut ui);
        assert!(
            !ui.confluence_ui.space_filter_active,
            "second Esc closes it"
        );
        assert!(!ui.accepts_text_input());
        assert_eq!(ui.confluence_ui.selected_space, 1, "selection is kept");
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);
        assert_eq!(ui.focus, PaneFocus::Nav);
    }

    #[tokio::test]
    async fn typing_in_space_filter_does_not_trigger_shortcuts() {
        use crossterm::event::KeyCode;
        let (mut app, mut ui, _dir) = test_app_and_ui();
        spaces_section(&mut app, &mut ui);
        dispatch(
            key_action(&ui, KeyCode::Char('/')).unwrap(),
            &mut app,
            &mut ui,
        );

        // `n` new page, `q` quit, `m` transition, `r` sync, `1` tab, `e` edit.
        for ch in "nqmr1e".chars() {
            let act = key_action(&ui, KeyCode::Char(ch)).unwrap();
            assert_eq!(act, Action::Input(ch));
            assert!(!dispatch(act, &mut app, &mut ui), "`{ch}` must not quit");
        }
        assert_eq!(ui.confluence_ui.space_filter.text, "nqmr1e");
        assert!(ui.overlays.is_empty(), "no form or overlay opened");
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);
        assert!(!app.syncing, "`r` did not start a sync");

        // Up/Down move within the matches; Enter opens the selection and keeps the query.
        for _ in 0..6 {
            dispatch(
                key_action(&ui, KeyCode::Backspace).unwrap(),
                &mut app,
                &mut ui,
            );
        }
        dispatch(
            key_action(&ui, KeyCode::Char('o')).unwrap(),
            &mut app,
            &mut ui,
        );
        assert_eq!(
            app.visible_space_indices(&ui),
            vec![1, 2],
            "OPS, then John Doe"
        );
        dispatch(key_action(&ui, KeyCode::Down).unwrap(), &mut app, &mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 2);
        dispatch(key_action(&ui, KeyCode::Up).unwrap(), &mut app, &mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 1);
        dispatch(key_action(&ui, KeyCode::Enter).unwrap(), &mut app, &mut ui);
        assert!(!ui.confluence_ui.space_filter_active);
        assert_eq!(ui.confluence_ui.space_filter.text, "o");
        assert!(
            ui.tasks.running().any(|t| t.label == "Space OPS"),
            "OPS is loading"
        );

        // Closed: letters are shortcuts again, and j/k still step within the matches.
        assert_eq!(key_action(&ui, KeyCode::Char('n')), Some(Action::NewItem));
        dispatch(Action::MoveDown, &mut app, &mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 2);
        dispatch(Action::MoveDown, &mut app, &mut ui);
        assert_eq!(ui.confluence_ui.selected_space, 2, "ENG is filtered out");
    }

    #[tokio::test]
    async fn new_page_form_flags_personal_space_owner() {
        let (mut app, mut ui, _dir) = test_app_and_ui();
        spaces_section(&mut app, &mut ui);
        app.spaces.push(SpaceSummary {
            id: "id-asmith".into(),
            key: "~asmith".into(),
            name: "Anna Smith".into(),
            space_type: "personal".into(),
            ..Default::default()
        });
        app.active_page = None;

        let open = |app: &mut AppState, ui: &mut UiState, idx: usize| {
            ui.confluence_ui.selected_space = idx;
            dispatch(Action::NewItem, app, ui);
            let form = match ui.overlays.last() {
                Some(Overlay::PageForm(f)) => (
                    f.personal_space,
                    f.personal_owner_is_me,
                    f.space_name.clone(),
                ),
                other => panic!("expected page form, got {other:?}"),
            };
            ui.pop_overlay();
            form
        };

        assert_eq!(
            open(&mut app, &mut ui, 0),
            (false, None, Some("Engineering".into()))
        );
        assert_eq!(
            open(&mut app, &mut ui, 2),
            (true, None, Some("John Doe".into())),
            "no cached identity: owner unknown"
        );
        app.me = Some(crate::models::UserSummary {
            id: "JDOE".into(),
            display_name: "John Doe".into(),
            ..Default::default()
        });
        assert_eq!(
            open(&mut app, &mut ui, 2).1,
            Some(true),
            "own space, case-insensitive"
        );
        assert_eq!(
            open(&mut app, &mut ui, 3).1,
            Some(false),
            "someone else's space"
        );
    }

    #[tokio::test]
    async fn buttons_work_while_inline_text_input_is_active() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Tab 5 Search defaults to SearchBar (accepts_text_input() == true).
        switch_tab(WorkspaceTab::Search, &mut app, &mut ui);
        assert!(ui.accepts_text_input());
        ui.hitmap.clear();
        ui.hitmap
            .add_line(10, 0, 12, HitTarget::Button(ButtonId::SiteSwitcher));

        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let action = crate::ui::event::mouse_to_action(ev, &ui.hitmap).unwrap();
        dispatch(action, &mut app, &mut ui);
        assert!(
            matches!(ui.overlays.last(), Some(Overlay::SiteSwitcher(_))),
            "clicking SiteSwitcher button must work while SearchBar is focused"
        );
        ui.pop_overlay();

        // Issues local filter active: clicking CommandPalette button must open it & clear filter mode.
        switch_tab(WorkspaceTab::Issues, &mut app, &mut ui);
        ui.issue_ui.local_filter_active = true;
        assert!(ui.accepts_text_input());
        ui.hitmap.clear();
        ui.hitmap
            .add_line(20, 0, 14, HitTarget::Button(ButtonId::CommandPalette));
        let ev2 = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 22,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let action2 = crate::ui::event::mouse_to_action(ev2, &ui.hitmap).unwrap();
        dispatch(action2, &mut app, &mut ui);
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::CommandPalette(_))
        ));
        assert!(!ui.issue_ui.local_filter_active);
    }

    #[tokio::test]
    async fn board_detail_click_focuses_detail_and_local_filter_clears_on_click_away() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Board tab: clicking DetailPane must focus PaneFocus::Detail (not Main).
        ui.active_tab = WorkspaceTab::Board;
        ui.focus = PaneFocus::Main;
        ui.board_ui.drawer_open = true;
        ui.hitmap.clear();
        ui.hitmap
            .add(Rect::new(60, 2, 40, 20), HitTarget::DetailPane);
        dispatch(Action::MouseClick { row: 5, col: 70 }, &mut app, &mut ui);
        assert_eq!(ui.focus, PaneFocus::Detail);

        // Issues tab: clicking an IssueRow while local_filter_active is true must clear it.
        ui.active_tab = WorkspaceTab::Issues;
        ui.issue_ui.local_filter_active = true;
        ui.hitmap.clear();
        ui.hitmap.add_line(2, 5, 30, HitTarget::IssueRow(0));
        dispatch(Action::MouseClick { row: 5, col: 10 }, &mut app, &mut ui);
        assert!(!ui.issue_ui.local_filter_active);
        assert_eq!(ui.focus, PaneFocus::Nav);
    }

    #[tokio::test]
    async fn hover_targeted_scroll_targets_hovered_pane_across_tabs() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Board: hover over KanbanColumn(1) while drawer is open and selected_col == 0.
        app.board_issues = vec![
            IssueSummary {
                key: "ATL-1".into(),
                summary: "Todo 1".into(),
                status: crate::models::IssueStatus {
                    id: "1".into(),
                    name: "To Do".into(),
                    category: crate::models::StatusCategory::Todo,
                },
                ..Default::default()
            },
            IssueSummary {
                key: "ATL-2".into(),
                summary: "Prog 1".into(),
                status: crate::models::IssueStatus {
                    id: "2".into(),
                    name: "In Progress".into(),
                    category: crate::models::StatusCategory::InProgress,
                },
                ..Default::default()
            },
            IssueSummary {
                key: "ATL-3".into(),
                summary: "Prog 2".into(),
                status: crate::models::IssueStatus {
                    id: "3".into(),
                    name: "In Progress".into(),
                    category: crate::models::StatusCategory::InProgress,
                },
                ..Default::default()
            },
        ];
        ui.active_tab = WorkspaceTab::Board;
        ui.board_ui.drawer_open = true;
        ui.board_ui.selected_col = 0;
        ui.board_ui.selected_row = 0;
        ui.board_ui.detail_scroll = 0;
        ui.hitmap.clear();
        ui.hitmap
            .add(Rect::new(30, 3, 25, 20), HitTarget::KanbanColumn(1));
        dispatch(
            Action::MouseScroll {
                delta: 3,
                row: 10,
                col: 40,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.board_ui.selected_col, 1);
        assert_eq!(ui.board_ui.selected_row, 1);
        assert_eq!(
            ui.board_ui.detail_scroll, 0,
            "scrolling KanbanColumn gap must NOT scroll the detail drawer"
        );

        // My Work: active_column is 0, hovering MyWorkPagesPane scrolls selected_page.
        app.my_pages = vec![
            PageSummary {
                id: "p1".into(),
                title: "Page 1".into(),
                ..Default::default()
            },
            PageSummary {
                id: "p2".into(),
                title: "Page 2".into(),
                ..Default::default()
            },
        ];
        ui.active_tab = WorkspaceTab::MyWork;
        ui.my_work_ui.active_column = 0;
        ui.my_work_ui.selected_page = 0;
        ui.hitmap.clear();
        ui.hitmap
            .add(Rect::new(50, 3, 40, 20), HitTarget::MyWorkPagesPane);
        dispatch(
            Action::MouseScroll {
                delta: 3,
                row: 6,
                col: 60,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.my_work_ui.selected_page, 1);
        assert_eq!(ui.my_work_ui.selected_issue, 0);
    }

    #[tokio::test]
    async fn right_click_context_menu_matches_my_work_page_and_wiki_tree() {
        let (mut app, mut ui, _dir) = test_app_and_ui();
        app.my_pages = vec![PageSummary {
            id: "pg-42".into(),
            title: "Runbook".into(),
            web_url: "https://acme.atlassian.net/wiki/pages/42".into(),
            ..Default::default()
        }];
        app.active_page = None;
        ui.active_tab = WorkspaceTab::MyWork;
        ui.hitmap.clear();
        ui.hitmap.add_line(50, 6, 30, HitTarget::MyWorkPage(0));

        dispatch(
            Action::MouseRightClick { row: 6, col: 55 },
            &mut app,
            &mut ui,
        );
        match ui.overlays.last() {
            Some(Overlay::ContextMenu(menu)) => {
                assert!(
                    matches!(
                        &menu.context,
                        crate::ui::overlays::context_menu::ContextTarget::ConfluencePage {
                            page_id,
                            ..
                        } if page_id == "pg-42"
                    ),
                    "expected ConfluencePage menu for MyWorkPage, got {:?}",
                    menu.context
                );
            }
            other => panic!("expected ContextMenu, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_dialog_buttons_and_backdrop_close_and_context_menu_body() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // ConfirmDialog: clicking OverlaySubmit dispatches on_confirm and closes.
        ui.push_overlay(Overlay::confirm(
            "Proceed?",
            Some(Action::ToggleHrefDisplay),
        ));
        assert!(!ui.show_hrefs);
        ui.hitmap.clear();
        ui.hitmap.add(Rect::new(10, 5, 40, 8), HitTarget::OverlayBody);
        ui.hitmap
            .add_line(14, 10, 15, HitTarget::Button(ButtonId::OverlaySubmit));
        dispatch(Action::MouseClick { row: 10, col: 16 }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());
        assert!(ui.show_hrefs);

        // ContextMenu: clicking OverlayBody (border/title) must NOT dismiss it.
        ui.push_overlay(Overlay::ContextMenu(ContextMenuOverlay::for_global((
            10, 5,
        ))));
        ui.hitmap.clear();
        ui.hitmap.add(Rect::new(10, 5, 30, 10), HitTarget::OverlayBody);
        dispatch(Action::MouseClick { row: 5, col: 12 }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_))));

        // Clicking outside OverlayBody dismisses any overlay (including Settings).
        ui.pop_overlay();
        ui.push_overlay(Overlay::Settings(SettingsOverlay::from_config(&app, &ui)));
        ui.hitmap.clear();
        ui.hitmap.add(Rect::new(20, 5, 40, 15), HitTarget::OverlayBody);
        dispatch(Action::MouseClick { row: 1, col: 1 }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());
    }

    #[tokio::test]
    async fn tree_chevron_toggles_without_opening_and_directory_double_click_opens() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Wiki tree: TreeChevron expands without loading page; TreeRow does not collapse.
        spaces_section(&mut app, &mut ui);
        let site = app.tree_site(&ui);
        let parent = PageSummary {
            id: "p-parent".into(),
            space_key: "ENG".into(),
            title: "Architecture".into(),
            has_children: Some(true),
            ..Default::default()
        };
        let child = PageSummary {
            id: "p-child".into(),
            space_key: "ENG".into(),
            title: "Child Doc".into(),
            parent_id: Some("p-parent".into()),
            ..Default::default()
        };
        app.root_pages_by_space
            .insert(SiteKey::new(&site, "ENG"), vec![parent]);
        app.page_children_map
            .insert(SiteKey::new(&site, "p-parent"), vec![child]);
        ui.confluence_ui.nav_section = 1;
        ui.hitmap.clear();
        ui.hitmap.add_line(2, 6, 25, HitTarget::TreeRow(0));
        ui.hitmap.add_line(2, 6, 3, HitTarget::TreeChevron(0));

        // Click chevron -> expands!
        dispatch(Action::MouseClick { row: 6, col: 2 }, &mut app, &mut ui);
        assert!(app
            .expanded_pages
            .contains(&SiteKey::new(&site, "p-parent")));

        // Single-click title -> stays expanded (does NOT accidentally collapse!).
        dispatch(Action::MouseClick { row: 6, col: 10 }, &mut app, &mut ui);
        assert!(app
            .expanded_pages
            .contains(&SiteKey::new(&site, "p-parent")));

        // Directory: single-click selects project, double-click opens Issues with project JQL.
        app.projects = vec![
            crate::models::ProjectSummary {
                id: "1".into(),
                key: "AAA".into(),
                name: "Alpha".into(),
                ..Default::default()
            },
            crate::models::ProjectSummary {
                id: "2".into(),
                key: "BBB".into(),
                name: "Beta".into(),
                ..Default::default()
            },
        ];
        app.project_site_ids = vec!["default".into(), "default".into()];
        ui.active_tab = WorkspaceTab::Directory;
        ui.last_click = None;
        ui.hitmap.clear();
        ui.hitmap.add_line(2, 10, 30, HitTarget::DirectoryProject(1));

        dispatch(Action::MouseClick { row: 10, col: 5 }, &mut app, &mut ui);
        assert_eq!(ui.active_tab, WorkspaceTab::Directory);
        assert_eq!(ui.directory_ui.selected_project, 1);

        dispatch(Action::MouseClick { row: 10, col: 5 }, &mut app, &mut ui);
        assert_eq!(ui.active_tab, WorkspaceTab::Issues);
        assert!(ui.issue_ui.jql.text.contains("project = BBB"));
    }

    #[tokio::test]
    async fn custom_jql_protected_from_background_sync_and_seq_guards_drop_stale_loads() {
        let (mut app, mut ui, _dir) = test_app_and_ui();

        // Simulate active custom JQL results.
        app.issues_custom_query = true;
        app.issues = vec![IssueSummary {
            key: "CUST-9".into(),
            summary: "Custom Query Issue".into(),
            ..Default::default()
        }];
        app.issue_site_ids = vec!["default".into()];

        // 45s background IssuesUpdated arrives with my_open_issues -> must NOT clobber custom JQL!
        let bg_issue = IssueSummary {
            key: "BG-1".into(),
            summary: "Background Issue".into(),
            ..Default::default()
        };
        app.handle_core_event(
            &mut ui,
            crate::models::CoreEvent::IssuesUpdated {
                site_id: "default".into(),
                issues: vec![bg_issue],
            },
        );
        assert_eq!(app.issues.len(), 1);
        assert_eq!(app.issues[0].key, "CUST-9");

        // Stale IssuesLoaded (seq < app.issues_req_seq) must be dropped.
        app.issues_req_seq = 5;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::IssuesLoaded {
                seq: 3,
                site_id: "default".into(),
                issues: vec![IssueSummary {
                    key: "STALE-1".into(),
                    summary: "Old response".into(),
                    ..Default::default()
                }],
                label: "Old".into(),
            },
        );
        assert_eq!(app.issues[0].key, "CUST-9");

        // MultiSiteSyncFinished must NOT wipe active local search hits.
        ui.search_ui.query.set_text("hello");
        ui.search_ui.is_remote = false;
        app.search_hits = vec![crate::models::UnifiedSearchHit {
            kind: SearchHitKind::JiraIssue,
            id_or_key: "ATL-1".into(),
            title: "Hello world".into(),
            container: "ATL".into(),
            status_or_version: "Open".into(),
            snippet: String::new(),
            web_url: String::new(),
            updated_at: String::new(),
        }];
        app.search_hit_site_ids = vec!["default".into()];
        app.search_hit_site_labels = vec![None];
        // Seed matching item in SQLite so re-running local search keeps hits populated.
        let _ = app.storage.upsert_issues_for_site(
            "default",
            &[IssueSummary {
                key: "ATL-1".into(),
                summary: "Hello world".into(),
                ..Default::default()
            }],
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MultiSiteSyncFinished(crate::sync::MultiSiteSyncOutcome {
                total_sites: 1,
                succeeded_sites: 1,
                per_site: Vec::new(),
                summary: "ok".into(),
            }),
        );
        assert!(
            !app.search_hits.is_empty(),
            "MultiSiteSyncFinished must not wipe active Search tab query with empty string"
        );
    }

    #[tokio::test]
    async fn all_sites_my_work_filters_per_site_me_id() {
        let storage = Storage::open_in_memory().unwrap();
        let make_issue = |key: &str, assignee_id: &str, cat: crate::models::StatusCategory| {
            IssueSummary {
                key: key.into(),
                summary: format!("Issue {key}"),
                assignee: Some(crate::models::UserSummary {
                    id: assignee_id.into(),
                    display_name: assignee_id.into(),
                    ..Default::default()
                }),
                status: crate::models::IssueStatus {
                    id: String::new(),
                    name: if cat == crate::models::StatusCategory::Done {
                        "Done".into()
                    } else {
                        "In Progress".into()
                    },
                    category: cat,
                },
                ..Default::default()
            }
        };
        storage
            .upsert_issues_for_site(
                "site-a",
                &[
                    make_issue("A-1", "me-a", crate::models::StatusCategory::InProgress),
                    make_issue("A-2", "other-user", crate::models::StatusCategory::InProgress),
                    make_issue("A-3", "me-a", crate::models::StatusCategory::Done),
                ],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "site-b",
                &[
                    make_issue("B-1", "me-b", crate::models::StatusCategory::Todo),
                    make_issue("B-2", "someone-else", crate::models::StatusCategory::Todo),
                ],
            )
            .unwrap();

        let mut me_ids = std::collections::HashMap::new();
        me_ids.insert("site-a".to_string(), "me-a".to_string());
        me_ids.insert("site-b".to_string(), "me-b".to_string());

        let rows = storage
            .list_my_work_all_sites_for_users(&me_ids, 40)
            .unwrap();
        let keys: Vec<String> = rows.into_iter().map(|(_, i)| i.key).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"A-1".to_string()));
        assert!(keys.contains(&"B-1".to_string()));
        assert!(!keys.contains(&"A-2".to_string()));
        assert!(!keys.contains(&"A-3".to_string()));
        assert!(!keys.contains(&"B-2".to_string()));
    }
}
