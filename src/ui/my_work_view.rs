//! Tab 2 (`Ctrl+3`): My Work Hub — My Open Assigned Jira Issues + Recently Updated Confluence Pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::HitTarget,
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    let cols = if main_area.width >= 84 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(main_area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(main_area)
    };

    draw_my_issues(frame, cols[0], app, ui);
    draw_recent_pages(frame, cols[1], app, ui);
}

fn draw_my_issues(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 0;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 🔷 내게 할당된 미해결 이슈 ({}) ", app.my_issues.len()),
            theme.title(active),
        )))
        .border_style(theme.border_style(active));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.my_issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  할당된 미해결 이슈가 없습니다 ([r]로 동기화).",
                theme.muted_style(),
            )),
            inner,
        );
        return;
    }

    let max_rows = inner.height as usize;
    let sel = ui
        .my_work_ui
        .selected_issue
        .min(app.my_issues.len().saturating_sub(1));
    let start = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
    let mut lines = Vec::new();
    for (vis_idx, issue) in app.my_issues.iter().skip(start).take(max_rows).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if active && is_sel {
            theme.selection()
        } else if is_sel {
            Style::default().bg(theme.card_selected_bg)
        } else {
            Style::default()
        };
        let (icon, icon_color) = theme.issue_type_badge(&issue.issue_type);
        let lozenge = theme.status_lozenge_span(&issue.status.name, issue.status.category);
        let sum_budget = inner
            .width
            .saturating_sub(issue.key.len() as u16 + lozenge.width() as u16 + 7)
            as usize;
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            Span::styled(format!("{} ", issue.key), theme.accent_style().patch(row_style)),
            lozenge,
            Span::styled(
                format!(" {}", truncate_width(&issue.summary, sum_budget)),
                row_style,
            ),
        ]));
        ui.hitmap.add_line(
            inner.x,
            inner.y + vis_idx as u16,
            inner.width,
            HitTarget::MyWorkIssue(abs_idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_recent_pages(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 1;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 📘 최근 업데이트 Confluence 페이지 ({}) ", app.my_pages.len()),
            theme.title(active),
        )))
        .border_style(theme.border_style(active));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.my_pages.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  최근 페이지가 없습니다 ([r]로 동기화).",
                theme.muted_style(),
            )),
            inner,
        );
        return;
    }

    let max_rows = inner.height as usize;
    let sel = ui
        .my_work_ui
        .selected_page
        .min(app.my_pages.len().saturating_sub(1));
    let start = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
    let mut lines = Vec::new();
    for (vis_idx, page) in app.my_pages.iter().skip(start).take(max_rows).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let row_style = if active && is_sel {
            theme.selection()
        } else if is_sel {
            Style::default().bg(theme.card_selected_bg)
        } else {
            Style::default()
        };
        let meta = format!("[{}] v{}", page.space_key, page.version_number);
        let title_budget = inner.width.saturating_sub(meta.len() as u16 + 7) as usize;
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
            Span::styled(
                format!("{meta} "),
                Style::default()
                    .fg(theme.confluence_teal)
                    .add_modifier(Modifier::BOLD)
                    .patch(row_style),
            ),
            Span::styled(truncate_width(&page.title, title_budget), row_style),
        ]));
        ui.hitmap.add_line(
            inner.x,
            inner.y + vis_idx as u16,
            inner.width,
            HitTarget::MyWorkPage(abs_idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
