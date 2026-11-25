//! Tab 1 (`Ctrl+2`): Minimal-chrome Jira Issue Explorer, JQL Navigator, and Native Issue Inspector.

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
    models::{IssueDetail, StatusCategory},
    ui::{
        app::{AppState, PaneFocus, UiState, WorkspaceTab},
        doc_view::{
            blocks_or_markdown, pad_right_width, render_blocks_with_options, truncate_width,
            wrap_plain_text,
        },
        draw_horizontal_hairline, fill_surface,
        hitmap::{ButtonId, HitMap, HitTarget},
        visible_window,
    },
};

pub fn draw(
    frame: &mut Frame,
    nav_area: Option<Rect>,
    main_area: Rect,
    app: &AppState,
    ui: &mut UiState,
) {
    if let Some(nav_rect) = nav_area {
        draw_issue_nav(frame, nav_rect, app, ui);
    }

    if main_area.width == 0 || main_area.height < 4 {
        return;
    }

    let main_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(main_area);

    draw_jql_bar(frame, main_rows[0], app, ui);

    draw_issue_inspector_pane(
        frame,
        main_rows[1],
        app,
        ui,
        ui.focus == PaneFocus::Main || ui.focus == PaneFocus::Detail,
        ui.issue_ui.detail_scroll,
        false,
    );
}

fn draw_jql_bar(frame: &mut Frame, area: Rect, _app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let jql_focused = ui.focus == PaneFocus::JqlBar;
    let filter_focused = ui.issue_ui.local_filter_active;
    let active = jql_focused || filter_focused;

    let bar_surface = theme.surface_style(if active {
        theme.bg_elevated
    } else {
        theme.bg_header
    });
    let input_rect = Rect::new(area.x, area.y, area.width, 1);
    fill_surface(frame, input_rect, bar_surface);
    ui.hitmap.add(area, HitTarget::JqlBar);

    let line = if filter_focused {
        let prefix = " Filter / ";
        let text = ui.issue_ui.local_filter.text.clone();
        let cursor = "▏";
        let sep = "   ";
        let close_lbl = "(Esc to close)";
        let close_x = input_rect
            .x
            .saturating_add(prefix.width() as u16)
            .saturating_add(text.width() as u16)
            .saturating_add(cursor.width() as u16)
            .saturating_add(sep.width() as u16);
        let close_w = close_lbl.width() as u16;
        if close_x < input_rect.right() {
            let clipped_w = close_w.min(input_rect.right().saturating_sub(close_x));
            ui.hitmap.add_line(
                close_x,
                input_rect.y,
                clipped_w,
                HitTarget::Button(ButtonId::CloseLocalFilter),
            );
        }
        Line::from(vec![
            Span::styled(prefix, theme.accent_style().patch(bar_surface)),
            Span::styled(text, bar_surface),
            Span::styled(cursor, theme.accent_style().patch(bar_surface)),
            Span::styled(sep, theme.muted_style().patch(bar_surface)),
            Span::styled(close_lbl, theme.muted_style().patch(bar_surface)),
        ])
    } else {
        let prefix = " JQL> ";
        let mut spans = vec![
            Span::styled(prefix, theme.accent_style().patch(bar_surface)),
            Span::styled(ui.issue_ui.jql.text.clone(), bar_surface),
        ];
        let prefix_w = prefix.width() as u16;
        if input_rect.x.saturating_add(prefix_w) <= input_rect.right() {
            ui.hitmap.add_line(
                input_rect.x,
                input_rect.y,
                prefix_w,
                HitTarget::Button(ButtonId::FocusJql),
            );
        }
        let mut cx = input_rect
            .x
            .saturating_add(prefix_w)
            .saturating_add(ui.issue_ui.jql.text.as_str().width() as u16);
        let muted = theme.muted_style().patch(bar_surface);
        if jql_focused {
            let cursor = "▏";
            spans.push(Span::styled(cursor, theme.accent_style().patch(bar_surface)));
            cx = cx.saturating_add(cursor.width() as u16);
            let sep = "   ";
            let run_lbl = "[Enter] Run";
            let run_x = cx.saturating_add(sep.width() as u16);
            let run_w = run_lbl.width() as u16;
            if run_x < input_rect.right() {
                let clipped_w = run_w.min(input_rect.right().saturating_sub(run_x));
                ui.hitmap.add_line(
                    run_x,
                    input_rect.y,
                    clipped_w,
                    HitTarget::Button(ButtonId::RunJql),
                );
            }
            spans.push(Span::styled(sep, muted));
            spans.push(Span::styled(run_lbl, muted));
            cx = run_x.saturating_add(run_w);
        } else {
            let segments: &[(&str, Option<ButtonId>)] = &[
                ("   ", None),
                ("[J] Edit", Some(ButtonId::FocusJql)),
                (" · ", None),
                ("[Enter] Run", Some(ButtonId::RunJql)),
                (" · ", None),
                ("[/] Filter", Some(ButtonId::FocusLocalFilter)),
            ];
            for &(seg, btn) in segments {
                let seg_w = seg.width() as u16;
                if let Some(btn_id) = btn {
                    if cx < input_rect.right() {
                        let clipped_w = seg_w.min(input_rect.right().saturating_sub(cx));
                        ui.hitmap
                            .add_line(cx, input_rect.y, clipped_w, HitTarget::Button(btn_id));
                    }
                }
                spans.push(Span::styled(seg, muted));
                cx = cx.saturating_add(seg_w);
            }
        }
        if !ui.issue_ui.local_filter.text.trim().is_empty() {
            let filter_st = if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                bar_surface.fg(theme.warning)
            };
            let sep = "   ";
            let badge = format!("[Filter: {}]", ui.issue_ui.local_filter.text);
            let badge_x = cx.saturating_add(sep.width() as u16);
            let badge_w = badge.width() as u16;
            if badge_x < input_rect.right() {
                let clipped_w = badge_w.min(input_rect.right().saturating_sub(badge_x));
                ui.hitmap.add_line(
                    badge_x,
                    input_rect.y,
                    clipped_w,
                    HitTarget::Button(ButtonId::FocusLocalFilter),
                );
            }
            spans.push(Span::styled(sep, muted));
            spans.push(Span::styled(badge, filter_st));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line).style(bar_surface), input_rect);

    if area.height >= 2 {
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
    }
}

fn draw_issue_nav(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Nav;
    let sidebar_style = theme.surface_style(theme.bg_sidebar);
    fill_surface(frame, area, sidebar_style);
    ui.hitmap.add(area, HitTarget::NavPane);
    if area.width < 4 || area.height < 4 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let focus_mark = if focused { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&format!("{focus_mark} Filters & Issues"), area.width as usize),
            if focused {
                theme.accent_style().patch(sidebar_style)
            } else {
                theme.muted_style().patch(sidebar_style).add_modifier(Modifier::BOLD)
            },
        )))
        .style(sidebar_style),
        hdr_rect,
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
    if inner.height < 4 {
        return;
    }

    let filter_h = (app.saved_filters.len() as u16 + 1).clamp(3, 7);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(filter_h), Constraint::Min(2)])
        .split(inner);

    // Register nav section containers BEFORE item rows for hover-scroll & fallback clicks.
    ui.hitmap.add(rows[0], HitTarget::IssueNavFilters);
    ui.hitmap.add(rows[1], HitTarget::IssueNavIssues);

    // Saved filters (viewport scrolled)
    let filter_list_h = rows[0].height.saturating_sub(1) as usize;
    let (f_scroll, f_window) = visible_window(
        ui.issue_ui.selected_filter,
        ui.issue_ui.filter_scroll,
        filter_list_h,
        app.saved_filters.len(),
    );
    ui.issue_ui.filter_scroll = f_scroll;
    let mut filter_title = " SAVED FILTERS".to_string();
    if f_window.start > 0 || f_window.end < app.saved_filters.len() {
        let up = if f_window.start > 0 { "▲" } else { "" };
        let down = if f_window.end < app.saved_filters.len() {
            "▼"
        } else {
            ""
        };
        filter_title.push_str(&format!(" {up}{down}"));
    }
    let mut f_lines = vec![Line::from(Span::styled(
        truncate_width(&filter_title, rows[0].width as usize),
        theme.muted_style(),
    ))];
    for (offset, idx) in f_window.enumerate() {
        let f = &app.saved_filters[idx];
        let is_sel = ui.issue_ui.selected_filter == idx;
        let is_active_focus = focused && ui.issue_ui.nav_section == 0 && is_sel;
        let style = if is_active_focus {
            theme.nav_selection()
        } else if is_sel {
            theme.accent_style()
        } else {
            sidebar_style
        };
        let pfx = if is_sel { "▸ " } else { "  " };
        let star = if f.builtin { "" } else { "★ " };
        f_lines.push(Line::from(Span::styled(
            truncate_width(&format!("{pfx}{star}{}", f.name), rows[0].width as usize),
            style,
        )));
        ui.hitmap.add_line(
            rows[0].x,
            rows[0].y + 1 + offset as u16,
            rows[0].width,
            HitTarget::FilterItem(idx),
        );
    }
    frame.render_widget(Paragraph::new(f_lines).style(sidebar_style), rows[0]);

    // Issue list (density-aware: Cozy = 2 lines per issue, Compact = 1 line per issue)
    // In multi-site All Sites mode, renders collapsible site group headers with viewport scroll.
    let issues_with_sites = app.filtered_issues_with_sites(ui);
    let list_h = rows[1].height.saturating_sub(1) as usize;
    let mut i_lines = Vec::new();
    if issues_with_sites.is_empty() {
        ui.issue_ui.issue_scroll = 0;
        i_lines.push(Line::from(Span::styled(
            " ISSUES (0)",
            theme.muted_style(),
        )));
        i_lines.push(Line::from(Span::styled(
            "  No issues to display",
            theme.muted_style(),
        )));
    } else if list_h > 0 && app.is_multi_site_all_mode() {
        enum IssueNavRow {
            SiteHeader {
                site_id: String,
                count: usize,
                collapsed: bool,
            },
            Issue(usize),
        }

        let row_lines = match app.config.density {
            DensityMode::Cozy => 2usize,
            DensityMode::Compact => 1usize,
        };
        let sel = ui
            .issue_ui
            .selected_issue
            .min(issues_with_sites.len().saturating_sub(1));
        let w = rows[1].width as usize;

        let mut site_order: Vec<String> = Vec::new();
        for site in app.config.enabled_sites() {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(&site.id)) {
                site_order.push(site.id);
            }
        }
        for (sid, _) in &issues_with_sites {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(sid)) {
                site_order.push(sid.clone());
            }
        }

        let mut nav_rows: Vec<IssueNavRow> = Vec::new();
        for site_id in site_order {
            let group_indices: Vec<usize> = issues_with_sites
                .iter()
                .enumerate()
                .filter(|(_, (sid, _))| sid.eq_ignore_ascii_case(&site_id))
                .map(|(i, _)| i)
                .collect();
            if group_indices.is_empty() {
                continue;
            }
            let collapse_key = format!("issues:{site_id}");
            let collapsed = ui.collapsed_site_groups.contains(&collapse_key);
            nav_rows.push(IssueNavRow::SiteHeader {
                site_id,
                count: group_indices.len(),
                collapsed,
            });
            if !collapsed {
                for abs_idx in group_indices {
                    nav_rows.push(IssueNavRow::Issue(abs_idx));
                }
            }
        }

        let row_height = |r: &IssueNavRow| -> usize {
            match r {
                IssueNavRow::SiteHeader { .. } => 1,
                IssueNavRow::Issue(_) => row_lines,
            }
        };
        let sel_row = nav_rows
            .iter()
            .position(|r| matches!(r, IssueNavRow::Issue(i) if *i == sel));

        let mut scroll = ui
            .issue_ui
            .issue_scroll
            .min(nav_rows.len().saturating_sub(1));
        if let Some(sr) = sel_row {
            if sr < scroll {
                scroll = sr;
            } else {
                while scroll < sr {
                    let used: usize = nav_rows[scroll..=sr].iter().map(row_height).sum();
                    if used <= list_h {
                        break;
                    }
                    scroll += 1;
                }
            }
            if sr == scroll
                && scroll > 0
                && matches!(nav_rows[scroll - 1], IssueNavRow::SiteHeader { .. })
                && row_lines < list_h
            {
                scroll -= 1;
            }
        }
        while scroll > 0 {
            let used: usize = nav_rows[(scroll - 1)..].iter().map(row_height).sum();
            if used <= list_h {
                scroll -= 1;
            } else {
                break;
            }
        }
        ui.issue_ui.issue_scroll = scroll;

        let total_from_scroll: usize = nav_rows[scroll..].iter().map(row_height).sum();
        let mut issues_title = format!(" ISSUES ({})", issues_with_sites.len());
        if scroll > 0 || total_from_scroll > list_h {
            let up = if scroll > 0 { "▲" } else { "" };
            let down = if total_from_scroll > list_h { "▼" } else { "" };
            issues_title.push_str(&format!(" {up}{down}"));
        }
        i_lines.push(Line::from(Span::styled(
            truncate_width(&issues_title, w),
            theme.muted_style(),
        )));

        let max_y = rows[1].bottom();
        let mut cursor_y = rows[1].y + 1;
        for row in &nav_rows[scroll..] {
            if cursor_y >= max_y {
                break;
            }
            match row {
                IssueNavRow::SiteHeader {
                    site_id,
                    count,
                    collapsed,
                } => {
                    let collapse_key = format!("issues:{site_id}");
                    let group_idx = ui.site_group_keys.len();
                    ui.site_group_keys.push(collapse_key);
                    let chevron = if *collapsed { "▸" } else { "▾" };
                    let site_label = app.site_display_label(site_id);
                    i_lines.push(Line::from(Span::styled(
                        truncate_width(&format!(" {chevron} {site_label} ({count})"), w),
                        theme.accent_style().patch(sidebar_style),
                    )));
                    ui.hitmap.add_line(
                        rows[1].x,
                        cursor_y,
                        rows[1].width,
                        HitTarget::SiteGroupHeader(group_idx),
                    );
                    cursor_y += 1;
                }
                IssueNavRow::Issue(abs_idx) => {
                    let abs_idx = *abs_idx;
                    let issue = &issues_with_sites[abs_idx].1;
                    let is_sel = abs_idx == sel;
                    let (icon, icon_color) = theme.issue_type_badge(&issue.issue_type);
                    let row_style = if is_sel && focused && ui.issue_ui.nav_section == 1 {
                        theme.nav_selection()
                    } else if is_sel {
                        theme.selection().add_modifier(Modifier::BOLD)
                    } else {
                        sidebar_style
                    };
                    if row_lines == 2 && cursor_y + 1 < max_y {
                        let status_txt = truncate_width(
                            &issue.status.name,
                            w.saturating_sub(issue.key.len() + 8),
                        );
                        i_lines.push(Line::from(vec![
                            Span::styled(if is_sel { "  ▸ " } else { "    " }, row_style),
                            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                            Span::styled(
                                format!("{} ", issue.key),
                                theme.accent_style().patch(row_style),
                            ),
                            Span::styled(status_txt, theme.muted_style().patch(row_style)),
                        ]));
                        let summary_line =
                            truncate_width(&format!("      {}", issue.summary), w);
                        i_lines.push(Line::from(Span::styled(summary_line, row_style)));
                        ui.hitmap.add_line(
                            rows[1].x,
                            cursor_y,
                            rows[1].width,
                            HitTarget::IssueRow(abs_idx),
                        );
                        ui.hitmap.add_line(
                            rows[1].x,
                            cursor_y + 1,
                            rows[1].width,
                            HitTarget::IssueRow(abs_idx),
                        );
                        cursor_y += 2;
                    } else {
                        let title_budget = w.saturating_sub(issue.key.len() + 8);
                        let summary = truncate_width(&issue.summary, title_budget);
                        i_lines.push(Line::from(vec![
                            Span::styled(if is_sel { "  ▸ " } else { "    " }, row_style),
                            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                            Span::styled(
                                format!("{} ", issue.key),
                                theme.accent_style().patch(row_style),
                            ),
                            Span::styled(summary, row_style),
                        ]));
                        ui.hitmap.add_line(
                            rows[1].x,
                            cursor_y,
                            rows[1].width,
                            HitTarget::IssueRow(abs_idx),
                        );
                        cursor_y += 1;
                    }
                }
            }
        }
    } else if list_h > 0 {
        let issues: Vec<_> = issues_with_sites.into_iter().map(|(_, i)| i).collect();
        let row_lines = match app.config.density {
            DensityMode::Cozy => 2usize,
            DensityMode::Compact => 1usize,
        };
        let visible_items = (list_h / row_lines).max(1);
        let sel = ui
            .issue_ui
            .selected_issue
            .min(issues.len().saturating_sub(1));
        let (scroll, window) =
            visible_window(sel, ui.issue_ui.issue_scroll, visible_items, issues.len());
        ui.issue_ui.issue_scroll = scroll;

        let mut issues_title = format!(" ISSUES ({})", issues.len());
        if window.start > 0 || window.end < issues.len() {
            let up = if window.start > 0 { "▲" } else { "" };
            let down = if window.end < issues.len() { "▼" } else { "" };
            issues_title.push_str(&format!(" {up}{down}"));
        }
        i_lines.push(Line::from(Span::styled(
            truncate_width(&issues_title, rows[1].width as usize),
            theme.muted_style(),
        )));

        let mut cursor_y = rows[1].y + 1;
        for abs_idx in window {
            let issue = &issues[abs_idx];
            let is_sel = abs_idx == sel;
            let (icon, icon_color) = theme.issue_type_badge(&issue.issue_type);
            let row_style = if is_sel && focused && ui.issue_ui.nav_section == 1 {
                theme.nav_selection()
            } else if is_sel {
                theme.selection().add_modifier(Modifier::BOLD)
            } else {
                sidebar_style
            };
            let w = rows[1].width as usize;

            if row_lines == 2 && cursor_y + 1 < rows[1].bottom() {
                let status_txt = truncate_width(
                    &issue.status.name,
                    w.saturating_sub(issue.key.len() + 6),
                );
                i_lines.push(Line::from(vec![
                    Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                    Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                    Span::styled(
                        format!("{} ", issue.key),
                        theme.accent_style().patch(row_style),
                    ),
                    Span::styled(status_txt, theme.muted_style().patch(row_style)),
                ]));
                let summary_line = truncate_width(&format!("    {}", issue.summary), w);
                i_lines.push(Line::from(Span::styled(summary_line, row_style)));
                ui.hitmap.add_line(
                    rows[1].x,
                    cursor_y,
                    rows[1].width,
                    HitTarget::IssueRow(abs_idx),
                );
                ui.hitmap.add_line(
                    rows[1].x,
                    cursor_y + 1,
                    rows[1].width,
                    HitTarget::IssueRow(abs_idx),
                );
                cursor_y += 2;
            } else {
                let title_budget = w.saturating_sub(issue.key.len() + 6);
                let summary = truncate_width(&issue.summary, title_budget);
                i_lines.push(Line::from(vec![
                    Span::styled(if is_sel { "▸ " } else { "  " }, row_style),
                    Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                    Span::styled(
                        format!("{} ", issue.key),
                        theme.accent_style().patch(row_style),
                    ),
                    Span::styled(summary, row_style),
                ]));
                ui.hitmap.add_line(
                    rows[1].x,
                    cursor_y,
                    rows[1].width,
                    HitTarget::IssueRow(abs_idx),
                );
                cursor_y += 1;
            }
        }
    } else {
        i_lines.push(Line::from(Span::styled(
            format!(" ISSUES ({})", issues_with_sites.len()),
            theme.muted_style(),
        )));
    }
    frame.render_widget(Paragraph::new(i_lines).style(sidebar_style), rows[1]);
}

/// Shared Native Jira Issue Inspector renderer (used in Tab 1 Main & Tab 0 Right Drawer).
pub fn draw_issue_inspector_pane(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    focused: bool,
    scroll: u16,
    compact_drawer: bool,
) {
    let theme = &ui.theme;
    let pane_surface = if compact_drawer {
        theme.surface_style(theme.bg_sidebar)
    } else {
        theme.surface_style(theme.bg_canvas)
    };
    fill_surface(frame, area, pane_surface);
    ui.hitmap.add(area, HitTarget::DetailPane);

    if area.width < 6 || area.height < 3 {
        return;
    }

    let title = match &app.active_issue {
        Some(d) => format!(" {} — Issue Detail", d.summary.key),
        None => " Issue Detail".to_string(),
    };
    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let focus_mark = if focused { "▸" } else { " " };
    if compact_drawer && area.width >= 10 {
        let close_pill = "[✕]";
        let close_w = close_pill.width() as u16;
        let title_budget = (area.width as usize).saturating_sub(close_w as usize + 1);
        let fitted_title = truncate_width(&format!("{focus_mark}{title}"), title_budget);
        let pad = (area.width as usize)
            .saturating_sub(fitted_title.width() + close_w as usize);
        let close_x = hdr_rect.right().saturating_sub(close_w);
        ui.hitmap.add_line(
            close_x,
            hdr_rect.y,
            close_w,
            HitTarget::Button(ButtonId::ToggleDrawer),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(fitted_title, theme.title(focused)),
                Span::raw(" ".repeat(pad)),
                Span::styled(close_pill, theme.muted_style()),
            ]))
            .style(pane_surface),
            hdr_rect,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(&format!("{focus_mark}{title}"), area.width as usize),
                theme.title(focused),
            )))
            .style(pane_surface),
            hdr_rect,
        );
    }
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
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(detail) = &app.active_issue else {
        if ui.active_tab == WorkspaceTab::Board {
            ui.board_ui.detail_max_scroll = 0;
            ui.board_ui.detail_scroll = 0;
        } else {
            ui.issue_ui.detail_max_scroll = 0;
            ui.issue_ui.detail_scroll = 0;
        }
        let empty = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No issue selected. Pick one from the list or press [n] to create a new issue.",
                theme.muted_style(),
            )),
        ];
        frame.render_widget(Paragraph::new(empty).style(pane_surface), inner);
        return;
    };

    let (lines, action_row, row_hits) =
        build_issue_inspector_lines_with_hits(detail, inner.width, app, ui, compact_drawer);

    let max_scroll = lines.len().saturating_sub(inner.height as usize) as u16;
    let clamped_scroll = scroll.min(max_scroll);
    if ui.active_tab == WorkspaceTab::Board {
        ui.board_ui.detail_max_scroll = max_scroll;
        ui.board_ui.detail_scroll = clamped_scroll;
    } else {
        ui.issue_ui.detail_max_scroll = max_scroll;
        ui.issue_ui.detail_scroll = clamped_scroll;
    }

    // Register clickable action bar buttons whenever the action bar row is visible.
    let action_row_u16 = action_row as u16;
    if clamped_scroll <= action_row_u16 {
        let vis_row = action_row_u16 - clamped_scroll;
        if vis_row < inner.height {
            register_issue_action_buttons(
                inner.x,
                inner.y + vis_row,
                inner.width,
                compact_drawer,
                &mut ui.hitmap,
            );
        }
    }

    // Register clickable subtasks, linked issues/pages, attachments, and Add Comment prompt.
    let scroll_usize = clamped_scroll as usize;
    for hit in row_hits {
        if hit.line_idx >= scroll_usize {
            let vis_row = hit.line_idx - scroll_usize;
            if vis_row < inner.height as usize {
                let y = inner.y + vis_row as u16;
                let x = inner.x.saturating_add(hit.x_offset);
                if x < inner.right() {
                    let max_w = inner.right().saturating_sub(x);
                    let w = hit.width.map_or(max_w, |w| w.min(max_w));
                    ui.hitmap.add_line(x, y, w, hit.target);
                }
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(pane_surface)
            .scroll((clamped_scroll, 0)),
        inner,
    );
}

const COMPACT_ISSUE_ACTION_BUTTONS: &[(&str, ButtonId)] = &[
    ("[m]Move", ButtonId::Transition),
    ("[i]Me", ButtonId::AssignToMe),
    ("[a]Assign", ButtonId::Assignee),
    ("[c]Comment", ButtonId::Comment),
    ("[e]Edit", ButtonId::Edit),
    ("[H: HTML]", ButtonId::HtmlPreview),
    ("[g]Links", ButtonId::CrossLinks),
    ("[v]Img", ButtonId::ImagePreview),
    ("[o]Open", ButtonId::OpenBrowser),
];

const FULL_ISSUE_ACTION_BUTTONS: &[(&str, ButtonId)] = &[
    ("[m]Transition", ButtonId::Transition),
    ("[i]Assign Me", ButtonId::AssignToMe),
    ("[a]Assignee", ButtonId::Assignee),
    ("[c]Comment", ButtonId::Comment),
    ("[e]Edit", ButtonId::Edit),
    ("[n]New", ButtonId::NewItem),
    ("[H: HTML]", ButtonId::HtmlPreview),
    ("[Ctrl+F]Attach", ButtonId::UploadAttachment),
    ("[v]Image", ButtonId::ImagePreview),
    ("[g]Links", ButtonId::CrossLinks),
    ("[o]Open", ButtonId::OpenBrowser),
];

fn issue_action_buttons(compact: bool) -> &'static [(&'static str, ButtonId)] {
    if compact {
        COMPACT_ISSUE_ACTION_BUTTONS
    } else {
        FULL_ISSUE_ACTION_BUTTONS
    }
}

fn register_issue_action_buttons(
    x: u16,
    row: u16,
    width: u16,
    compact: bool,
    hitmap: &mut HitMap,
) {
    let buttons = issue_action_buttons(compact);
    let mut cx = x + 1;
    let max_x = x.saturating_add(width);
    for &(label, btn) in buttons {
        let w = label.width() as u16;
        if cx.saturating_add(w) > max_x {
            break;
        }
        hitmap.add_line(cx, row, w, HitTarget::Button(btn));
        cx = cx.saturating_add(w + 1);
    }
}

struct IssueLineHit {
    line_idx: usize,
    x_offset: u16,
    width: Option<u16>,
    target: HitTarget,
}

pub fn build_issue_inspector_lines(
    detail: &IssueDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
    compact: bool,
) -> Vec<Line<'static>> {
    build_issue_inspector_lines_with_hits(detail, width, app, ui, compact).0
}

fn build_issue_inspector_lines_with_hits(
    detail: &IssueDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
    compact: bool,
) -> (Vec<Line<'static>>, usize, Vec<IssueLineHit>) {
    let theme = &ui.theme;
    let is_compact = compact || app.config.density == DensityMode::Compact;
    let w = (width as usize).max(20);
    let mut lines = Vec::new();
    let mut row_hits = Vec::new();
    let s = &detail.summary;

    // 1. Breadcrumb
    let parent_crumb = s
        .parent_key
        .as_deref()
        .map(|p| format!("{p} / "))
        .unwrap_or_default();
    let (type_icon, type_color) = theme.issue_type_badge(&s.issue_type);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{type_icon} "), Style::default().fg(type_color)),
        Span::styled(
            format!("{} / {}{}", s.project_key, parent_crumb, s.key),
            theme.muted_style(),
        ),
    ]));

    // 2. Bold Summary
    for seg in wrap_plain_text(&s.summary, w.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(
            format!(" {seg}"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }

    // 3. Action hint bar
    let action_row = lines.len();
    let action_bar = format!(
        " {}",
        issue_action_buttons(compact)
            .iter()
            .map(|(lbl, _)| *lbl)
            .collect::<Vec<_>>()
            .join(" ")
    );
    let action_style = if theme.no_color {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(theme.accent_soft)
    };
    lines.push(Line::from(Span::styled(
        truncate_width(&action_bar, w),
        action_style,
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(120)),
        theme.subtle_style(),
    )));

    // 4. 2-Column Metadata
    let (prio_icon, prio_color) = theme.priority_badge(s.priority.as_ref());
    let prio_name = s
        .priority
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or("Medium");
    let assignee = s
        .assignee
        .as_ref()
        .map(|u| u.display_name.as_str())
        .unwrap_or("Unassigned");
    let reporter = s
        .reporter
        .as_ref()
        .map(|u| u.display_name.as_str())
        .unwrap_or("-");
    let labels = if s.labels.is_empty() {
        "-".to_string()
    } else {
        s.labels.join(", ")
    };
    let sprint = s.sprint_name.as_deref().unwrap_or("-");
    let fix_v = if detail.fix_versions.is_empty() {
        "-".to_string()
    } else {
        detail.fix_versions.join(", ")
    };
    let pts = s
        .story_points
        .map(|p| format!("{p:.1}"))
        .unwrap_or_else(|| "-".to_string());

    let col1_w = (w / 2).max(16);
    lines.push(Line::from(vec![
        Span::styled(" Status: ", theme.muted_style()),
        theme.status_lozenge_span(&s.status.name, s.status.category),
        Span::raw("   "),
        Span::styled("Priority: ", theme.muted_style()),
        Span::styled(
            format!("{prio_icon} {prio_name}"),
            Style::default().fg(prio_color),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" Assignee: {assignee}"), col1_w),
            col1_w,
        )),
        Span::styled(
            truncate_width(&format!("Reporter: {reporter}"), w.saturating_sub(col1_w)),
            theme.muted_style(),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" Sprint: {sprint}"), col1_w),
            col1_w,
        )),
        Span::raw(truncate_width(
            &format!("Story Points: {pts}"),
            w.saturating_sub(col1_w),
        )),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" Labels: {labels}"), col1_w),
            col1_w,
        )),
        Span::styled(
            truncate_width(&format!("Fix Versions: {fix_v}"), w.saturating_sub(col1_w)),
            theme.muted_style(),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" Updated: {}", s.updated_at), w),
        theme.muted_style(),
    )));
    if !is_compact {
        lines.push(Line::from(""));
    }

    // 5. Description
    lines.push(Line::from(Span::styled(
        " ▾ Description",
        theme.accent_style(),
    )));
    let desc_blocks = blocks_or_markdown(
        &detail.description_blocks,
        (!detail.description_markdown.trim().is_empty())
            .then_some(detail.description_markdown.as_str()),
    );
    if desc_blocks.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no description)",
            theme.muted_style(),
        )));
    } else {
        for mut l in render_blocks_with_options(
            &desc_blocks,
            w.saturating_sub(2) as u16,
            theme,
            Some(&app.image_cache),
            ui.inline_images,
            ui.show_hrefs,
        ) {
            l.spans.insert(0, Span::raw("  "));
            lines.push(l);
        }
    }
    if !is_compact {
        lines.push(Line::from(""));
    }

    // 6. Subtasks
    if !detail.subtasks.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" ▾ Subtasks ({})", detail.subtasks.len()),
            theme.accent_style(),
        )));
        for (idx, st) in detail.subtasks.iter().enumerate() {
            let done = st.status.category == StatusCategory::Done;
            let chk = if done { "☑" } else { "☐" };
            let chk_col = if theme.no_color {
                ratatui::style::Color::Reset
            } else if done {
                theme.success
            } else {
                theme.fg_muted
            };
            row_hits.push(IssueLineHit {
                line_idx: lines.len(),
                x_offset: 0,
                width: None,
                target: HitTarget::DetailSubtask(idx),
            });
            lines.push(Line::from(vec![
                Span::styled(format!("  {chk} "), Style::default().fg(chk_col)),
                Span::styled(format!("{} ", st.key), theme.accent_style()),
                Span::raw(truncate_width(&st.summary, w.saturating_sub(18))),
            ]));
        }
        if !is_compact {
            lines.push(Line::from(""));
        }
    }

    // 7. Linked Issues & Linked Confluence Pages
    if !detail.linked_issues.is_empty() || !detail.linked_pages.is_empty() {
        let link_hdr_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.confluence_teal)
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(Span::styled(
            " ▾ Linked Issues & Confluence Pages ([g] to jump)",
            link_hdr_style,
        )));
        for (idx, lp) in detail.linked_pages.iter().enumerate() {
            row_hits.push(IssueLineHit {
                line_idx: lines.len(),
                x_offset: 0,
                width: None,
                target: HitTarget::DetailConfluenceLink(idx),
            });
            lines.push(Line::from(vec![
                Span::styled("  [Confluence] ", link_hdr_style),
                Span::raw(truncate_width(&lp.title, w.saturating_sub(18))),
            ]));
        }
        for (idx, li) in detail.linked_issues.iter().enumerate() {
            row_hits.push(IssueLineHit {
                line_idx: lines.len(),
                x_offset: 0,
                width: None,
                target: HitTarget::DetailIssueLink(idx),
            });
            lines.push(Line::from(vec![
                Span::styled(format!("  ↳ {} ", li.relation_label), theme.muted_style()),
                Span::styled(format!("{} ", li.key), theme.accent_style()),
                theme.status_lozenge_span(&li.status.name, li.status.category),
                Span::raw(format!(
                    " {}",
                    truncate_width(&li.summary, w.saturating_sub(36))
                )),
            ]));
        }
        if !is_compact {
            lines.push(Line::from(""));
        }
    }

    // 8. Attachments
    if !detail.attachments.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                " ▾ Attachments ({}) — [d] Download · [v] Preview",
                detail.attachments.len()
            ),
            theme.accent_style(),
        )));
        let file_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.chip_fg)
        };
        for (idx, att) in detail.attachments.iter().enumerate() {
            let kb = (att.size / 1024).max(1);
            row_hits.push(IssueLineHit {
                line_idx: lines.len(),
                x_offset: 0,
                width: None,
                target: HitTarget::Attachment(idx),
            });
            lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(att.filename.clone(), file_style),
                Span::styled(
                    format!(" ({kb} KB · {})", att.mime_type),
                    theme.muted_style(),
                ),
            ]));
        }
        if !is_compact {
            lines.push(Line::from(""));
        }
    }

    // 9. Comments Thread
    let comment_hdr_prefix = format!(" ▾ Comments ({}) — ", detail.comments.len());
    let comment_btn_lbl = "[c] Add Comment";
    row_hits.push(IssueLineHit {
        line_idx: lines.len(),
        x_offset: comment_hdr_prefix.width() as u16,
        width: Some(comment_btn_lbl.width() as u16),
        target: HitTarget::Button(ButtonId::Comment),
    });
    lines.push(Line::from(Span::styled(
        format!("{comment_hdr_prefix}{comment_btn_lbl}"),
        theme.accent_style(),
    )));
    if detail.comments.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no comments)",
            theme.muted_style(),
        )));
    } else {
        for c in &detail.comments {
            let author_style = if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.author(&c.author.display_name))
                    .add_modifier(Modifier::BOLD)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", c.author.display_name), author_style),
                Span::styled(c.created_at.clone(), theme.muted_style()),
            ]));
            let c_blocks = blocks_or_markdown(&c.blocks, Some(&c.body_markdown));
            for mut l in render_blocks_with_options(
                &c_blocks,
                w.saturating_sub(4) as u16,
                theme,
                None,
                false,
                ui.show_hrefs,
            ) {
                l.spans.insert(0, Span::raw("    "));
                lines.push(l);
            }
            if !is_compact {
                lines.push(Line::from(""));
            }
        }
    }

    (lines, action_row, row_hits)
}
