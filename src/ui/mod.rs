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
    hitmap::HitTarget,
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
            my_work_view::draw(frame, main_rect, app, ui);
        }
        WorkspaceTab::Confluence => {
            confluence_view::draw(frame, nav_rect, main_rect, drawer_rect, app, ui);
        }
        WorkspaceTab::Search => {
            search_view::draw(frame, main_rect, app, ui);
        }
        WorkspaceTab::Directory => {
            directory_view::draw(frame, main_rect, app, ui);
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
    for overlay in &ui.overlays {
        overlay.draw(frame, area, app, ui, &mut hitmap);
    }
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

    let site = format!(" {} ", site_display_english(app));
    x += site.width() as u16;
    spans.push(Span::styled(
        site,
        if theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().bg(theme.bg_header).fg(theme.fg_muted)
        },
    ));

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
        spans.push(Span::styled(
            " ".repeat((remaining - right_w) as usize),
            bar_style,
        ));
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

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState) {
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

    let (left_msg, left_style) = if let Some(toast) = ui.toasts.current() {
        let fg = if theme.no_color {
            Color::Reset
        } else {
            match toast.kind {
                ToastKind::Info => theme.accent_soft,
                ToastKind::Success => theme.success,
                ToastKind::Error => theme.error,
            }
        };
        (
            format!(" {} ", toast.message),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        )
    } else if let Some(task) = ui.tasks.first_running() {
        (
            format!(" {} {} ", spinner_glyph(), task.display()),
            Style::default()
                .bg(bg)
                .fg(if theme.no_color {
                    Color::Reset
                } else {
                    theme.accent_soft
                }),
        )
    } else if app.syncing {
        (
            format!(" {} Syncing workspace… ", spinner_glyph()),
            Style::default()
                .bg(bg)
                .fg(if theme.no_color {
                    Color::Reset
                } else {
                    theme.accent_soft
                }),
        )
    } else {
        let hist = if app.nav_history.is_empty() {
            String::new()
        } else {
            format!("[Backspace] Back ({}) · ", app.nav_history.len())
        };
        let rail_hint = if !ui.show_nav_rail {
            "Nav Hidden · "
        } else {
            ""
        };
        (
            format!(
                " {focus_badge} {hist}{rail_hint}[Tab] Pane · [1-6] Tabs · [Ctrl+P] Palette · [?] Help · [q] Quit "
            ),
            if theme.no_color {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default().bg(bg).fg(theme.fg_muted)
            },
        )
    };

    let right_meta = format!(" {} ", app.config.density.label());
    let right_w = right_meta.width();
    let total_w = area.width as usize;

    if total_w > right_w + 16 {
        let left_budget = total_w - right_w;
        let fitted = truncate_width(&left_msg, left_budget);
        let pad = left_budget.saturating_sub(fitted.width());
        let right_style = if theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().bg(bg).fg(theme.fg_secondary)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(fitted, left_style),
                Span::styled(" ".repeat(pad), bar_style),
                Span::styled(right_meta, right_style),
            ]))
            .style(bar_style),
            area,
        );
    } else {
        let fitted = truncate_width(&left_msg, total_w);
        let pad = total_w.saturating_sub(fitted.width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(fitted, left_style),
                Span::styled(" ".repeat(pad), bar_style),
            ]))
            .style(bar_style),
            area,
        );
    }
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
            has_children: true,
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
}
