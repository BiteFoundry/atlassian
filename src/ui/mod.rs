//! Unified Jira + Confluence UI module (`ratatui`): Persona Header Bar, 3-tier responsive layout, and rendering pipeline.

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

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use self::{
    app::{AppState, PaneFocus, UiState, WorkspaceTab, TAB_LABELS},
    doc_view::truncate_width,
    hitmap::HitTarget,
    toast::ToastKind,
};

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

pub fn draw(frame: &mut Frame, app: &AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.hitmap.clear();

    if area.width < 12 || area.height < 5 {
        return None;
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
    let has_nav = matches!(
        ui.active_tab,
        WorkspaceTab::Board | WorkspaceTab::Issues | WorkspaceTab::Confluence
    );
    let has_drawer = match ui.active_tab {
        WorkspaceTab::Board => ui.board_ui.drawer_open && mode == LayoutMode::Wide,
        WorkspaceTab::Confluence => ui.confluence_ui.toc_open && mode == LayoutMode::Wide,
        _ => false,
    };

    let (nav_rect, main_rect, drawer_rect) = if !has_nav {
        (None, body, None)
    } else {
        match mode {
            LayoutMode::Wide => {
                let nav_w = 28u16;
                let drawer_w = if has_drawer {
                    (body.width * 34 / 100).clamp(34, 54)
                } else {
                    0
                };
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(nav_w),
                        Constraint::Min(30),
                        Constraint::Length(drawer_w),
                    ])
                    .split(body);
                (
                    Some(cols[0]),
                    cols[1],
                    (drawer_w > 0).then_some(cols[2]),
                )
            }
            LayoutMode::Medium => {
                let nav_w = 24u16;
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(nav_w), Constraint::Min(28)])
                    .split(body);
                (Some(cols[0]), cols[1], None)
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
    let header_bg = Color::Rgb(16, 24, 40);
    let mut spans = Vec::new();
    let mut x = area.x;

    let badge = theme.persona_badge(app.persona);
    x += badge.content.width() as u16;
    spans.push(badge);

    let site = format!(" {} ", app.site_display());
    x += site.width() as u16;
    spans.push(Span::styled(
        site,
        Style::default().bg(header_bg).fg(theme.muted),
    ));

    for (idx, label) in TAB_LABELS.iter().enumerate() {
        let active = ui.active_tab.index() == idx;
        let text = format!(" {label} ");
        let w = text.width() as u16;
        let style = if active {
            let active_bg = match idx {
                3 => theme.confluence_teal,
                _ => theme.jira_cobalt,
            };
            let active_fg = if idx == 3 {
                Color::Rgb(9, 30, 66)
            } else {
                Color::White
            };
            Style::default()
                .bg(active_bg)
                .fg(active_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(header_bg).fg(Color::Rgb(180, 192, 212))
        };
        if x + w <= area.x + area.width {
            ui.hitmap.add_line(x, area.y, w, HitTarget::Tab(idx));
        }
        x += w;
        spans.push(Span::styled(text, style));
    }

    // Right side: user & sync status
    let user_str = app
        .me
        .as_ref()
        .map(|u| format!("@{}", u.display_name))
        .unwrap_or_else(|| app.auth.status_summary());
    let sync_icon = if app.syncing || ui.tasks.first_running().is_some() {
        "⟳ 동기화"
    } else {
        "● 연결"
    };
    let right_text = format!(" {user_str} · {sync_icon} ");
    let right_w = right_text.width() as u16;
    let remaining = (area.x + area.width).saturating_sub(x);
    if remaining > right_w {
        spans.push(Span::styled(
            " ".repeat((remaining - right_w) as usize),
            Style::default().bg(header_bg),
        ));
        spans.push(Span::styled(
            right_text,
            Style::default().bg(header_bg).fg(theme.confluence_teal),
        ));
    } else if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining as usize),
            Style::default().bg(header_bg),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState) {
    let theme = &ui.theme;
    let bg = Color::Rgb(18, 22, 32);

    let (msg, style) = if let Some(toast) = ui.toasts.current() {
        let fg = match toast.kind {
            ToastKind::Info => theme.accent,
            ToastKind::Success => theme.success,
            ToastKind::Error => theme.error,
        };
        (
            format!(" {} ", toast.message),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        )
    } else if let Some(task) = ui.tasks.first_running() {
        (
            format!(" ⟳ {} ", task.display()),
            Style::default().bg(bg).fg(theme.confluence_teal),
        )
    } else {
        let hist = if app.nav_history.is_empty() {
            String::new()
        } else {
            format!(" [Backspace]뒤로({}) ·", app.nav_history.len())
        };
        (
            format!(
                " {hist} [Tab]패널이동 · [Ctrl+1..6]탭 · [Ctrl+P / :]팔레트 · [?]도움말 · [q]종료 "
            ),
            Style::default().bg(bg).fg(theme.muted),
        )
    };

    let fitted = truncate_width(&msg, area.width as usize);
    let pad = (area.width as usize).saturating_sub(fitted.width());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(fitted, style),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ])),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona},
        models::{
            IssueDetail, IssuePriority, IssueStatus, IssueSummary, IssueType, PageDetail,
            PageSummary, SpaceSummary, StatusCategory,
        },
        storage::Storage,
        sync,
        ui::{action, event, keymap::Action},
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
                summary: "칸반 보드 구현".into(),
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
                summary: "Confluence 트리 렌더링".into(),
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
            description_markdown: "관련 문서 확인".into(),
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
            title: "시스템 아키텍처".into(),
            version_number: 3,
            has_children: true,
            ..Default::default()
        };
        app.root_pages_by_space
            .insert("ENG".into(), vec![root_page.clone()]);
        app.active_page = Some(PageDetail {
            summary: root_page,
            body_markdown: "ATL-1 이슈와 연결됨".into(),
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
        assert!(screen.contains("JIRA"), "expected JIRA badge:\n{screen}");
        assert!(screen.contains("acme.atlassian.net"), "expected site:\n{screen}");
        assert!(screen.contains("ATL-1"), "expected card ATL-1:\n{screen}");
        assert!(screen.contains("ATL-2"), "expected card ATL-2:\n{screen}");

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
            screen.contains("CONFLUENCE"),
            "expected CONFLUENCE badge:\n{screen}"
        );
        assert!(
            screen.contains("시스템 아키텍처"),
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
