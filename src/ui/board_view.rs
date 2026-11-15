//! Tab 0 (`Ctrl+1`): Multi-column Native Jira Kanban / Sprint Board.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::{
    models::IssueSummary,
    ui::{
        app::{AppState, KanbanColumn, PaneFocus, UiState},
        doc_view::{truncate_width, wrap_plain_text},
        hitmap::HitTarget,
        issue_view::draw_issue_inspector_pane,
    },
};

const MIN_COL_WIDTH: u16 = 28;
const CARD_HEIGHT: u16 = 6;

pub fn draw(
    frame: &mut Frame,
    nav_area: Option<Rect>,
    main_area: Rect,
    drawer_area: Option<Rect>,
    app: &AppState,
    ui: &mut UiState,
) {
    if let Some(nav_rect) = nav_area {
        draw_board_nav(frame, nav_rect, app, ui);
    }

    draw_kanban_canvas(frame, main_area, app, ui);

    if let Some(drawer_rect) = drawer_area {
        draw_issue_inspector_pane(
            frame,
            drawer_rect,
            app,
            ui,
            ui.focus == PaneFocus::Detail,
            ui.board_ui.detail_scroll,
            true,
        );
    }
}

fn draw_board_nav(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Nav;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" 보드 & 스프린트 ", theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    ui.hitmap.add(area, HitTarget::NavPane);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    let split_h = (app.boards.len() as u16 + 1).clamp(3, (inner.height / 2).max(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(split_h), Constraint::Min(2)])
        .split(inner);

    // Boards section
    let mut b_lines = vec![Line::from(Span::styled(" Agile 보드", theme.muted_style()))];
    if app.boards.is_empty() {
        b_lines.push(Line::from(Span::styled("  (보드 없음)", theme.muted_style())));
    } else {
        for (idx, b) in app
            .boards
            .iter()
            .enumerate()
            .take(rows[0].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.board_ui.selected_board == idx;
            let style = if focused && ui.board_ui.nav_section == 0 && is_sel {
                theme.selection()
            } else if is_sel {
                theme.accent_style()
            } else {
                Style::default()
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let badge = if b.board_type.eq_ignore_ascii_case("scrum") {
                "🏃"
            } else {
                "📋"
            };
            b_lines.push(Line::from(Span::styled(
                truncate_width(&format!("{pfx}{badge} {}", b.name), rows[0].width as usize),
                style,
            )));
            ui.hitmap.add_line(
                rows[0].x,
                rows[0].y + 1 + idx as u16,
                rows[0].width,
                HitTarget::BoardItem(idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(b_lines), rows[0]);

    // Sprints section
    let mut s_lines = vec![Line::from(Span::styled(" 스프린트", theme.muted_style()))];
    if app.sprints.is_empty() {
        s_lines.push(Line::from(Span::styled("  (전체 보드 이슈)", theme.muted_style())));
    } else {
        for (idx, sp) in app
            .sprints
            .iter()
            .enumerate()
            .take(rows[1].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.board_ui.selected_sprint == idx;
            let style = if focused && ui.board_ui.nav_section == 1 && is_sel {
                theme.selection()
            } else if is_sel {
                theme.accent_style()
            } else {
                Style::default()
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let state_tag = match sp.state.as_str() {
                "active" => "●",
                "future" => "○",
                _ => "·",
            };
            s_lines.push(Line::from(Span::styled(
                truncate_width(&format!("{pfx}{state_tag} {}", sp.name), rows[1].width as usize),
                style,
            )));
            ui.hitmap.add_line(
                rows[1].x,
                rows[1].y + 1 + idx as u16,
                rows[1].width,
                HitTarget::SprintItem(idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(s_lines), rows[1]);
}

fn draw_kanban_canvas(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Main;
    let board_name = app
        .active_board(ui)
        .map(|b| b.name.as_str())
        .unwrap_or("칸반 보드");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 🔷 {board_name} — [h/l]컬럼 [j/k]카드 [Enter]상세 [m]상태전환 [n]새이슈 "),
            theme.title(focused),
        )))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    let columns = app.build_kanban_columns(ui);
    if columns.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  컬럼이 없습니다", theme.muted_style())),
            inner,
        );
        return;
    }

    let sel_col = ui.board_ui.selected_col.min(columns.len().saturating_sub(1));
    let visible_count = ((inner.width / MIN_COL_WIDTH) as usize)
        .max(1)
        .min(columns.len());
    let mut col_scroll = ui.board_ui.col_scroll.min(columns.len().saturating_sub(visible_count));
    if sel_col < col_scroll {
        col_scroll = sel_col;
    } else if sel_col >= col_scroll + visible_count {
        col_scroll = sel_col + 1 - visible_count;
    }

    let constraints: Vec<Constraint> = (0..visible_count)
        .map(|_| Constraint::Ratio(1, visible_count as u32))
        .collect();
    let col_rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(inner);

    for (vis_idx, col_rect) in col_rects.iter().enumerate() {
        let abs_col = col_scroll + vis_idx;
        let Some(col) = columns.get(abs_col) else {
            continue;
        };
        let is_active_col = focused && abs_col == sel_col;
        draw_kanban_column(frame, *col_rect, abs_col, col, is_active_col, app, ui);
    }
}

fn draw_kanban_column(
    frame: &mut Frame,
    area: Rect,
    col_idx: usize,
    col: &KanbanColumn,
    is_active_col: bool,
    _app: &AppState,
    ui: &mut UiState,
) {
    let theme = &ui.theme;
    let col_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            theme.status_lozenge_span(&col.title, col.category),
            Span::styled(format!(" ({}) ", col.issues.len()), theme.muted_style()),
        ]))
        .border_style(theme.border_style(is_active_col));
    let inner = col_block.inner(area);
    frame.render_widget(col_block, area);
    if inner.width < 6 || inner.height < 3 {
        return;
    }

    if col.issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (이슈 없음)", theme.muted_style())),
            inner,
        );
        return;
    }

    let cards_fit = (inner.height / CARD_HEIGHT).max(1) as usize;
    let sel_row = if col_idx == ui.board_ui.selected_col {
        ui.board_ui.selected_row.min(col.issues.len().saturating_sub(1))
    } else {
        0
    };
    let start = if sel_row >= cards_fit {
        sel_row - cards_fit + 1
    } else {
        0
    };

    for (vis_idx, issue) in col.issues.iter().skip(start).take(cards_fit).enumerate() {
        let abs_row = start + vis_idx;
        let y = inner.y + (vis_idx as u16) * CARD_HEIGHT;
        let h = CARD_HEIGHT.min(inner.y + inner.height - y);
        if h < 4 {
            break;
        }
        let card_rect = Rect::new(inner.x, y, inner.width, h);
        let selected = col_idx == ui.board_ui.selected_col && abs_row == sel_row;
        draw_kanban_card(frame, card_rect, issue, selected, ui);
        ui.hitmap.add(
            card_rect,
            HitTarget::KanbanCard {
                col: col_idx,
                row: abs_row,
            },
        );
    }
}

fn draw_kanban_card(
    frame: &mut Frame,
    area: Rect,
    issue: &IssueSummary,
    selected: bool,
    ui: &UiState,
) {
    let theme = &ui.theme;
    let bg = if selected {
        theme.card_selected_bg
    } else {
        theme.card_bg
    };
    let card_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if selected {
            Style::default()
                .fg(theme.border_focus)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.border)
        })
        .style(Style::default().bg(bg));
    let inner = card_block.inner(area);
    frame.render_widget(card_block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }

    let (type_icon, type_color) = theme.issue_type_badge(&issue.issue_type);
    let (prio_icon, prio_color) = theme.priority_badge(issue.priority.as_ref());
    let pts = issue
        .story_points
        .map(|p| format!(" [{p:.0}]"))
        .unwrap_or_default();

    let mut card_lines = Vec::new();
    // Line 1: [TypeIcon] KEY-123  [Priority] [pts]
    card_lines.push(Line::from(vec![
        Span::styled(format!("{type_icon} "), Style::default().fg(type_color)),
        Span::styled(
            issue.key.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(prio_icon, Style::default().fg(prio_color)),
        Span::styled(pts, theme.muted_style()),
    ]));

    // Line 2-3: 2-line wrapped summary
    let wrapped = wrap_plain_text(&issue.summary, inner.width as usize);
    let max_sum_lines = inner.height.saturating_sub(2).min(2) as usize;
    for seg in wrapped.into_iter().take(max_sum_lines) {
        card_lines.push(Line::from(Span::styled(
            truncate_width(&seg, inner.width as usize),
            if selected {
                Style::default().fg(theme.selection_fg).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        )));
    }

    // Footer line: @담당자 · 💬N · 📎N
    if inner.height >= 4 {
        let assignee = issue
            .assignee
            .as_ref()
            .map(|u| format!("@{}", u.display_name))
            .unwrap_or_else(|| "@미할당".to_string());
        let meta = truncate_width(
            &format!(
                "{assignee} · 💬{} · 📎{}",
                issue.comment_count, issue.attachment_count
            ),
            inner.width as usize,
        );
        card_lines.push(Line::from(Span::styled(meta, theme.muted_style())));
    }

    frame.render_widget(Paragraph::new(card_lines), inner);
}
