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
        hitmap::{ButtonId, HitTarget},
        issue_view::draw_issue_inspector_pane,
        visible_window,
    },
};
use unicode_width::UnicodeWidthStr;

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

    let multi_site = app.is_multi_site_all_mode();
    let extra_group_rows = if multi_site {
        app.config.enabled_sites().len().max(2) as u16
    } else {
        0
    };
    let split_h = (app.boards.len() as u16 + 2 + extra_group_rows)
        .clamp(3, (inner.height / 2).max(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(split_h), Constraint::Min(2)])
        .split(inner);

    // Register nav section containers BEFORE item rows for hover-scroll & click fallback.
    ui.hitmap.add(rows[0], HitTarget::BoardNavBoards);
    ui.hitmap.add(rows[1], HitTarget::BoardNavSprints);

    // Boards section (grouped by site with collapsible headers in multi-site All Sites mode)
    let board_list_h = rows[0].height.saturating_sub(1) as usize;
    let mut b_lines = Vec::new();
    if app.boards.is_empty() {
        b_lines.push(Line::from(Span::styled(
            " AGILE BOARDS",
            theme.muted_style(),
        )));
        b_lines.push(Line::from(Span::styled("  (no boards)", theme.muted_style())));
        ui.board_ui.board_scroll = 0;
    } else if multi_site {
        enum BoardNavRow {
            SiteHeader {
                site_id: String,
                count: usize,
                collapsed: bool,
            },
            Board(usize),
        }

        let mut site_order: Vec<String> = Vec::new();
        for site in app.config.enabled_sites() {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(&site.id)) {
                site_order.push(site.id);
            }
        }
        for sid in &app.board_site_ids {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(sid)) {
                site_order.push(sid.clone());
            }
        }

        let mut nav_rows: Vec<BoardNavRow> = Vec::new();
        for site_id in site_order {
            let group_indices: Vec<usize> = app
                .boards
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    app.board_site_ids
                        .get(*i)
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();
            if group_indices.is_empty() {
                continue;
            }
            let collapse_key = format!("board:{site_id}");
            let collapsed = ui.collapsed_site_groups.contains(&collapse_key);
            nav_rows.push(BoardNavRow::SiteHeader {
                site_id,
                count: group_indices.len(),
                collapsed,
            });
            if !collapsed {
                for idx in group_indices {
                    nav_rows.push(BoardNavRow::Board(idx));
                }
            }
        }

        let sel_row = nav_rows
            .iter()
            .position(|r| matches!(r, BoardNavRow::Board(i) if *i == ui.board_ui.selected_board));
        let (mut scroll, mut window) = visible_window(
            sel_row.unwrap_or(ui.board_ui.board_scroll),
            ui.board_ui.board_scroll,
            board_list_h,
            nav_rows.len(),
        );
        if let Some(r) = sel_row.filter(|&r| r == scroll && r > 0 && board_list_h > 1) {
            if matches!(nav_rows[r - 1], BoardNavRow::SiteHeader { .. }) {
                scroll = r - 1;
                window = scroll..(scroll + board_list_h).min(nav_rows.len());
            }
        }
        ui.board_ui.board_scroll = scroll;

        let mut title = " AGILE BOARDS".to_string();
        if window.start > 0 || window.end < nav_rows.len() {
            let up = if window.start > 0 { "▲" } else { "" };
            let down = if window.end < nav_rows.len() { "▼" } else { "" };
            title.push_str(&format!(" {up}{down}"));
        }
        b_lines.push(Line::from(Span::styled(
            truncate_width(&title, rows[0].width as usize),
            theme.muted_style(),
        )));

        for (offset, row) in nav_rows[window].iter().enumerate() {
            let y = rows[0].y + 1 + offset as u16;
            match row {
                BoardNavRow::SiteHeader {
                    site_id,
                    count,
                    collapsed,
                } => {
                    let collapse_key = format!("board:{site_id}");
                    let group_idx = ui.site_group_keys.len();
                    ui.site_group_keys.push(collapse_key);
                    let chevron = if *collapsed { "▸" } else { "▾" };
                    let site_label = app.site_display_label(site_id);
                    b_lines.push(Line::from(Span::styled(
                        truncate_width(
                            &format!(" {chevron} {site_label} ({count})"),
                            rows[0].width as usize,
                        ),
                        theme.accent_style().patch(sidebar_style),
                    )));
                    ui.hitmap.add_line(
                        rows[0].x,
                        y,
                        rows[0].width,
                        HitTarget::SiteGroupHeader(group_idx),
                    );
                }
                BoardNavRow::Board(idx) => {
                    let b = &app.boards[*idx];
                    let is_sel = ui.board_ui.selected_board == *idx;
                    let style = if focused && ui.board_ui.nav_section == 0 && is_sel {
                        theme.nav_selection()
                    } else if is_sel {
                        theme.accent_style()
                    } else {
                        sidebar_style
                    };
                    let pfx = if is_sel { "  ▸ " } else { "    " };
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
                        y,
                        rows[0].width,
                        HitTarget::BoardItem(*idx),
                    );
                }
            }
        }
    } else {
        let (scroll, window) = visible_window(
            ui.board_ui.selected_board,
            ui.board_ui.board_scroll,
            board_list_h,
            app.boards.len(),
        );
        ui.board_ui.board_scroll = scroll;

        let mut title = " AGILE BOARDS".to_string();
        if window.start > 0 || window.end < app.boards.len() {
            let up = if window.start > 0 { "▲" } else { "" };
            let down = if window.end < app.boards.len() { "▼" } else { "" };
            title.push_str(&format!(" {up}{down}"));
        }
        b_lines.push(Line::from(Span::styled(
            truncate_width(&title, rows[0].width as usize),
            theme.muted_style(),
        )));

        for (offset, idx) in window.enumerate() {
            let b = &app.boards[idx];
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
                rows[0].y + 1 + offset as u16,
                rows[0].width,
                HitTarget::BoardItem(idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(b_lines).style(sidebar_style), rows[0]);

    // Sprints section
    let sprint_list_h = rows[1].height.saturating_sub(1) as usize;
    let mut s_lines = Vec::new();
    if app.sprints.is_empty() {
        s_lines.push(Line::from(Span::styled(" SPRINTS", theme.muted_style())));
        s_lines.push(Line::from(Span::styled(
            "  (all board issues)",
            theme.muted_style(),
        )));
        ui.board_ui.sprint_scroll = 0;
    } else {
        let (scroll, window) = visible_window(
            ui.board_ui.selected_sprint,
            ui.board_ui.sprint_scroll,
            sprint_list_h,
            app.sprints.len(),
        );
        ui.board_ui.sprint_scroll = scroll;

        let mut title = " SPRINTS".to_string();
        if window.start > 0 || window.end < app.sprints.len() {
            let up = if window.start > 0 { "▲" } else { "" };
            let down = if window.end < app.sprints.len() { "▼" } else { "" };
            title.push_str(&format!(" {up}{down}"));
        }
        s_lines.push(Line::from(Span::styled(
            truncate_width(&title, rows[1].width as usize),
            theme.muted_style(),
        )));

        for (offset, idx) in window.enumerate() {
            let sp = &app.sprints[idx];
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
                rows[1].y + 1 + offset as u16,
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

    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    let columns = app.build_kanban_columns(ui);
    let sel_col = ui
        .board_ui
        .selected_col
        .min(columns.len().saturating_sub(1));
    let visible_count = if columns.is_empty() {
        0
    } else {
        ((inner.width / MIN_COL_WIDTH) as usize)
            .max(1)
            .min(columns.len())
    };
    let mut col_scroll = if visible_count == 0 {
        0
    } else {
        ui.board_ui
            .col_scroll
            .min(columns.len().saturating_sub(visible_count))
    };
    if visible_count > 0 {
        if sel_col < col_scroll {
            col_scroll = sel_col;
        } else if sel_col >= col_scroll + visible_count {
            col_scroll = sel_col + 1 - visible_count;
        }
    }
    ui.board_ui.col_scroll = col_scroll;

    // Minimal-chrome top board bar + hairline
    let header_area = Rect::new(area.x, area.y, area.width, 1);
    let show_col_pager = columns.len() > visible_count && visible_count > 0;
    let pager_parts = if show_col_pager {
        let prev_lbl = " < ";
        let range_lbl = format!(
            "{}-{}/{}",
            col_scroll + 1,
            (col_scroll + visible_count).min(columns.len()),
            columns.len()
        );
        let next_lbl = " > ";
        let total_w = (prev_lbl.width() + range_lbl.width() + next_lbl.width()) as u16;
        if header_area.width > total_w + 6 {
            Some((prev_lbl, range_lbl, next_lbl, total_w))
        } else {
            None
        }
    } else {
        None
    };

    let left_max_x = match &pager_parts {
        Some((_, _, _, pw)) => header_area.right().saturating_sub(*pw),
        None => header_area.right(),
    };
    let title_budget = left_max_x.saturating_sub(header_area.x) as usize;
    let title_str = truncate_width(&format!(" {board_name}"), title_budget);
    let title_w = title_str.width() as u16;
    let mut hdr_spans = vec![Span::styled(title_str, theme.title(focused))];
    let mut cx = header_area.x.saturating_add(title_w);
    let segments: &[(&str, Option<ButtonId>)] = &[
        (" [h/l] Col · [j/k] Card · ", None),
        ("[t] Drawer", Some(ButtonId::ToggleDrawer)),
        (" · ", None),
        ("[m] Transition", Some(ButtonId::Transition)),
        (" · ", None),
        ("[n] New", Some(ButtonId::NewItem)),
        (" ", None),
    ];
    for &(seg, btn) in segments {
        let seg_w = seg.width() as u16;
        if cx.saturating_add(seg_w) > left_max_x {
            let rem = left_max_x.saturating_sub(cx) as usize;
            if rem > 0 {
                let fitted = truncate_width(seg, rem);
                let fw = fitted.width() as u16;
                if let Some(btn_id) = btn {
                    if fw > 0 {
                        ui.hitmap.add_line(cx, header_area.y, fw, HitTarget::Button(btn_id));
                    }
                }
                hdr_spans.push(Span::styled(fitted, theme.muted_style()));
                cx = cx.saturating_add(fw);
            }
            break;
        }
        if let Some(btn_id) = btn {
            ui.hitmap.add_line(cx, header_area.y, seg_w, HitTarget::Button(btn_id));
        }
        hdr_spans.push(Span::styled(seg, theme.muted_style()));
        cx = cx.saturating_add(seg_w);
    }

    if let Some((prev_lbl, range_lbl, next_lbl, _)) = pager_parts {
        let pad = left_max_x.saturating_sub(cx) as usize;
        if pad > 0 {
            hdr_spans.push(Span::raw(" ".repeat(pad)));
            cx = left_max_x;
        }
        let prev_w = prev_lbl.width() as u16;
        let range_w = range_lbl.width() as u16;
        let next_w = next_lbl.width() as u16;
        let can_prev = col_scroll > 0;
        let can_next = col_scroll + visible_count < columns.len();
        ui.hitmap.add_line(
            cx,
            header_area.y,
            prev_w,
            HitTarget::Button(ButtonId::BoardColPrev),
        );
        hdr_spans.push(Span::styled(
            prev_lbl,
            if can_prev {
                theme.accent_style()
            } else {
                theme.muted_style()
            },
        ));
        cx = cx.saturating_add(prev_w);
        hdr_spans.push(Span::styled(range_lbl, theme.muted_style()));
        cx = cx.saturating_add(range_w);
        ui.hitmap.add_line(
            cx,
            header_area.y,
            next_w,
            HitTarget::Button(ButtonId::BoardColNext),
        );
        hdr_spans.push(Span::styled(
            next_lbl,
            if can_next {
                theme.accent_style()
            } else {
                theme.muted_style()
            },
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

    if inner.width == 0 || inner.height < 3 {
        return;
    }

    if columns.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  No board columns", theme.muted_style())),
            inner,
        );
        return;
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
    // Register the entire Kanban column rect BEFORE individual cards.
    ui.hitmap.add(area, HitTarget::KanbanColumn(col_idx));

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
