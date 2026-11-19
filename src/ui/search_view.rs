//! Tab 4 (`Ctrl+5`): Minimal-chrome Unified Local FTS5 + `Ctrl+R` Remote JQL & CQL Parallel Search.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    models::SearchHitKind,
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, fill_surface,
        hitmap::HitTarget,
    },
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
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
            "   [/] Focus · [Ctrl+R] Remote · [Enter] Open",
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

    // Results header + list
    let list_focused = ui.focus == PaneFocus::Main;
    let hdr_rect = Rect::new(rows[1].x, rows[1].y, rows[1].width, 1);
    let mark = if list_focused { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{mark} Results ({})", app.search_hits.len()),
            theme.title(list_focused),
        ))),
        hdr_rect,
    );

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
        let head = format!(" {} [{}] ", hit.id_or_key, hit.container);
        let w = list_inner.width as usize;
        let title_budget = w.saturating_sub(head.len() + 10);

        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            badge,
            Span::styled(head, theme.accent_style().patch(row_style)),
            Span::styled(truncate_width(&hit.title, title_budget), row_style),
        ]));
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
