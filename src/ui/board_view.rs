//! Tab 0 (`Ctrl+1`): Minimal-chrome multi-column Native Jira Kanban / Sprint Board.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    models::IssueSummary,
    ui::{
        app::{AppState, KanbanColumn, PaneFocus, UiState},
        doc_view::{truncate_width, wrap_plain_text},
        draw_horizontal_hairline, draw_vertical_hairline, fill_surface,
        hitmap::HitTarget,
        issue_view::draw_issue_inspector_pane,
    },
};

const MIN_COL_WIDTH: u16 = 26;

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

    if main_area.width > 0 && main_area.height > 0 {
        draw_kanban_canvas(frame, main_area, app, ui);
    }

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
    let sidebar_style = theme.surface_style(theme.bg_sidebar);
    fill_surface(frame, area, sidebar_style);
    ui.hitmap.add(area, HitTarget::NavPane);
    if area.width < 4 || area.height < 4 {
        return;
    }

    // 1-line minimal-chrome section header
    let header_area = Rect::new(area.x, area.y, area.width, 1);
    let header_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.bg_sidebar)
            .fg(if focused { theme.accent } else { theme.fg_secondary })
            .add_modifier(Modifier::BOLD)
    };
    let focus_mark = if focused { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&format!("{focus_mark} Boards & Sprints"), area.width as usize),
            header_style,
        )))
        .style(sidebar_style),
        header_area,
    );

    draw_horizontal_hairline(
        frame,
        area.x,
        area.y + 1,
        area.width,
        if focused {
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
    if inner.height < 3 {
        return;
    }

    let split_h = (app.boards.len() as u16 + 2).clamp(3, (inner.height / 2).max(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(split_h), Constraint::Min(2)])
        .split(inner);

    // Boards section
    let mut b_lines = vec![Line::from(Span::styled(
        " AGILE BOARDS",
        theme.muted_style(),
    ))];
    if app.boards.is_empty() {
        b_lines.push(Line::from(Span::styled("  (no boards)", theme.muted_style())));
    } else {
        for (idx, b) in app
            .boards
            .iter()
            .enumerate()
            .take(rows[0].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.board_ui.selected_board == idx;
            let style = if focused && ui.board_ui.nav_section == 0 && is_sel {
                theme.nav_selection()
            } else if is_sel {
                theme.accent_style()
            } else {
                sidebar_style
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let tag = if b.board_type.eq_ignore_ascii_case("scrum") {
                "S"
            } else {
                "K"
            };
            b_lines.push(Line::from(Span::styled(
                truncate_width(&format!("{pfx}[{tag}] {}", b.name), rows[0].width as usize),
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
    frame.render_widget(Paragraph::new(b_lines).style(sidebar_style), rows[0]);

    // Sprints section
    let mut s_lines = vec![Line::from(Span::styled(" SPRINTS", theme.muted_style()))];
    if app.sprints.is_empty() {
        s_lines.push(Line::from(Span::styled(
            "  (all board issues)",
            theme.muted_style(),
        )));
    } else {
        for (idx, sp) in app
            .sprints
            .iter()
            .enumerate()
            .take(rows[1].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.board_ui.selected_sprint == idx;
            let style = if focused && ui.board_ui.nav_section == 1 && is_sel {
                theme.nav_selection()
            } else if is_sel {
                theme.accent_style()
            } else {
                sidebar_style
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let state_tag = match sp.state.as_str() {
                "active" => "●",
                "future" => "○",
                _ => "·",
            };
            s_lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!("{pfx}{state_tag} {}", sp.name),
                    rows[1].width as usize,
                ),
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
    frame.render_widget(Paragraph::new(s_lines).style(sidebar_style), rows[1]);
}

fn draw_kanban_canvas(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Main;
    let board_name = app
        .active_board(ui)
        .map(|b| b.name.as_str())
        .unwrap_or("Kanban Board");

    if area.width < 8 || area.height < 4 {
        return;
    }

    // Minimal-chrome top board bar + hairline
    let header_area = Rect::new(area.x, area.y, area.width, 1);
    let hint = " [h/l] Col · [j/k] Card · [Enter] Detail · [m] Transition · [n] New ";
    let title_str = format!(" {board_name}");
    let avail_hint = (area.width as usize).saturating_sub(title_str.len() + 2);
    let mut hdr_spans = vec![Span::styled(title_str, theme.title(focused))];
    if avail_hint >= 12 {
        hdr_spans.push(Span::styled(
            truncate_width(hint, avail_hint),
            theme.muted_style(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(hdr_spans)), header_area);
    draw_horizontal_hairline(
        frame,
        area.x,
        area.y + 1,
        area.width,
        if focused {
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
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let columns = app.build_kanban_columns(ui);
    if columns.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  No board columns", theme.muted_style())),
            inner,
        );
        return;
    }

    let sel_col = ui
        .board_ui
        .selected_col
        .min(columns.len().saturating_sub(1));
    let visible_count = ((inner.width / MIN_COL_WIDTH) as usize)
        .max(1)
        .min(columns.len());
    let mut col_scroll = ui
        .board_ui
        .col_scroll
        .min(columns.len().saturating_sub(visible_count));
    if sel_col < col_scroll {
        col_scroll = sel_col;
    } else if sel_col >= col_scroll + visible_count {
        col_scroll = sel_col + 1 - visible_count;
    }

    // Interleave 1px vertical hairline separators between Kanban columns
    let mut constraints = Vec::with_capacity(visible_count * 2 - 1);
    for i in 0..visible_count {
        constraints.push(Constraint::Ratio(1, visible_count as u32));
        if i + 1 < visible_count {
            constraints.push(Constraint::Length(1));
        }
    }
    let col_rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(inner);

    for vis_idx in 0..visible_count {
        let col_rect = col_rects[vis_idx * 2];
        let abs_col = col_scroll + vis_idx;
        let Some(col) = columns.get(abs_col) else {
            continue;
        };
        let is_active_col = focused && abs_col == sel_col;
        draw_kanban_column(frame, col_rect, abs_col, col, is_active_col, app, ui);

        if vis_idx + 1 < visible_count {
            let sep_rect = col_rects[vis_idx * 2 + 1];
            let sep_style = ui.theme.subtle_style();
            draw_vertical_hairline(
                frame,
                sep_rect.x,
                sep_rect.y,
                sep_rect.height,
                sep_style,
            );
        }
    }
}

fn draw_kanban_column(
    frame: &mut Frame,
    area: Rect,
    col_idx: usize,
    col: &KanbanColumn,
    is_active_col: bool,
    app: &AppState,
    ui: &mut UiState,
) {
    let theme = &ui.theme;
    if area.width < 6 || area.height < 3 {
        return;
    }

    // Column header: status lozenge + count + subtle underline
    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let hdr_line = Line::from(vec![
        Span::raw(" "),
        theme.status_lozenge_span(&col.title, col.category),
        Span::styled(format!(" ({})", col.issues.len()), theme.muted_style()),
    ]);
    frame.render_widget(Paragraph::new(hdr_line), hdr_rect);
    draw_horizontal_hairline(
        frame,
        area.x,
        area.y + 1,
        area.width,
        if is_active_col {
            theme.accent_style()
        } else {
            theme.subtle_style()
        },
    );

    let body = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if body.height == 0 {
        return;
    }

    if col.issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (no issues)", theme.muted_style())),
            body,
        );
        return;
    }

    let density = app.config.density;
    let (card_h, stride): (u16, u16) = match density {
        DensityMode::Cozy => (4, 5),
        DensityMode::Compact => (2, 3),
    };
    let cards_fit = ((body.height + 1) / stride).max(1) as usize;
    let sel_row = if col_idx == ui.board_ui.selected_col {
        ui.board_ui
            .selected_row
            .min(col.issues.len().saturating_sub(1))
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
        let y = body.y + (vis_idx as u16) * stride;
        if y >= body.bottom() {
            break;
        }
        let h = card_h.min(body.bottom() - y);
        if h < 2 {
            break;
        }
        let card_rect = Rect::new(
            body.x.saturating_add(1),
            y,
            body.width.saturating_sub(2).max(1),
            h,
        );
        let selected = col_idx == ui.board_ui.selected_col && abs_row == sel_row;
        draw_kanban_card(frame, card_rect, issue, selected, density, ui);
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
    density: DensityMode,
    ui: &UiState,
) {
    let theme = &ui.theme;
    if area.width < 4 || area.height < 2 {
        return;
    }

    let card_surface = if theme.no_color {
        if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        }
    } else {
        let bg = if selected {
            theme.bg_selected
        } else {
            theme.bg_card
        };
        Style::default().bg(bg).fg(theme.fg_primary)
    };
    fill_surface(frame, area, card_surface);

    let (type_icon, type_color) = theme.issue_type_badge(&issue.issue_type);
    let (prio_icon, prio_color) = theme.priority_badge(issue.priority.as_ref());
    let pts = issue
        .story_points
        .map(|p| format!(" [{p:.0}]"))
        .unwrap_or_default();

    let bar_style = if theme.no_color {
        if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    } else if selected {
        Style::default()
            .fg(theme.accent)
            .bg(theme.bg_selected)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_subtle).bg(theme.bg_card)
    };
    let bar = Span::styled(if selected { "▌ " } else { "▎ " }, bar_style);
    let content_w = (area.width as usize).saturating_sub(2).max(2);

    let mut card_lines = Vec::new();
    match density {
        DensityMode::Compact => {
            // Line 1: [Type] KEY  Summary
            let prefix_len = issue.key.len() + 6;
            let sum_budget = content_w.saturating_sub(prefix_len);
            card_lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(format!("{type_icon} "), Style::default().fg(type_color)),
                Span::styled(
                    format!("{} ", issue.key),
                    theme.accent_style().patch(card_surface),
                ),
                Span::styled(
                    truncate_width(&issue.summary, sum_budget),
                    if selected {
                        card_surface.add_modifier(Modifier::BOLD)
                    } else {
                        card_surface
                    },
                ),
            ]));
            // Line 2: Priority · @assignee · C/A
            let assignee = issue
                .assignee
                .as_ref()
                .map(|u| format!("@{}", u.display_name))
                .unwrap_or_else(|| "Unassigned".to_string());
            let meta = truncate_width(
                &format!(
                    "{prio_icon}{pts} · {assignee} · {}c {}f",
                    issue.comment_count, issue.attachment_count
                ),
                content_w,
            );
            card_lines.push(Line::from(vec![
                bar,
                Span::styled(meta, theme.muted_style().patch(card_surface)),
            ]));
        }
        DensityMode::Cozy => {
            // Line 1: [Type] KEY  [Priority] [pts]
            card_lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(format!("{type_icon} "), Style::default().fg(type_color)),
                Span::styled(
                    issue.key.clone(),
                    theme.accent_style().patch(card_surface),
                ),
                Span::raw("  "),
                Span::styled(prio_icon, Style::default().fg(prio_color)),
                Span::styled(pts, theme.muted_style().patch(card_surface)),
            ]));

            // Line 2-3: up to 2-line wrapped summary
            let wrapped = wrap_plain_text(&issue.summary, content_w);
            let max_sum_lines = area.height.saturating_sub(2).min(2) as usize;
            for seg in wrapped.into_iter().take(max_sum_lines) {
                card_lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(
                        truncate_width(&seg, content_w),
                        if selected {
                            card_surface.add_modifier(Modifier::BOLD)
                        } else {
                            card_surface
                        },
                    ),
                ]));
            }

            // Footer line: @assignee · N comments · N files
            if area.height >= 4 {
                let assignee = issue
                    .assignee
                    .as_ref()
                    .map(|u| format!("@{}", u.display_name))
                    .unwrap_or_else(|| "Unassigned".to_string());
                let meta = truncate_width(
                    &format!(
                        "{assignee} · {} comments · {} files",
                        issue.comment_count, issue.attachment_count
                    ),
                    content_w,
                );
                card_lines.push(Line::from(vec![
                    bar,
                    Span::styled(meta, theme.muted_style().patch(card_surface)),
                ]));
            }
        }
    }

    frame.render_widget(Paragraph::new(card_lines).style(card_surface), area);
}
