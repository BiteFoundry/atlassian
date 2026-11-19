//! Tab 2 (`Ctrl+3`): Minimal-chrome My Work Hub — Assigned Jira Issues & Recent Confluence Pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, draw_vertical_hairline,
        hitmap::HitTarget,
    },
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
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

        draw_my_issues(frame, cols[0], app, ui);
        draw_vertical_hairline(
            frame,
            cols[1].x,
            cols[1].y,
            cols[1].height,
            ui.theme.subtle_style(),
        );
        draw_recent_pages(frame, cols[2], app, ui);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(3),
            ])
            .split(main_area);

        draw_my_issues(frame, rows[0], app, ui);
        draw_horizontal_hairline(
            frame,
            rows[1].x,
            rows[1].y,
            rows[1].width,
            ui.theme.subtle_style(),
        );
        draw_recent_pages(frame, rows[2], app, ui);
    }
}

fn draw_my_issues(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 0;
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(
                &format!("{mark} Assigned Open Issues ({})", app.my_issues.len()),
                area.width as usize,
            ),
            theme.title(active),
        ))),
        hdr_rect,
    );
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
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            let (prio_icon, prio_color) = theme.priority_badge(issue.priority.as_ref());
            lines.push(Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(
                    format!("{} ", issue.key),
                    theme.accent_style().patch(row_style),
                ),
                lozenge,
                Span::styled(format!(" {prio_icon}"), Style::default().fg(prio_color)),
            ]));
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
            let sum_budget = w.saturating_sub(issue.key.len() + lozenge.width() + 7);
            lines.push(Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(
                    format!("{} ", issue.key),
                    theme.accent_style().patch(row_style),
                ),
                lozenge,
                Span::styled(
                    format!(" {}", truncate_width(&issue.summary, sum_budget)),
                    row_style,
                ),
            ]));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkIssue(abs_idx));
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_recent_pages(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.my_work_ui.active_column == 1;
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(
                &format!("{mark} Recent Confluence Pages ({})", app.my_pages.len()),
                area.width as usize,
            ),
            theme.title(active),
        ))),
        hdr_rect,
    );
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
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            lines.push(Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{meta} "), teal_bold.patch(row_style)),
                Span::styled(
                    truncate_width(&page.updated_at, w.saturating_sub(meta.len() + 4)),
                    theme.muted_style().patch(row_style),
                ),
            ]));
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
            let title_budget = w.saturating_sub(meta.len() + 5);
            lines.push(Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                Span::styled(format!("{meta} "), teal_bold.patch(row_style)),
                Span::styled(truncate_width(&page.title, title_budget), row_style),
            ]));
            ui.hitmap
                .add_line(inner.x, cursor_y, inner.width, HitTarget::MyWorkPage(abs_idx));
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
