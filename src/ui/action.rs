//! Action dispatcher for keyboard, mouse, overlays, and workspace tabs.

use crate::{
    models::{CrossRefTarget, SearchHitKind},
    sync::SyncCommand,
    ui::{
        app::{AppState, PaneFocus, UiState, WorkspaceTab},
        hitmap::HitTarget,
        keymap::Action,
        overlays::{
            comment::{CommentOverlay, CommentTarget},
            cross_link::CrossLinkOverlay,
            image_preview::ImagePreviewOverlay,
            issue_form::IssueFormOverlay,
            page_form::PageFormOverlay,
            path_input::{PathInputOverlay, UploadTarget},
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
        if let Action::MouseClick { row, col } = action {
            if let Some(HitTarget::OverlayItem(idx)) = ui.hitmap.hit(row, col) {
                return handle_overlay_item_click(idx, app, ui);
            }
            return false;
        }

        let outcome = ui.overlays.last_mut().map(|o| o.handle(&action));
        if let Some(out) = outcome {
            match out {
                OverlayOutcome::Handled => return false,
                OverlayOutcome::Close => {
                    ui.pop_overlay();
                    return false;
                }
                OverlayOutcome::Dispatch(sub) => {
                    return handle_overlay_dispatch(sub, app, ui);
                }
                OverlayOutcome::CloseAndDispatch(sub) => {
                    ui.pop_overlay();
                    return dispatch(sub, app, ui);
                }
            }
        }
    }

    // Mouse click dispatch outside overlays.
    if let Action::MouseClick { row, col } = action {
        return handle_mouse_click(row, col, app, ui);
    }

    // Inline text bar input (JQL / Local Filter / Unified Search).
    if ui.accepts_text_input() {
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
        Action::OpenHelp => {
            ui.push_overlay(Overlay::Help);
        }
        Action::SyncAll => {
            app.syncing = true;
            let _ = app.sync_ctrl.request(SyncCommand::RefreshAll);
            if ui.active_tab == WorkspaceTab::MyWork {
                app.request_my_work_refresh(ui);
            }
            ui.info("전체 동기화를 요청했습니다");
        }
        Action::CheckUpdate => {
            app.request_check_update(ui);
            ui.info("GitHub 릴리스 업데이트 확인 중…");
        }
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            ui.info(format!(
                "인라인 이미지 썸네일: {}",
                if ui.inline_images { "ON" } else { "OFF" }
            ));
        }
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
        }
        Action::PopNavHistory => {
            if !app.pop_nav_history(ui) {
                ui.info("이전 탐색 히스토리가 없습니다");
            }
        }
        Action::OpenCrossLinks => {
            let (label, targets) = app.collect_cross_links(ui.active_tab);
            ui.push_overlay(Overlay::CrossLink(CrossLinkOverlay::new(label, targets)));
        }
        Action::FocusNextPane => cycle_focus(ui, true),
        Action::FocusPrevPane => cycle_focus(ui, false),

        // Jira & Confluence Domain Actions
        Action::OpenTransition => open_transition(app, ui),
        Action::AssignToMe => {
            if let Some(key) = app.selected_issue_key(ui) {
                app.request_assign_to_me(ui, &key);
            } else {
                ui.info("선택된 Jira 이슈가 없습니다");
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
                ui.info("선택된 Jira 이슈가 없습니다");
            }
        }
        Action::OpenComment => open_comment(app, ui),
        Action::NewItem => open_new_item(app, ui),
        Action::EditItem => open_edit_item(app, ui),
        Action::UploadAttachment => open_upload_attachment(app, ui),
        Action::DownloadAttachment => download_first_attachment(app, ui),
        Action::OpenImagePreview => open_image_preview(app, ui),
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
        Action::ScrollUp | Action::PageUp => scroll_active_pane(ui, -6),
        Action::ScrollDown | Action::PageDown => scroll_active_pane(ui, 6),
        Action::GoTop => scroll_active_pane(ui, -10000),
        Action::GoBottom => scroll_active_pane(ui, 10000),
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
                        app.request_jql_search(ui, jql, "사용자 JQL".to_string());
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
                        let _ = open::that(&att.content_url);
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
                    app.request_submit_issue_form(ui, &ov);
                }
                Overlay::PageForm(ov) => {
                    app.request_submit_page_form(ui, &ov);
                }
                Overlay::Comment(ov) => {
                    app.request_submit_comment(ui, &ov);
                }
                Overlay::CrossLink(ov) => {
                    if let Some(target) = ov.selected_target().cloned() {
                        app.jump_to_cross_ref(ui, target);
                    }
                }
                Overlay::PathInput(ov) => {
                    app.request_upload_attachment(ui, &ov);
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
        match top {
            Overlay::Transition(ov) => ov.selected = idx,
            Overlay::Assignee(ov) => ov.selected = idx,
            Overlay::CrossLink(ov) => ov.selected = idx,
            Overlay::CommandPalette(ov) => ov.selected = idx,
            _ => return false,
        }
    }
    handle_overlay_dispatch(Action::Submit, app, ui)
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
            if let Some(issue) = app.selected_board_issue(ui) {
                app.request_issue_detail(ui, &issue.key);
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
            let list = app.filtered_issues(ui);
            if let Some(it) = list.get(idx) {
                app.request_issue_detail(ui, &it.key);
            }
        }
        HitTarget::MyWorkIssue(idx) => {
            ui.my_work_ui.active_column = 0;
            ui.my_work_ui.selected_issue = idx;
            if let Some(it) = app.my_issues.get(idx).cloned() {
                app.jump_to_cross_ref(
                    ui,
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
            if let Some(pg) = app.my_pages.get(idx).cloned() {
                app.jump_to_cross_ref(
                    ui,
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
            if let Some(sp) = app.spaces.get(idx).cloned() {
                app.request_space_root_pages(ui, &sp.key);
            }
        }
        HitTarget::TreeRow(idx) => {
            ui.focus = PaneFocus::Nav;
            ui.confluence_ui.nav_section = 1;
            ui.confluence_ui.selected_tree_row = idx;
            let tree = app.current_confluence_tree(ui);
            if let Some(row) = tree.get(idx) {
                let page = row.page.clone();
                app.request_page_detail(ui, &page.id);
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
        HitTarget::Button(_) | HitTarget::Attachment(_) | HitTarget::OverlayItem(_) => {}
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

fn scroll_active_pane(ui: &mut UiState, delta: i32) {
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
        _ => {}
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
                if let Some(issue) = app.selected_board_issue(ui) {
                    app.request_issue_detail(ui, &issue.key);
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
                let list = app.filtered_issues(ui);
                if let Some(it) = list.get(ui.issue_ui.selected_issue) {
                    app.request_issue_detail(ui, &it.key);
                    ui.focus = PaneFocus::Main;
                }
            }
        }
        WorkspaceTab::MyWork => {
            if ui.my_work_ui.active_column == 0 {
                if let Some(it) = app.my_issues.get(ui.my_work_ui.selected_issue).cloned() {
                    app.jump_to_cross_ref(
                        ui,
                        CrossRefTarget::JiraIssue {
                            key: it.key,
                            label: Some(it.summary),
                        },
                    );
                }
            } else if let Some(pg) = app.my_pages.get(ui.my_work_ui.selected_page).cloned() {
                app.jump_to_cross_ref(
                    ui,
                    CrossRefTarget::ConfluencePage {
                        page_id: pg.id,
                        title: Some(pg.title),
                    },
                );
            }
        }
        WorkspaceTab::Confluence => match ui.focus {
            PaneFocus::Nav => {
                if ui.confluence_ui.nav_section == 0 {
                    if let Some(sp) = app.spaces.get(ui.confluence_ui.selected_space).cloned() {
                        ui.confluence_ui.selected_tree_row = 0;
                        app.request_space_root_pages(ui, &sp.key);
                    }
                } else {
                    let tree = app.current_confluence_tree(ui);
                    if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                        let page = row.page.clone();
                        app.request_page_detail(ui, &page.id);
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
                app.jump_to_cross_ref(ui, target);
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
        ui.info("상태를 전환할 Jira 이슈를 먼저 선택하세요");
    }
}

fn open_comment(app: &AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        if let Some(page) = &app.active_page {
            ui.push_overlay(Overlay::Comment(CommentOverlay::new(CommentTarget::Page {
                page_id: page.summary.id.clone(),
                title: page.summary.title.clone(),
            })));
            return;
        }
    }
    if let Some(key) = app.selected_issue_key(ui) {
        ui.push_overlay(Overlay::Comment(CommentOverlay::new(CommentTarget::Issue(
            key,
        ))));
    } else {
        ui.info("댓글을 작성할 이슈 또는 페이지를 먼저 선택하세요");
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
            ui.info("Confluence 스페이스를 먼저 선택하세요");
            return;
        }
        let parent_id = app.active_page.as_ref().map(|p| p.summary.id.clone());
        ui.push_overlay(Overlay::PageForm(PageFormOverlay::new_page(
            space_key,
            parent_id,
        )));
    } else {
        let default_proj = app
            .active_board(ui)
            .and_then(|b| b.project_key.clone())
            .or_else(|| app.config.default_project.clone())
            .or_else(|| app.projects.first().map(|p| p.key.clone()))
            .unwrap_or_default();
        ui.push_overlay(Overlay::IssueForm(IssueFormOverlay::new_issue(
            &default_proj,
        )));
    }
}

fn open_edit_item(app: &AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        if let Some(page) = &app.active_page {
            ui.push_overlay(Overlay::PageForm(PageFormOverlay::edit_page(page)));
        } else {
            ui.info("편집할 Confluence 페이지를 먼저 선택하세요");
        }
    } else if let Some(issue) = &app.active_issue {
        ui.push_overlay(Overlay::IssueForm(IssueFormOverlay::edit_issue(issue)));
    } else {
        ui.info("편집할 Jira 이슈를 먼저 선택하세요");
    }
}

fn open_upload_attachment(app: &AppState, ui: &mut UiState) {
    if ui.active_tab == WorkspaceTab::Confluence {
        if let Some(page) = &app.active_page {
            ui.push_overlay(Overlay::PathInput(PathInputOverlay::new(
                UploadTarget::Page(page.summary.id.clone()),
            )));
            return;
        }
    }
    if let Some(key) = app.selected_issue_key(ui) {
        ui.push_overlay(Overlay::PathInput(PathInputOverlay::new(
            UploadTarget::Issue(key),
        )));
    } else {
        ui.info("파일을 첨부할 이슈 또는 페이지를 먼저 선택하세요");
    }
}

fn download_first_attachment(app: &AppState, ui: &mut UiState) {
    let (_, atts) = app.current_attachments(ui);
    if let Some(first) = atts.first().cloned() {
        app.request_download_attachment(ui, first);
    } else {
        ui.info("다운로드할 첨부파일이 없습니다");
    }
}

fn open_image_preview(app: &AppState, ui: &mut UiState) {
    let (label, atts) = app.current_attachments(ui);
    if let Some(ov) = ImagePreviewOverlay::from_attachments(label, &atts, None) {
        ui.push_overlay(Overlay::ImagePreview(ov));
        app.request_load_preview_image(ui);
    } else {
        ui.info("미리볼 이미지 첨부파일이 없습니다");
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
            ui.error(format!("브라우저 열기 실패: {err}"));
        }
    } else {
        ui.info("열 수 있는 웹 URL이 없습니다");
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
                    if p.pinned { "고정됨" } else { "고정 해제" }
                ));
            }
        } else if let Some(s) = app.spaces.get_mut(ui.directory_ui.selected_space) {
            s.pinned = !s.pinned;
            let _ = app.storage.set_space_pinned(&s.key, s.pinned);
            ui.info(format!(
                "{} {}",
                s.key,
                if s.pinned { "고정됨" } else { "고정 해제" }
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
