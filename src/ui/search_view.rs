//! Tab 4 (`Ctrl+5`): Minimal-chrome Unified Local FTS5 + `Ctrl+R` Remote JQL & CQL Parallel Search.

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
    models::SearchHitKind,
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, fill_surface,
        hitmap::HitTarget,
        my_work_view::format_site_pill,
        theme::Theme,
    },
};

/// Optional multi-site federation metadata passed by the integration layer into [`draw_with_scope`].
#[derive(Debug, Clone, Default)]
pub struct SearchScope<'a> {
    /// Active search scope label (`None` => `All Sites`, `Some("acme")` => single site).
    pub active_site_label: Option<&'a str>,
    /// Optional per-hit site labels aligned by index with `app.search_hits`.
    pub hit_site_labels: &'a [Option<String>],
}

/// Formats a compact federated site pill for a search result row (reusing `format_site_pill`).
pub fn format_search_site_pill(site_label: Option<&str>, theme: &Theme) -> Option<Span<'static>> {
    format_site_pill(site_label, theme)
}

/// Formats the search scope indicator displayed in the search header (`Scope: All Sites` or active site).
pub fn format_search_scope(active_scope: Option<&str>, theme: &Theme) -> Span<'static> {
    let label = active_scope
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_width(s, 22))
        .unwrap_or_else(|| "All Sites".to_string());
    let style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.chip_bg)
            .fg(theme.chip_fg)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" Scope: {label} "), style)
}

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    draw_with_scope(frame, main_area, app, ui, &SearchScope::default());
}

/// Draws Unified Search with optional federated scope indicator and per-hit site pills.
pub fn draw_with_scope(
    frame: &mut Frame,
    main_area: Rect,
    app: &AppState,
    ui: &mut UiState,
    scope: &SearchScope<'_>,
) {
    let theme = &ui.theme;
    if main_area.width < 10 || main_area.height < 4 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(2)])
        .split(main_area);

    // Top minimal-chrome search bar
    let bar_focused = ui.focus == PaneFocus::SearchBar;
    let mode_lbl = if ui.search_ui.is_remote {
        "Remote JQL+CQL"
    } else {
        "Local FTS5"
    };
    let bar_surface = theme.surface_style(if bar_focused {
        theme.bg_elevated
    } else {
        theme.bg_header
    });
    let input_rect = Rect::new(rows[0].x, rows[0].y, rows[0].width, 1);
    fill_surface(frame, input_rect, bar_surface);
    ui.hitmap.add(rows[0], HitTarget::SearchBar);

    let mut bar_spans = vec![
        Span::styled(
            format!(" Search [{mode_lbl}]> "),
            theme.accent_style().patch(bar_surface),
        ),
        Span::styled(ui.search_ui.query.text.clone(), bar_surface),
    ];
    if bar_focused {
        bar_spans.push(Span::styled("▏", theme.accent_style().patch(bar_surface)));
    } else {
        bar_spans.push(Span::styled(
            "   [/] Focus · [Ctrl+R] Remote · [Ctrl+S] Site · [Enter] Open",
            theme.muted_style().patch(bar_surface),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(bar_spans)).style(bar_surface),
        input_rect,
    );
    draw_horizontal_hairline(
        frame,
        rows[0].x,
        rows[0].y + 1,
        rows[0].width,
        if bar_focused {
            theme.accent_style()
        } else {
            theme.subtle_style()
        },
    );

    // Results header + active scope indicator
    let list_focused = ui.focus == PaneFocus::Main;
    let hdr_rect = Rect::new(rows[1].x, rows[1].y, rows[1].width, 1);
    let mark = if list_focused { "▸" } else { " " };
    let results_title = format!("{mark} Results ({})", app.search_hits.len());
    let scope_span = format_search_scope(scope.active_site_label, theme);
    let hdr_line = if (hdr_rect.width as usize) > results_title.width() + scope_span.width() + 2 {
        Line::from(vec![
            Span::styled(results_title, theme.title(list_focused)),
            Span::raw("  "),
            scope_span,
        ])
    } else {
        Line::from(Span::styled(
            truncate_width(&results_title, hdr_rect.width as usize),
            theme.title(list_focused),
        ))
    };
    frame.render_widget(Paragraph::new(hdr_line), hdr_rect);

    let list_inner = Rect::new(
        rows[1].x,
        rows[1].y + 1,
        rows[1].width,
        rows[1].height.saturating_sub(1),
    );
    if list_inner.width == 0 || list_inner.height == 0 {
        return;
    }

    if app.search_hits.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  No search results. Press [/] to search locally or [Ctrl+R] for remote JQL+CQL search.",
                theme.muted_style(),
            )),
            list_inner,
        );
        return;
    }

    let row_lines = match app.config.density {
        DensityMode::Cozy => 2usize,
        DensityMode::Compact => 1usize,
    };
    let max_items = ((list_inner.height as usize) / row_lines).max(1);
    let sel = ui
        .search_ui
        .selected_hit
        .min(app.search_hits.len().saturating_sub(1));
    let start = if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    };

    let mut lines = Vec::new();
    let mut cursor_y = list_inner.y;
    for (vis_idx, hit) in app
        .search_hits
        .iter()
        .skip(start)
        .take(max_items)
        .enumerate()
    {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if is_sel {
            theme.nav_selection()
        } else {
            Style::default()
        };
        let badge = match hit.kind {
            SearchHitKind::JiraIssue => Span::styled(
                " JIRA ",
                if theme.no_color {
                    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(theme.jira_cobalt)
                        .fg(theme.jira_pill_fg)
                        .add_modifier(Modifier::BOLD)
                },
            ),
            SearchHitKind::ConfluencePage => Span::styled(
                " WIKI ",
                if theme.no_color {
                    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(theme.confluence_teal)
                        .fg(theme.confluence_pill_fg)
                        .add_modifier(Modifier::BOLD)
                },
            ),
        };
        let site_pill = scope
            .hit_site_labels
            .get(abs_idx)
            .and_then(|opt| format_search_site_pill(opt.as_deref(), theme));
        let pill_w = site_pill.as_ref().map_or(0, |p| p.width() + 1);

        let head = format!(" {} [{}] ", hit.id_or_key, hit.container);
        let w = list_inner.width as usize;
        let title_budget = w.saturating_sub(head.width() + pill_w + 10);

        let mut row_spans = vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            badge,
        ];
        if let Some(pill) = site_pill {
            row_spans.push(Span::styled(" ", row_style));
            row_spans.push(pill);
        }
        row_spans.push(Span::styled(head, theme.accent_style().patch(row_style)));
        row_spans.push(Span::styled(
            truncate_width(&hit.title, title_budget),
            row_style,
        ));
        lines.push(Line::from(row_spans));
        ui.hitmap.add_line(
            list_inner.x,
            cursor_y,
            list_inner.width,
            HitTarget::SearchHit(abs_idx),
        );
        cursor_y += 1;

        if row_lines == 2 && cursor_y < list_inner.bottom() {
            let snippet = if hit.snippet.trim().is_empty() {
                hit.updated_at.as_str()
            } else {
                hit.snippet.as_str()
            };
            lines.push(Line::from(Span::styled(
                truncate_width(&format!("    {snippet}"), w),
                theme.muted_style().patch(row_style),
            )));
            ui.hitmap.add_line(
                list_inner.x,
                cursor_y,
                list_inner.width,
                HitTarget::SearchHit(abs_idx),
            );
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), list_inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona, ThemePreset},
        models::UnifiedSearchHit,
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
        app.search_hits = vec![
            UnifiedSearchHit {
                kind: SearchHitKind::JiraIssue,
                id_or_key: "ENG-101".into(),
                container: "ENG".into(),
                title: "Federated FTS5 index".into(),
                snippet: "Composite site_id key prevents collisions".into(),
                status_or_version: "In Progress".into(),
                updated_at: "2026-11-19".into(),
                web_url: "https://acme.atlassian.net/browse/ENG-101".into(),
            },
            UnifiedSearchHit {
                kind: SearchHitKind::ConfluencePage,
                id_or_key: "442".into(),
                container: "DOCS".into(),
                title: "On-prem Migration Notes".into(),
                snippet: "Data Center PAT rotation guide".into(),
                status_or_version: "v2".into(),
                updated_at: "2026-11-18".into(),
                web_url: "https://wiki.corp.internal/pages/442".into(),
            },
        ];
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[test]
    fn format_search_scope_and_pill_render_expected_labels() {
        let theme = Theme::midnight_dark();
        assert_eq!(
            format_search_scope(None, &theme).content.as_ref(),
            " Scope: All Sites "
        );
        assert_eq!(
            format_search_scope(Some("acme"), &theme).content.as_ref(),
            " Scope: acme "
        );
        assert_eq!(
            format_search_site_pill(Some("corp-dc"), &theme)
                .unwrap()
                .content
                .as_ref(),
            "[corp-dc]"
        );
    }

    #[tokio::test]
    async fn search_view_renders_scope_indicator_and_federated_pills() {
        let (app, mut ui) = test_app();
        let labels = vec![Some("acme".to_string()), Some("corp-dc".to_string())];
        let scope = SearchScope {
            active_site_label: None,
            hit_site_labels: &labels,
        };

        let backend = TestBackend::new(110, 18);
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
        assert!(text.contains("Scope: All Sites"), "missing scope indicator:\n{text}");
        assert!(text.contains("[acme]"), "missing [acme] pill:\n{text}");
        assert!(text.contains("[corp-dc]"), "missing [corp-dc] pill:\n{text}");
        assert!(text.contains("ENG-101"));
        assert!(text.contains("On-prem Migration Notes"));

        // Single-site scope + NoColor preset
        ui.theme = Theme::detect(ThemePreset::NoColor);
        let single_scope = SearchScope {
            active_site_label: Some("acme"),
            hit_site_labels: &labels,
        };
        terminal
            .draw(|f| draw_with_scope(f, f.area(), &app, &mut ui, &single_scope))
            .unwrap();
        let single_text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(single_text.contains("Scope: acme"));
    }
}
