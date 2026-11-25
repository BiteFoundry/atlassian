//! Tab 2 (`Ctrl+3`): Minimal-chrome My Work Hub — Assigned Jira Issues & Recent Confluence Pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::DensityMode,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, draw_vertical_hairline,
        hitmap::{ButtonId, HitTarget},
        theme::Theme,
    },
};

/// Optional multi-site federation metadata passed by the integration layer into [`draw_with_scope`].
#[derive(Debug, Clone, Default)]
pub struct MyWorkScope<'a> {
    /// Active scope display label (`None` => `All Sites`, `Some("acme")` => single site).
    pub active_site_label: Option<&'a str>,
    /// Optional per-issue site labels aligned by index with `app.my_issues`.
    pub issue_site_labels: &'a [Option<String>],
    /// Optional per-page site labels aligned by index with `app.my_pages`.
    pub page_site_labels: &'a [Option<String>],
}

/// Formats a compact federated site pill (e.g. `[acme]` / `[corp-dc]`) using
/// `theme.chip_bg` and `theme.chip_fg`. Returns `None` for empty labels. Zero emoji.
pub fn format_site_pill(site_label: Option<&str>, theme: &Theme) -> Option<Span<'static>> {
    let label = site_label?.trim();
    if label.is_empty() {
        return None;
    }
    let short = truncate_width(label, 18);
    let style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.chip_bg)
            .fg(theme.chip_fg)
            .add_modifier(Modifier::BOLD)
    };
    Some(Span::styled(format!("[{short}]"), style))
}

/// Formats the active scope badge displayed in My Work headers (`All Sites` vs single site name).
pub fn format_scope_badge(active_scope: Option<&str>, theme: &Theme) -> Span<'static> {
    let label = active_scope
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_width(s, 22))
        .unwrap_or_else(|| "All Sites".to_string());
    let style = if theme.no_color {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.chip_bg)
            .fg(theme.chip_fg)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" {label} "), style)
}

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    draw_with_scope(frame, main_area, app, ui, &MyWorkScope::default());
}

/// Draws the My Work Hub with optional federated scope badge and per-row site pills.
pub fn draw_with_scope(
    frame: &mut Frame,
    main_area: Rect,
    app: &AppState,
    ui: &mut UiState,
    scope: &MyWorkScope<'_>,
) {
    if main_area.width < 10 || main_area.height < 4 {
        return;
    }

    if main_area.width >= 84 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(55),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(main_area);

        draw_my_issues(frame, cols[0], app, ui, scope);
        draw_vertical_hairline(
            frame,
            cols[1].x,
            cols[1].y,
            cols[1].height,
            ui.theme.subtle_style(),
        );
        draw_recent_pages(frame, cols[2], app, ui, scope);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(3),
            ])
            .split(main_area);

        draw_my_issues(frame, rows[0], app, ui, scope);
        draw_horizontal_hairline(
            frame,
            rows[1].x,
            rows[1].y,
            rows[1].width,
            ui.theme.subtle_style(),
        );
        draw_recent_pages(frame, rows[2], app, ui, scope);
    }
}

fn draw_my_issues(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    scope: &MyWorkScope<'_>,
) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 0;
    ui.hitmap.add(area, HitTarget::MyWorkIssuesPane);
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    let title_text = format!("{mark} Assigned Open Issues ({})", app.my_issues.len());
    let scope_badge = format_scope_badge(scope.active_site_label, theme);
    let title_w = title_text.width();
    let badge_w = scope_badge.width();
    let hdr_line = if (area.width as usize) > title_w + badge_w + 2 {
        let badge_x = hdr_rect.x.saturating_add((title_w + 2) as u16);
        ui.hitmap.add_line(
            badge_x,
            hdr_rect.y,
            badge_w as u16,
            HitTarget::Button(ButtonId::SiteSwitcher),
        );
        Line::from(vec![
            Span::styled(title_text, theme.title(active)),
            Span::raw("  "),
            scope_badge,
        ])
    } else {
        Line::from(Span::styled(
            truncate_width(&title_text, area.width as usize),
            theme.title(active),
        ))
    };
    frame.render_widget(Paragraph::new(hdr_line), hdr_rect);
    draw_horizontal_hairline(
        frame,
        area.x,
        area.y + 1,
        area.width,
        if active {
            theme.accent_style()
        } else {
            theme.subtle_style()
        },
    );

    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.my_issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  No assigned open issues. Press [r] to sync.",
                theme.muted_style(),
            )),
            inner,
        );
        return;
    }

    let row_lines = match app.config.density {
        DensityMode::Cozy => 2usize,
        DensityMode::Compact => 1usize,
    };
    let max_items = ((inner.height as usize) / row_lines).max(1);
    let sel = ui
        .my_work_ui
        .selected_issue
        .min(app.my_issues.len().saturating_sub(1));
    let start = if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    };

    let mut lines = Vec::new();
    let mut cursor_y = inner.y;
    for (vis_idx, issue) in app
        .my_issues
        .iter()
        .skip(start)
        .take(max_items)
        .enumerate()
    {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if active && is_sel {
            theme.nav_selection()
        } else if is_sel {
            theme.selection()
        } else {
            Style::default()
        };
        let (icon, icon_color) = theme.issue_type_badge(&issue.issue_type);
        let lozenge = theme.status_lozenge_span(&issue.status.name, issue.status.category);
        let site_pill = scope
            .issue_site_labels
            .get(abs_idx)
            .and_then(|opt| format_site_pill(opt.as_deref(), theme));
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            let (prio_icon, prio_color) = theme.priority_badge(issue.priority.as_ref());
            let mut top_spans = vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            ];
            if let Some(pill) = site_pill {
                top_spans.push(pill);
                top_spans.push(Span::styled(" ", row_style));
            }
            top_spans.push(Span::styled(
                format!("{} ", issue.key),
                theme.accent_style().patch(row_style),
            ));
            top_spans.push(lozenge);
            top_spans.push(Span::styled(
                format!(" {prio_icon}"),
                Style::default().fg(prio_color),
            ));
            lines.push(Line::from(top_spans));
            lines.push(Line::from(Span::styled(
                truncate_width(&format!("    {}", issue.summary), w),
                row_style,
            )));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkIssue(abs_idx));
            ui.hitmap.add_line(
                inner.x,
                cursor_y + 1,
                inner.width,
                HitTarget::MyWorkIssue(abs_idx),
            );
            cursor_y += 2;
        } else {
            let pill_w = site_pill.as_ref().map_or(0, |p| p.width() + 1);
            let sum_budget = w.saturating_sub(issue.key.len() + lozenge.width() + pill_w + 7);
            let mut row_spans = vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            ];
            if let Some(pill) = site_pill {
                row_spans.push(pill);
                row_spans.push(Span::styled(" ", row_style));
            }
            row_spans.push(Span::styled(
                format!("{} ", issue.key),
                theme.accent_style().patch(row_style),
            ));
            row_spans.push(lozenge);
            row_spans.push(Span::styled(
                format!(" {}", truncate_width(&issue.summary, sum_budget)),
                row_style,
            ));
            lines.push(Line::from(row_spans));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkIssue(abs_idx));
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_recent_pages(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    scope: &MyWorkScope<'_>,
) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 1;
    ui.hitmap.add(area, HitTarget::MyWorkPagesPane);
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    let title_text = format!("{mark} Recent Confluence Pages ({})", app.my_pages.len());
    let scope_badge = format_scope_badge(scope.active_site_label, theme);
    let title_w = title_text.width();
    let badge_w = scope_badge.width();
    let hdr_line = if (area.width as usize) > title_w + badge_w + 2 {
        let badge_x = hdr_rect.x.saturating_add((title_w + 2) as u16);
        ui.hitmap.add_line(
            badge_x,
            hdr_rect.y,
            badge_w as u16,
            HitTarget::Button(ButtonId::SiteSwitcher),
        );
        Line::from(vec![
            Span::styled(title_text, theme.title(active)),
            Span::raw("  "),
            scope_badge,
        ])
    } else {
        Line::from(Span::styled(
            truncate_width(&title_text, area.width as usize),
            theme.title(active),
        ))
    };
    frame.render_widget(Paragraph::new(hdr_line), hdr_rect);
    draw_horizontal_hairline(
        frame,
        area.x,
        area.y + 1,
        area.width,
        if active {
            theme.accent_style()
        } else {
            theme.subtle_style()
        },
    );

    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.my_pages.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  No recent pages. Press [r] to sync.",
                theme.muted_style(),
            )),
            inner,
        );
        return;
    }

    let row_lines = match app.config.density {
        DensityMode::Cozy => 2usize,
        DensityMode::Compact => 1usize,
    };
    let max_items = ((inner.height as usize) / row_lines).max(1);
    let sel = ui
        .my_work_ui
        .selected_page
        .min(app.my_pages.len().saturating_sub(1));
    let start = if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    };

    let teal_bold = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.confluence_teal)
            .add_modifier(Modifier::BOLD)
    };

    let mut lines = Vec::new();
    let mut cursor_y = inner.y;
    for (vis_idx, page) in app.my_pages.iter().skip(start).take(max_items).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if active && is_sel {
            theme.nav_selection()
        } else if is_sel {
            theme.selection()
        } else {
            Style::default()
        };
        let meta = format!("[{}] v{}", page.space_key, page.version_number);
        let site_pill = scope
            .page_site_labels
            .get(abs_idx)
            .and_then(|opt| format_site_pill(opt.as_deref(), theme));
        let pill_w = site_pill.as_ref().map_or(0, |p| p.width() + 1);
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            let mut top_spans = vec![Span::styled(if is_sel { "▸ " } else { "  " }, row_style)];
            if let Some(pill) = site_pill {
                top_spans.push(pill);
                top_spans.push(Span::styled(" ", row_style));
            }
            top_spans.push(Span::styled(
                format!("{meta} "),
                teal_bold.patch(row_style),
            ));
            top_spans.push(Span::styled(
                truncate_width(&page.updated_at, w.saturating_sub(meta.len() + pill_w + 4)),
                theme.muted_style().patch(row_style),
            ));
            lines.push(Line::from(top_spans));
            lines.push(Line::from(Span::styled(
                truncate_width(&format!("    {}", page.title), w),
                row_style,
            )));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkPage(abs_idx));
            ui.hitmap.add_line(
                inner.x,
                cursor_y + 1,
                inner.width,
                HitTarget::MyWorkPage(abs_idx),
            );
            cursor_y += 2;
        } else {
            let title_budget = w.saturating_sub(meta.len() + pill_w + 5);
            let mut row_spans = vec![Span::styled(if is_sel { "▸ " } else { "  " }, row_style)];
            if let Some(pill) = site_pill {
                row_spans.push(pill);
                row_spans.push(Span::styled(" ", row_style));
            }
            row_spans.push(Span::styled(
                format!("{meta} "),
                teal_bold.patch(row_style),
            ));
            row_spans.push(Span::styled(
                truncate_width(&page.title, title_budget),
                row_style,
            ));
            lines.push(Line::from(row_spans));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkPage(abs_idx));
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona, ThemePreset},
        models::{IssueStatus, IssueSummary, PageSummary, StatusCategory},
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn test_app() -> (AppState, UiState) {
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
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        app.my_issues = vec![
            IssueSummary {
                key: "ENG-101".into(),
                summary: "Ship multi-site switcher".into(),
                status: IssueStatus {
                    id: "1".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
                ..Default::default()
            },
            IssueSummary {
                key: "OPS-42".into(),
                summary: "Audit on-prem certificates".into(),
                status: IssueStatus {
                    id: "2".into(),
                    name: "To Do".into(),
                    category: StatusCategory::Todo,
                },
                ..Default::default()
            },
        ];
        app.my_pages = vec![PageSummary {
            id: "100".into(),
            space_key: "DOCS".into(),
            title: "Federation Runbook".into(),
            version_number: 4,
            updated_at: "2026-11-19".into(),
            ..Default::default()
        }];
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[test]
    fn format_site_pill_handles_empty_and_styled_labels() {
        let dark = Theme::midnight_dark();
        assert!(format_site_pill(None, &dark).is_none());
        assert!(format_site_pill(Some("   "), &dark).is_none());

        let pill = format_site_pill(Some("acme"), &dark).unwrap();
        assert_eq!(pill.content.as_ref(), "[acme]");
        assert_eq!(pill.style.bg, Some(dark.chip_bg));
        assert_eq!(pill.style.fg, Some(dark.chip_fg));

        let dc_pill = format_site_pill(Some("corp-dc"), &Theme::no_color()).unwrap();
        assert_eq!(dc_pill.content.as_ref(), "[corp-dc]");
    }

    #[test]
    fn format_scope_badge_defaults_to_all_sites_or_single_site() {
        let theme = Theme::midnight_dark();
        assert_eq!(format_scope_badge(None, &theme).content.as_ref(), " All Sites ");
        assert_eq!(
            format_scope_badge(Some("  "), &theme).content.as_ref(),
            " All Sites "
        );
        assert_eq!(
            format_scope_badge(Some("corp-dc"), &theme).content.as_ref(),
            " corp-dc "
        );
    }

    #[tokio::test]
    async fn my_work_renders_federated_pills_and_scope_badge() {
        let (mut app, mut ui) = test_app();
        let issue_labels = vec![Some("acme".to_string()), Some("corp-dc".to_string())];
        let page_labels = vec![Some("acme".to_string())];
        let scope = MyWorkScope {
            active_site_label: None,
            issue_site_labels: &issue_labels,
            page_site_labels: &page_labels,
        };

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_with_scope(f, f.area(), &app, &mut ui, &scope))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("All Sites"), "missing All Sites header badge:\n{text}");
        assert!(text.contains("[acme]"), "missing [acme] pill:\n{text}");
        assert!(text.contains("[corp-dc]"), "missing [corp-dc] pill:\n{text}");
        assert!(text.contains("ENG-101"));
        assert!(text.contains("Federation Runbook"));

        // Compact density + single-site scope + NoColor preset
        app.config.density = DensityMode::Compact;
        ui.theme = Theme::detect(ThemePreset::NoColor);
        let single_scope = MyWorkScope {
            active_site_label: Some("corp-dc"),
            issue_site_labels: &issue_labels,
            page_site_labels: &page_labels,
        };
        terminal
            .draw(|f| draw_with_scope(f, f.area(), &app, &mut ui, &single_scope))
            .unwrap();
        let compact_text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(compact_text.contains("corp-dc"));
        assert!(compact_text.contains("[corp-dc]"));
    }
}
