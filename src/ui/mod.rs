//! Unified Jira + Confluence UI module (`ratatui`): minimal-chrome persona header,
//! 3-tier responsive hairline layout, density support, and Braille spinner status bar.

pub mod action;
pub mod app;
pub mod board_view;
pub mod confluence_view;
pub mod directory_view;
pub mod doc_view;
pub mod event;
pub mod hitmap;
pub mod issue_view;
pub mod keymap;
pub mod my_work_view;
pub mod notification;
pub mod overlays;
pub mod search_view;
pub mod text_edit;
pub mod theme;
pub mod toast;

use std::{sync::OnceLock, time::Instant};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use self::{
    app::{AppState, PaneFocus, UiState, WorkspaceTab},
    doc_view::truncate_width,
    hitmap::{ButtonId, HitTarget},
    my_work_view::MyWorkScope,
    search_view::SearchScope,
    toast::ToastKind,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const WORKSPACE_TAB_LABELS: [&str; 6] = [
    "1:Board",
    "2:Issues",
    "3:My Work",
    "4:Wiki",
    "5:Search",
    "6:Directory",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
}

pub fn layout_mode(width: u16) -> LayoutMode {
    if width >= 120 {
        LayoutMode::Wide
    } else if width >= 80 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    }
}

pub(crate) fn spinner_glyph() -> &'static str {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed_ms = START.get_or_init(Instant::now).elapsed().as_millis();
    SPINNER_FRAMES[((elapsed_ms / 120) as usize) % SPINNER_FRAMES.len()]
}

pub(crate) fn site_display_english(app: &AppState) -> String {
    let raw = app.config.site_url.trim();
    if raw.is_empty() {
        "Not configured".to_string()
    } else {
        raw.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string()
    }
}

pub(crate) fn draw_vertical_hairline(
    frame: &mut Frame,
    x: u16,
    y: u16,
    height: u16,
    style: Style,
) {
    let buf_area = frame.area();
    if height == 0 || x < buf_area.left() || x >= buf_area.right() {
        return;
    }
    let start_y = y.max(buf_area.top());
    let end_y = y.saturating_add(height).min(buf_area.bottom());
    let buf = frame.buffer_mut();
    for row_y in start_y..end_y {
        buf[(x, row_y)].set_symbol("│").set_style(style);
    }
}

pub(crate) fn draw_horizontal_hairline(
    frame: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
) {
    if width == 0 {
        return;
    }
    let buf_area = frame.area();
    if y < buf_area.top() || y >= buf_area.bottom() {
        return;
    }
    let start_x = x.max(buf_area.left());
    let end_x = x.saturating_add(width).min(buf_area.right());
    let buf = frame.buffer_mut();
    for col_x in start_x..end_x {
        buf[(col_x, y)].set_symbol("─").set_style(style);
    }
}

pub(crate) fn fill_surface(frame: &mut Frame, area: Rect, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(style), area);
}

pub fn draw(frame: &mut Frame, app: &AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.hitmap.clear();
    ui.site_group_keys.clear();

    if area.width < 12 || area.height < 5 {
        return None;
    }

    if !ui.theme.no_color {
        let base_style = Style::default()
            .bg(ui.theme.bg_canvas)
            .fg(ui.theme.fg_primary);
        fill_surface(frame, area, base_style);
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_persona_header(frame, rows[0], app, ui);

    let mode = layout_mode(area.width);
    let body = rows[1];
    let tab_supports_nav = matches!(
        ui.active_tab,
        WorkspaceTab::Board | WorkspaceTab::Issues | WorkspaceTab::Confluence
    );
    let show_nav = tab_supports_nav && (ui.show_nav_rail || mode == LayoutMode::Narrow);
    let has_drawer = match ui.active_tab {
        WorkspaceTab::Board => ui.board_ui.drawer_open && mode == LayoutMode::Wide,
        WorkspaceTab::Confluence => ui.confluence_ui.toc_open && mode == LayoutMode::Wide,
        _ => false,
    };

    let (nav_rect, main_rect, drawer_rect) = if !show_nav {
        if has_drawer && mode == LayoutMode::Wide {
            let drawer_w = (body.width * 34 / 100).clamp(34, 54);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(30),
                    Constraint::Length(1),
                    Constraint::Length(drawer_w),
                ])
                .split(body);
            (None, cols[0], Some(cols[2]))
        } else {
            (None, body, None)
        }
    } else {
        match mode {
            LayoutMode::Wide => {
                let nav_w = 28u16;
                let nav_sep = 1u16;
                let drawer_w = if has_drawer {
                    (body.width * 34 / 100).clamp(34, 54)
                } else {
                    0
                };
                let drawer_sep = if drawer_w > 0 { 1u16 } else { 0 };
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(nav_w),
                        Constraint::Length(nav_sep),
                        Constraint::Min(30),
                        Constraint::Length(drawer_sep),
                        Constraint::Length(drawer_w),
                    ])
                    .split(body);
                (
                    Some(cols[0]),
                    cols[2],
                    (drawer_w > 0).then_some(cols[4]),
                )
            }
            LayoutMode::Medium => {
                let nav_w = 24u16;
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(nav_w),
                        Constraint::Length(1),
                        Constraint::Min(28),
                    ])
                    .split(body);
                (Some(cols[0]), cols[2], None)
            }
            LayoutMode::Narrow => match ui.focus {
                PaneFocus::Nav => (Some(body), Rect::default(), None),
                PaneFocus::Detail if has_drawer => (None, Rect::default(), Some(body)),
                _ => (None, body, None),
            },
        }
    };

    match ui.active_tab {
        WorkspaceTab::Board => {
            board_view::draw(frame, nav_rect, main_rect, drawer_rect, app, ui);
        }
        WorkspaceTab::Issues => {
            issue_view::draw(frame, nav_rect, main_rect, app, ui);
        }
        WorkspaceTab::MyWork => {
            let active_label = app.active_scope_label();
            let scope = MyWorkScope {
                active_site_label: active_label.as_deref(),
                issue_site_labels: &app.my_issue_site_labels,
                page_site_labels: &app.my_page_site_labels,
            };
            my_work_view::draw_with_scope(frame, main_rect, app, ui, &scope);
        }
        WorkspaceTab::Confluence => {
            confluence_view::draw(frame, nav_rect, main_rect, drawer_rect, app, ui);
        }
        WorkspaceTab::Search => {
            let active_label = app.active_scope_label();
            let scope = SearchScope {
                active_site_label: active_label.as_deref(),
                hit_site_labels: &app.search_hit_site_labels,
            };
            search_view::draw_with_scope(frame, main_rect, app, ui, &scope);
        }
        WorkspaceTab::Directory => {
            let connected_sites = app.build_connected_site_rows();
            let active_label = app.active_scope_label();
            directory_view::draw_with_sites(
                frame,
                main_rect,
                app,
                ui,
                &connected_sites,
                active_label.as_deref(),
            );
        }
    }

    // 1px vertical hairline dividers between adjacent panes
    if let Some(nav_r) = nav_rect {
        if main_rect.width > 0 {
            let sep_x = nav_r.right();
            if sep_x < main_rect.x && sep_x < area.right() {
                let sep_focused = matches!(ui.focus, PaneFocus::Nav | PaneFocus::Main);
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_vertical_hairline(frame, sep_x, nav_r.y, nav_r.height, sep_style);
            }
        }
    }

    if let Some(drawer_r) = drawer_rect {
        if main_rect.width > 0 {
            let sep_x = main_rect.right();
            if sep_x < drawer_r.x && sep_x < area.right() {
                let sep_focused = matches!(ui.focus, PaneFocus::Main | PaneFocus::Detail);
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_vertical_hairline(frame, sep_x, drawer_r.y, drawer_r.height, sep_style);
            }
        }
    }

    draw_status_bar(frame, rows[2], app, ui);

    let mut hitmap = std::mem::take(&mut ui.hitmap);
    let mut overlays = std::mem::take(&mut ui.overlays);
    for overlay in &mut overlays {
        hitmap.clear();
        overlay.draw(frame, area, app, ui, &mut hitmap);
    }
    ui.overlays = overlays;
    ui.hitmap = hitmap;

    None
}

fn draw_persona_header(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let bar_style = theme.surface_style(theme.bg_header);
    let mut spans = Vec::new();
    let mut x = area.x;

    let badge = theme.persona_badge(app.persona);
    x += badge.content.width() as u16;
    spans.push(badge);

    // Clickable active site pill (opens SiteSwitcherOverlay on click or Ctrl+S).
    spans.push(Span::styled(" ", bar_style));
    x += 1;
    let site_pill_label = truncate_width(&app.header_site_pill_label(), 22);
    let site_pill_text = format!(" {site_pill_label} ▾ ");
    let site_pill_w = site_pill_text.width() as u16;
    let site_pill_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.chip_bg)
            .fg(theme.chip_fg)
            .add_modifier(Modifier::BOLD)
    };
    if x + site_pill_w <= area.x + area.width {
        ui.hitmap
            .add_line(x, area.y, site_pill_w, HitTarget::Button(ButtonId::SiteSwitcher));
    }
    x += site_pill_w;
    spans.push(Span::styled(site_pill_text, site_pill_style));
    spans.push(Span::styled(" ", bar_style));
    x += 1;

    for (idx, label) in WORKSPACE_TAB_LABELS.iter().enumerate() {
        let active = ui.active_tab.index() == idx;
        let text = format!(" {label} ");
        let w = text.width() as u16;
        let style = if active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                let (active_bg, active_fg) = if idx == WorkspaceTab::Confluence.index() {
                    (theme.confluence_teal, theme.confluence_pill_fg)
                } else {
                    (theme.jira_cobalt, theme.jira_pill_fg)
                };
                Style::default()
                    .bg(active_bg)
                    .fg(active_fg)
                    .add_modifier(Modifier::BOLD)
            }
        } else if theme.no_color {
            Style::default()
        } else {
            Style::default().bg(theme.bg_header).fg(theme.fg_secondary)
        };
        if x + w <= area.x + area.width {
            ui.hitmap.add_line(x, area.y, w, HitTarget::Tab(idx));
        }
        x += w;
        spans.push(Span::styled(text, style));
    }

    // Right side: user & sync status with Braille spinner when active
    let user_str = app
        .me
        .as_ref()
        .map(|u| format!("@{}", u.display_name))
        .unwrap_or_else(|| {
            if app.auth.is_logged_in() {
                "Authenticated".to_string()
            } else {
                "Offline".to_string()
            }
        });
    let is_busy = app.syncing || ui.tasks.first_running().is_some();
    let sync_status = if is_busy {
        format!("{} Syncing", spinner_glyph())
    } else if app.auth.is_logged_in() {
        "● Connected".to_string()
    } else {
        "○ Offline".to_string()
    };
    let right_text = format!(" {user_str} · {sync_status} ");
    let right_w = right_text.width() as u16;
    let remaining = (area.x + area.width).saturating_sub(x);
    if remaining > right_w {
        let pad_w = remaining - right_w;
        spans.push(Span::styled(" ".repeat(pad_w as usize), bar_style));
        let status_x = x.saturating_add(pad_w);
        ui.hitmap.add_line(
            status_x,
            area.y,
            right_w,
            HitTarget::Button(ButtonId::SyncAll),
        );
        let status_fg = if theme.no_color {
            Color::Reset
        } else if is_busy {
            theme.accent_soft
        } else if app.auth.is_logged_in() {
            theme.success
        } else {
            theme.fg_muted
        };
        spans.push(Span::styled(
            right_text,
            if theme.no_color {
                Style::default()
            } else {
                Style::default().bg(theme.bg_header).fg(status_fg)
            },
        ));
    } else if remaining > 0 {
        spans.push(Span::styled(" ".repeat(remaining as usize), bar_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), area);
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let bar_style = theme.surface_style(theme.bg_header);
    let bg = if theme.no_color {
        Color::Reset
    } else {
        theme.bg_header
    };

    let focus_badge = match ui.focus {
        PaneFocus::Nav => "[NAV]",
        PaneFocus::Main => "[MAIN]",
        PaneFocus::Detail => "[DETAIL]",
        PaneFocus::JqlBar => "[JQL]",
        PaneFocus::SearchBar => "[SEARCH]",
    };

    let right_meta = format!(" {} ", app.config.density.label());
    let right_w = right_meta.width();
    let total_w = area.width as usize;
    let show_right = total_w > right_w + 16;
    let left_budget = if show_right { total_w - right_w } else { total_w };

    let mut left_spans: Vec<Span<'static>> = Vec::new();
    let mut left_used_w = 0usize;

    if let Some(toast) = ui.toasts.current() {
        let fg = if theme.no_color {
            Color::Reset
        } else {
            match toast.kind {
                ToastKind::Info => theme.accent_soft,
                ToastKind::Success => theme.success,
                ToastKind::Error => theme.error,
            }
        };
        let style = Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD);
        let fitted = truncate_width(&format!(" {} ", toast.message), left_budget);
        left_used_w = fitted.width();
        left_spans.push(Span::styled(fitted, style));
    } else if let Some(task) = ui.tasks.first_running() {
        let style = Style::default().bg(bg).fg(if theme.no_color {
            Color::Reset
        } else {
            theme.accent_soft
        });
        let fitted = truncate_width(
            &format!(" {} {} ", spinner_glyph(), task.display()),
            left_budget,
        );
        left_used_w = fitted.width();
        left_spans.push(Span::styled(fitted, style));
    } else if app.syncing {
        let style = Style::default().bg(bg).fg(if theme.no_color {
            Color::Reset
        } else {
            theme.accent_soft
        });
        let fitted = truncate_width(
            &format!(" {} Syncing workspace… ", spinner_glyph()),
            left_budget,
        );
        left_used_w = fitted.width();
        left_spans.push(Span::styled(fitted, style));
    } else {
        let muted_style = if theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().bg(bg).fg(theme.fg_muted)
        };
        let mut segments: Vec<(String, Option<ButtonId>)> = vec![(format!(" {focus_badge} "), None)];
        if !app.nav_history.is_empty() {
            segments.push((
                format!("[Backspace] Back ({})", app.nav_history.len()),
                Some(ButtonId::BackHistory),
            ));
            segments.push((" · ".to_string(), None));
        }
        if !ui.show_nav_rail {
            segments.push(("Nav Hidden · ".to_string(), None));
        }
        segments.push(("[Tab] Pane · [1-6] Tabs · ".to_string(), None));
        segments.push(("[Ctrl+P] Palette".to_string(), Some(ButtonId::CommandPalette)));
        segments.push((" · ".to_string(), None));
        segments.push(("[?] Help".to_string(), Some(ButtonId::Help)));
        segments.push((" · [q] Quit ".to_string(), None));

        for (seg, btn) in segments {
            let rem = left_budget.saturating_sub(left_used_w);
            if rem == 0 {
                break;
            }
            let seg_w = seg.width();
            if seg_w <= rem {
                if let Some(btn_id) = btn {
                    ui.hitmap.add_line(
                        area.x.saturating_add(left_used_w as u16),
                        area.y,
                        seg_w as u16,
                        HitTarget::Button(btn_id),
                    );
                }
                left_used_w += seg_w;
                left_spans.push(Span::styled(seg, muted_style));
            } else {
                let fitted = truncate_width(&seg, rem);
                let fw = fitted.width();
                if let Some(btn_id) = btn {
                    if fw > 0 {
                        ui.hitmap.add_line(
                            area.x.saturating_add(left_used_w as u16),
                            area.y,
                            fw as u16,
                            HitTarget::Button(btn_id),
                        );
                    }
                }
                left_used_w += fw;
                left_spans.push(Span::styled(fitted, muted_style));
                break;
            }
        }
    }

    let pad = left_budget.saturating_sub(left_used_w);
    if pad > 0 {
        left_spans.push(Span::styled(" ".repeat(pad), bar_style));
    }
    if show_right {
        let right_style = if theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().bg(bg).fg(theme.fg_secondary)
        };
        left_spans.push(Span::styled(right_meta, right_style));
    }

    frame.render_widget(Paragraph::new(Line::from(left_spans)).style(bar_style), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, DensityMode, Persona, ThemePreset},
        models::{
            IssueDetail, IssuePriority, IssueStatus, IssueSummary, IssueType, PageDetail,
            PageSummary, SpaceSummary, StatusCategory,
        },
        storage::Storage,
        sync,
        ui::{action, event, keymap::Action, theme::Theme},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn test_app(persona: Persona) -> (AppState, UiState) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
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
            persona,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, persona);

        app.board_issues = vec![
            IssueSummary {
                key: "ATL-1".into(),
                project_key: "ATL".into(),
                summary: "Implement Kanban board".into(),
                status: IssueStatus {
                    id: "1".into(),
                    name: "To Do".into(),
                    category: StatusCategory::Todo,
                },
                priority: Some(IssuePriority {
                    id: "2".into(),
                    name: "High".into(),
                }),
                issue_type: IssueType {
                    id: "10".into(),
                    name: "Story".into(),
                    subtask: false,
                    epic: false,
                },
                ..Default::default()
            },
            IssueSummary {
                key: "ATL-2".into(),
                project_key: "ATL".into(),
                summary: "Render Confluence page tree".into(),
                status: IssueStatus {
                    id: "2".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
                ..Default::default()
            },
        ];
        app.issues = app.board_issues.clone();
        app.active_issue = Some(IssueDetail {
            summary: app.board_issues[0].clone(),
            description_markdown: "Review architecture documentation".into(),
            ..Default::default()
        });

        app.spaces = vec![SpaceSummary {
            id: "sp1".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            space_type: "global".into(),
            ..Default::default()
        }];
        let root_page = PageSummary {
            id: "p1".into(),
            space_key: "ENG".into(),
            title: "System Architecture".into(),
            version_number: 3,
            has_children: Some(true),
            ..Default::default()
        };
        app.root_pages_by_space
            .insert("ENG".into(), vec![root_page.clone()]);
        app.active_page = Some(PageDetail {
            summary: root_page,
            body_markdown: "Linked with ATL-1 issue".into(),
            ..Default::default()
        });

        (app, ui)
    }

    fn render_buffer_text(app: &AppState, ui: &mut UiState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, app, ui);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            let mut x = 0;
            while x < width {
                let sym = buf[(x, y)].symbol();
                out.push_str(sym);
                let w = sym.width().max(1) as u16;
                x += w;
            }
            out.push('\n');
        }
        out
    }

    #[tokio::test]
    async fn jira_persona_renders_kanban_board_and_navigates_columns_and_cards() {
        let (mut app, mut ui) = test_app(Persona::Jira);
        assert_eq!(ui.active_tab, WorkspaceTab::Board);

        let screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(screen.contains(" JIRA "), "expected clean JIRA pill:\n{screen}");
        assert!(screen.contains("acme.atlassian.net"), "expected site:\n{screen}");
        assert!(screen.contains("ATL-1"), "expected card ATL-1:\n{screen}");
        assert!(screen.contains("ATL-2"), "expected card ATL-2:\n{screen}");
        assert!(screen.contains("│"), "expected hairline divider:\n{screen}");

        // Navigate right column with `l` / `MoveRight`
        assert_eq!(ui.board_ui.selected_col, 0);
        action::dispatch(Action::MoveRight, &mut app, &mut ui);
        assert_eq!(ui.board_ui.selected_col, 1);
        assert_eq!(app.selected_board_issue(&ui).unwrap().key, "ATL-2");

        // Transitioning ATL-2 immediately moves the card to the target column
        app.request_transition_issue(
            &mut ui,
            "ATL-2",
            crate::models::IssueTransition {
                id: "31".into(),
                name: "Done".into(),
                to_status: IssueStatus {
                    id: "3".into(),
                    name: "Done".into(),
                    category: StatusCategory::Done,
                },
            },
        );
        let cols = app.build_kanban_columns(&ui);
        assert!(cols[2].issues.iter().any(|i| i.key == "ATL-2"));
    }

    #[tokio::test]
    async fn confluence_persona_renders_page_tree_and_wiki_reader() {
        let (mut app, mut ui) = test_app(Persona::Confluence);
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);

        let screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(
            screen.contains(" CONFLUENCE "),
            "expected clean CONFLUENCE pill:\n{screen}"
        );
        assert!(
            screen.contains("System Architecture"),
            "expected page title in tree/reader:\n{screen}"
        );

        // Toggle TOC drawer (`t`) and verify other tabs render cleanly too
        action::dispatch(Action::ToggleDrawerOrToc, &mut app, &mut ui);
        assert!(ui.confluence_ui.toc_open);
        let _ = render_buffer_text(&app, &mut ui, 130, 36);

        for tab in 0..=5 {
            action::dispatch(Action::SwitchTab(tab), &mut app, &mut ui);
            let _ = render_buffer_text(&app, &mut ui, 130, 36);
            let _ = render_buffer_text(&app, &mut ui, 70, 24);
        }
    }

    #[tokio::test]
    async fn nav_rail_density_themes_and_spinner_render_cleanly() {
        let (mut app, mut ui) = test_app(Persona::Jira);

        // Toggle nav rail off
        ui.show_nav_rail = false;
        let without_nav = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(!without_nav.contains("Boards & Sprints"));
        assert!(without_nav.contains("Nav Hidden"));

        // Density compact + syncing spinner
        app.config.density = DensityMode::Compact;
        app.syncing = true;
        let compact_sync = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(compact_sync.contains("Compact"));
        assert!(compact_sync.contains("Syncing"));
        assert!(
            SPINNER_FRAMES.iter().any(|f| compact_sync.contains(f)),
            "expected braille spinner in:\n{compact_sync}"
        );

        // Verify all 5 theme presets render without panicking
        app.syncing = false;
        ui.show_nav_rail = true;
        for preset in [
            ThemePreset::Dark,
            ThemePreset::Light,
            ThemePreset::Ansi256,
            ThemePreset::Ansi16,
            ThemePreset::NoColor,
        ] {
            ui.theme = Theme::detect(preset);
            let out = render_buffer_text(&app, &mut ui, 130, 36);
            assert!(out.contains("ATL-1"));
        }
    }

    #[tokio::test]
    async fn keymap_and_dispatch_actions_work_end_to_end() {
        let (mut app, mut ui) = test_app(Persona::Jira);

        // `J` focuses JQL bar in Issues tab
        let act_j = event::key_to_action(
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
        .unwrap();
        assert_eq!(act_j, Action::FocusJql);
        action::dispatch(act_j, &mut app, &mut ui);
        assert_eq!(ui.active_tab, WorkspaceTab::Issues);
        assert_eq!(ui.focus, PaneFocus::JqlBar);

        // Esc exits JQL bar
        action::dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert_eq!(ui.focus, PaneFocus::Main);

        // `m` opens TransitionOverlay
        let act_m = event::key_to_action(
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
        .unwrap();
        action::dispatch(act_m, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(overlays::Overlay::Transition(_))));
        action::dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // `g` opens CrossLinkOverlay and finds ATL-1 / linked refs
        let act_g = event::key_to_action(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
        .unwrap();
        action::dispatch(act_g, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(overlays::Overlay::CrossLink(_))));
        action::dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // `i` maps to AssignToMe
        let act_i = event::key_to_action(
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
        .unwrap();
        assert_eq!(act_i, Action::AssignToMe);

        // `v` maps to OpenImagePreview
        let act_v = event::key_to_action(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            ui.active_tab,
            ui.focus,
            ui.has_overlay(),
            ui.accepts_text_input(),
            &ui.keymap,
        )
        .unwrap();
        assert_eq!(act_v, Action::OpenImagePreview);
    }

    #[tokio::test]
    async fn multi_site_header_pill_switcher_federated_views_and_collapsible_groups() {
        use crate::{
            config::{DeploymentKind, SiteProfile},
            models::{BoardSummary, SiteScope},
            ui::overlays::OverlayKind,
        };

        let mut cfg = AppConfig::default();
        cfg.upsert_site(SiteProfile {
            id: "acme-cloud".into(),
            name: "Acme Cloud".into(),
            site_url: "https://acme.atlassian.net".into(),
            confluence_url: String::new(),
            deployment: DeploymentKind::Cloud,
            cloud_id: Some("cid-acme".into()),
            account_id: "cloud-oauth".into(),
            enabled: true,
            scopes: vec![],
            confluence_account_id: None,
        });
        cfg.upsert_site(SiteProfile {
            id: "corp-dc".into(),
            name: "Corp DC".into(),
            site_url: "https://jira.corp.internal".into(),
            confluence_url: String::new(),
            deployment: DeploymentKind::DataCenter,
            cloud_id: None,
            account_id: "corp-dc".into(),
            enabled: true,
            scopes: vec![],
            confluence_account_id: None,
        });

        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_boards_for_site(
                "acme-cloud",
                &[BoardSummary {
                    id: 1,
                    name: "Acme Board".into(),
                    board_type: "scrum".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        storage
            .upsert_boards_for_site(
                "corp-dc",
                &[BoardSummary {
                    id: 2,
                    name: "Corp Board".into(),
                    board_type: "kanban".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "acme-cloud",
                &[IssueSummary {
                    id: "1".into(),
                    key: "ENG-1".into(),
                    project_key: "ENG".into(),
                    summary: "Acme federated ticket".into(),
                    status: IssueStatus {
                        id: "1".into(),
                        name: "In Progress".into(),
                        category: StatusCategory::InProgress,
                    },
                    issue_type: IssueType {
                        id: "10".into(),
                        name: "Task".into(),
                        subtask: false,
                        epic: false,
                    },
                    updated_at: "2026-11-19T12:00:00Z".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        storage
            .upsert_issues_for_site(
                "corp-dc",
                &[IssueSummary {
                    id: "2".into(),
                    key: "ENG-1".into(),
                    project_key: "ENG".into(),
                    summary: "Corp federated ticket".into(),
                    status: IssueStatus {
                        id: "1".into(),
                        name: "To Do".into(),
                        category: StatusCategory::Todo,
                    },
                    issue_type: IssueType {
                        id: "10".into(),
                        name: "Task".into(),
                        subtask: false,
                        epic: false,
                    },
                    updated_at: "2026-11-19T13:00:00Z".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        storage
            .upsert_spaces_for_site(
                "acme-cloud",
                &[SpaceSummary {
                    id: "sp1".into(),
                    key: "DOCS".into(),
                    name: "Acme Docs".into(),
                    space_type: "global".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        storage
            .upsert_spaces_for_site(
                "corp-dc",
                &[SpaceSummary {
                    id: "sp2".into(),
                    key: "DOCS".into(),
                    name: "Corp Docs".into(),
                    space_type: "global".into(),
                    ..Default::default()
                }],
            )
            .unwrap();

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
        app.load_cached_initial();
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

        // 1. Top header shows clickable All Sites pill and Board nav shows collapsible site groups
        let board_screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(board_screen.contains("All Sites (2)"), "expected All Sites pill:\n{board_screen}");
        assert!(board_screen.contains("▾ Acme Cloud"), "expected Acme site group:\n{board_screen}");
        assert!(board_screen.contains("▾ Corp DC"), "expected Corp site group:\n{board_screen}");

        // Collapse first site group header via collapsed_site_groups
        assert!(!ui.site_group_keys.is_empty());
        let first_key = ui.site_group_keys[0].clone();
        ui.collapsed_site_groups.insert(first_key);
        let collapsed_screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(collapsed_screen.contains("▸ Acme Cloud"), "expected collapsed chevron:\n{collapsed_screen}");

        // 2. My Work shows federated [site] pills & All Sites scope
        action::dispatch(Action::SwitchTab(WorkspaceTab::MyWork.index()), &mut app, &mut ui);
        let my_work_screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(
            my_work_screen.contains("[Acme Cloud]") || my_work_screen.contains("[Corp DC]"),
            "expected site pills:\n{my_work_screen}"
        );

        // 3. Directory shows Connected Sites & Accounts
        action::dispatch(Action::SwitchTab(WorkspaceTab::Directory.index()), &mut app, &mut ui);
        let dir_screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(dir_screen.contains("Connected Sites & Accounts"), "expected Connected Sites table:\n{dir_screen}");
        assert!(dir_screen.contains("Acme Cloud"), "expected Acme Cloud row:\n{dir_screen}");
        assert!(dir_screen.contains("Corp DC"), "expected Corp DC row:\n{dir_screen}");

        // 4. Open Site Switcher via Ctrl+S, select Acme Cloud, verify SiteScope & header pill update
        action::dispatch(Action::OpenSiteSwitcher, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(|o| o.kind()), Some(OverlayKind::SiteSwitcher));
        action::dispatch(Action::MoveDown, &mut app, &mut ui);
        action::dispatch(Action::Activate, &mut app, &mut ui);
        assert!(!ui.has_overlay());
        assert_eq!(app.site_scope, SiteScope::Only("acme-cloud".into()));
        let scoped_screen = render_buffer_text(&app, &mut ui, 130, 36);
        assert!(scoped_screen.contains(" Acme Cloud ▾ "), "expected active site pill:\n{scoped_screen}");
    }

    #[tokio::test]
    async fn newly_wired_buttons_register_hit_targets_and_dispatch_on_mouse_click() {
        use crate::models::{AttachmentItem, CrossRefTarget};
        use crate::ui::overlays::{image_preview::ImagePreviewOverlay, Overlay, OverlayKind};

        let (mut app, mut ui) = test_app(Persona::Jira);
        app.nav_history.push(CrossRefTarget::ConfluencePage {
            page_id: "p1".into(),
            title: Some("System Architecture".into()),
        });

        // 1. Board view: status bar has BackHistory, CommandPalette, Help; board header has ToggleDrawer
        let _ = render_buffer_text(&app, &mut ui, 140, 36);

        let palette_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::CommandPalette))
            .expect("CommandPalette button must be registered in status bar");
        let help_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::Help))
            .expect("Help button must be registered in status bar");
        let back_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::BackHistory))
            .expect("BackHistory button must be registered when nav_history is non-empty");
        let drawer_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleDrawer))
            .expect("ToggleDrawer button must be registered in Board header");

        // Click CommandPalette via MouseClick
        action::dispatch(
            Action::MouseClick {
                row: palette_rect.y,
                col: palette_rect.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(
            ui.overlays.last().map(|o| o.kind()),
            Some(OverlayKind::CommandPalette)
        );
        ui.pop_overlay();

        // Click Help via MouseClick
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        action::dispatch(
            Action::MouseClick {
                row: help_rect.y,
                col: help_rect.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.overlays.last().map(|o| o.kind()), Some(OverlayKind::Help));
        ui.pop_overlay();

        // Click ToggleDrawer on Board header via MouseClick
        let drawer_before = ui.board_ui.drawer_open;
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        action::dispatch(
            Action::MouseClick {
                row: drawer_rect.y,
                col: drawer_rect.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_ne!(ui.board_ui.drawer_open, drawer_before);

        // Click BackHistory in status bar via MouseClick -> pops nav history to Confluence
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        action::dispatch(
            Action::MouseClick {
                row: back_rect.y,
                col: back_rect.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert!(app.nav_history.is_empty());
        assert_eq!(ui.active_tab, WorkspaceTab::Confluence);

        // 2. Issues tab: FocusJql button in JQL bar focuses JqlBar on MouseClick
        action::dispatch(Action::SwitchTab(WorkspaceTab::Issues.index()), &mut app, &mut ui);
        ui.focus = PaneFocus::Main;
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        let jql_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::FocusJql))
            .expect("FocusJql button must be registered in JQL bar");
        action::dispatch(
            Action::MouseClick {
                row: jql_btn.y,
                col: jql_btn.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus, PaneFocus::JqlBar);
        ui.focus = PaneFocus::Main;

        // 3. Directory tab: ToggleInlineImages & ToggleDrawer pills work on MouseClick
        action::dispatch(Action::SwitchTab(WorkspaceTab::Directory.index()), &mut app, &mut ui);
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        let img_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleInlineImages))
            .expect("ToggleInlineImages pill must be registered in Directory view");
        let dir_drawer_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleDrawer))
            .expect("ToggleDrawer pill must be registered in Directory view");
        let inline_before = ui.inline_images;
        action::dispatch(
            Action::MouseClick {
                row: img_btn.y,
                col: img_btn.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_ne!(ui.inline_images, inline_before);

        let auto_drawer_before = ui.auto_detail_drawer;
        let _ = render_buffer_text(&app, &mut ui, 140, 36);
        action::dispatch(
            Action::MouseClick {
                row: dir_drawer_btn.y,
                col: dir_drawer_btn.x + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_ne!(ui.auto_detail_drawer, auto_drawer_before);

        // 4. ImagePreviewOverlay: click inside stays open; click outside or on [✕ Esc] closes
        let att = AttachmentItem {
            id: "img-1".into(),
            filename: "diagram.png".into(),
            mime_type: "image/png".into(),
            size: 1024,
            author_name: None,
            created_at: String::new(),
            content_url: String::new(),
            thumbnail_url: None,
        };
        let ov = ImagePreviewOverlay::from_attachments("ATL-1", &[att], None).unwrap();
        ui.push_overlay(Overlay::ImagePreview(ov));
        let _ = render_buffer_text(&app, &mut ui, 120, 32);

        let body = ui
            .hitmap
            .region_of(HitTarget::OverlayBody)
            .expect("ImagePreviewOverlay must register OverlayBody");
        // Click inside modal -> stays open
        action::dispatch(
            Action::MouseClick {
                row: body.y + 3,
                col: body.x + 5,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(
            ui.overlays.last().map(|o| o.kind()),
            Some(OverlayKind::ImagePreview)
        );
        // Click outside modal (top-left corner 0,0) -> closes cleanly
        action::dispatch(Action::MouseClick { row: 0, col: 0 }, &mut app, &mut ui);
        assert!(!ui.has_overlay());
    }
}
