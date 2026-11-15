//! Tab 4 (`Ctrl+5`): Unified local FTS5 + `Ctrl+R` remote JQL & CQL parallel search.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    models::SearchHitKind,
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::truncate_width,
        hitmap::HitTarget,
    },
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(main_area);

    let bar_focused = ui.focus == PaneFocus::SearchBar;
    let mode_lbl = if ui.search_ui.is_remote {
        "원격 JQL+CQL"
    } else {
        "로컬 FTS5"
    };
    let bar_block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 통합 검색 [{mode_lbl}] (/ 포커스 · Ctrl+R 원격검색 · Enter 열기) "),
            theme.title(bar_focused),
        )))
        .border_style(theme.border_style(bar_focused));
    let bar_inner = bar_block.inner(rows[0]);
    frame.render_widget(bar_block, rows[0]);
    ui.hitmap.add(rows[0], HitTarget::SearchBar);

    if bar_inner.width > 0 && bar_inner.height > 0 {
        let mut spans = vec![
            Span::styled(" 🔍 ", theme.accent_style()),
            Span::raw(ui.search_ui.query.text.clone()),
        ];
        if bar_focused {
            spans.push(Span::styled("▏", theme.accent_style()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), bar_inner);
    }

    let list_focused = ui.focus == PaneFocus::Main;
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 검색 결과 ({}) ", app.search_hits.len()),
            theme.title(list_focused),
        )))
        .border_style(theme.border_style(list_focused));
    let list_inner = list_block.inner(rows[1]);
    frame.render_widget(list_block, rows[1]);
    if list_inner.width == 0 || list_inner.height == 0 {
        return;
    }

    if app.search_hits.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  검색 결과가 없습니다. `/`를 눌러 검색어를 입력하거나 `Ctrl+R`로 원격 JQL+CQL 검색을 실행하세요.",
                theme.muted_style(),
            )),
            list_inner,
        );
        return;
    }

    let max_rows = list_inner.height as usize;
    let sel = ui
        .search_ui
        .selected_hit
        .min(app.search_hits.len().saturating_sub(1));
    let start = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
    let mut lines = Vec::new();
    for (vis_idx, hit) in app.search_hits.iter().skip(start).take(max_rows).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if is_sel {
            theme.selection()
        } else {
            Style::default()
        };
        let badge = match hit.kind {
            SearchHitKind::JiraIssue => Span::styled(
                " JIRA ",
                Style::default()
                    .bg(theme.jira_cobalt)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            SearchHitKind::ConfluencePage => Span::styled(
                " WIKI ",
                Style::default()
                    .bg(theme.confluence_teal)
                    .fg(Color::Rgb(9, 30, 66))
                    .add_modifier(Modifier::BOLD),
            ),
        };
        let head = format!(" {} [{}] ", hit.id_or_key, hit.container);
        let title_budget = list_inner
            .width
            .saturating_sub(head.len() as u16 + 10)
            as usize;
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            badge,
            Span::styled(head, theme.accent_style().patch(row_style)),
            Span::styled(truncate_width(&hit.title, title_budget), row_style),
        ]));
        ui.hitmap.add_line(
            list_inner.x,
            list_inner.y + vis_idx as u16,
            list_inner.width,
            HitTarget::SearchHit(abs_idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), list_inner);
}
