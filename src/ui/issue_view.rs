//! Tab 1 (`Ctrl+2`): Minimal-chrome Jira Issue Explorer, JQL Navigator, and Native Issue Inspector.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

use bitefoundry_html::layout::{windowed_single_line_input, wrap_styled_spans, WrapPrefix};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::DensityMode,
    models::{IssueDetail, StatusCategory},
    ui::{
        app::{AppState, CachedLineHit, CachedRenderedDoc, PaneFocus, UiState, WorkspaceTab},
        doc_view::{
            blocks_or_markdown, pad_right_width, render_blocks_with_prefix, truncate_width,
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

    let muted = theme.muted_style().patch(bar_surface);
    let accent = theme.accent_style().patch(bar_surface);
    let total_w = input_rect.width as usize;

    let line = if filter_focused {
        let prefix = " Filter / ";
        let prefix_w = prefix.width();
        let close_lbl = "(Esc to close)";
        let close_w = close_lbl.width();
        let gap_w = 2usize;
        let show_hint = total_w >= prefix_w + 6 + gap_w + close_w;
        let reserved_right = if show_hint { gap_w + close_w } else { 0 };
        let viewport_w = total_w.saturating_sub(prefix_w + reserved_right);

        let win = windowed_single_line_input(
            &ui.issue_ui.local_filter.text,
            ui.issue_ui.local_filter.cursor,
            viewport_w,
        );
        let mut spans = vec![Span::styled(prefix, accent)];
        let mut rendered_w = 0usize;
        if win.clipped_left {
            spans.push(Span::styled("‹", muted));
            rendered_w += 1;
        }
        if !win.before_cursor.is_empty() {
            rendered_w += win.before_cursor.width();
            spans.push(Span::styled(win.before_cursor, bar_surface));
        }
        if viewport_w > 0 {
            spans.push(Span::styled("▏", accent));
            rendered_w += 1;
        }
        if !win.after_cursor.is_empty() {
            rendered_w += win.after_cursor.width();
            spans.push(Span::styled(win.after_cursor, bar_surface));
        }
        if win.clipped_right {
            spans.push(Span::styled("›", muted));
            rendered_w += 1;
        }
        if show_hint {
            let pad = total_w.saturating_sub(prefix_w + rendered_w + close_w);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), bar_surface));
            }
            let close_x = input_rect.right().saturating_sub(close_w as u16);
            ui.hitmap.add_line(
                close_x,
                input_rect.y,
                close_w as u16,
                HitTarget::Button(ButtonId::CloseLocalFilter),
            );
            spans.push(Span::styled(close_lbl, muted));
        }
        Line::from(spans)
    } else {
        let prefix = " JQL> ";
        let prefix_w = prefix.width();
        if input_rect.x.saturating_add(prefix_w as u16) <= input_rect.right() {
            ui.hitmap.add_line(
                input_rect.x,
                input_rect.y,
                prefix_w as u16,
                HitTarget::Button(ButtonId::FocusJql),
            );
        }

        let filter_badge = (!ui.issue_ui.local_filter.text.trim().is_empty())
            .then(|| format!("[Filter: {}]", ui.issue_ui.local_filter.text));
        let filter_st = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            bar_surface.fg(theme.warning)
        };

        let mut right_segments: Vec<(String, Style, Option<ButtonId>)> = Vec::new();
        if jql_focused {
            right_segments.push(("[Enter] Run".to_string(), muted, Some(ButtonId::RunJql)));
        } else {
            right_segments.push(("[J] Edit".to_string(), muted, Some(ButtonId::FocusJql)));
            right_segments.push((" · ".to_string(), muted, None));
            right_segments.push(("[Enter] Run".to_string(), muted, Some(ButtonId::RunJql)));
            right_segments.push((" · ".to_string(), muted, None));
            right_segments.push((
                "[/] Filter".to_string(),
                muted,
                Some(ButtonId::FocusLocalFilter),
            ));
        }
        if let Some(badge) = filter_badge {
            right_segments.push(("  ".to_string(), muted, None));
            right_segments.push((badge, filter_st, Some(ButtonId::FocusLocalFilter)));
        }

        let min_viewport = if jql_focused { 10usize } else { 8usize };
        let gap_w = 2usize;
        let full_right_w: usize = right_segments.iter().map(|(s, _, _)| s.width()).sum();
        let active_right: Vec<(String, Style, Option<ButtonId>)> =
            if total_w >= prefix_w + min_viewport + gap_w + full_right_w {
                right_segments
            } else if jql_focused {
                let short = ("[Enter] Run".to_string(), muted, Some(ButtonId::RunJql));
                if total_w >= prefix_w + min_viewport + gap_w + short.0.width() {
                    vec![short]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
        let active_right_w: usize = active_right.iter().map(|(s, _, _)| s.width()).sum();
        let reserved_right = if active_right.is_empty() {
            0
        } else {
            gap_w + active_right_w
        };
        let viewport_w = total_w.saturating_sub(prefix_w + reserved_right);

        let mut spans = vec![Span::styled(prefix, accent)];
        let mut rendered_w = 0usize;
        if jql_focused {
            let win = windowed_single_line_input(
                &ui.issue_ui.jql.text,
                ui.issue_ui.jql.cursor,
                viewport_w,
            );
            if win.clipped_left {
                spans.push(Span::styled("‹", muted));
                rendered_w += 1;
            }
            if !win.before_cursor.is_empty() {
                rendered_w += win.before_cursor.width();
                spans.push(Span::styled(win.before_cursor, bar_surface));
            }
            if viewport_w > 0 {
                spans.push(Span::styled("▏", accent));
                rendered_w += 1;
            }
            if !win.after_cursor.is_empty() {
                rendered_w += win.after_cursor.width();
                spans.push(Span::styled(win.after_cursor, bar_surface));
            }
            if win.clipped_right {
                spans.push(Span::styled("›", muted));
                rendered_w += 1;
            }
        } else {
            let fitted = truncate_width(&ui.issue_ui.jql.text, viewport_w);
            rendered_w += fitted.width();
            spans.push(Span::styled(fitted, bar_surface));
        }

        if !active_right.is_empty() {
            let pad = total_w.saturating_sub(prefix_w + rendered_w + active_right_w);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), bar_surface));
            }
            let mut cx = input_rect.right().saturating_sub(active_right_w as u16);
            for (seg, st, btn) in active_right {
                let seg_w = seg.width() as u16;
                if let Some(btn_id) = btn {
                    if cx < input_rect.right() {
                        let clipped_w = seg_w.min(input_rect.right().saturating_sub(cx));
                        ui.hitmap
                            .add_line(cx, input_rect.y, clipped_w, HitTarget::Button(btn_id));
                    }
                }
                spans.push(Span::styled(seg, st));
                cx = cx.saturating_add(seg_w);
            }
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
        frame.render_widget(
            Paragraph::new(empty)
                .style(pane_surface)
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    };

    let cache_key = issue_render_cache_key(detail, inner.width, compact_drawer, app, ui);
    let cache_hit = ui
        .issue_render_cache
        .as_ref()
        .is_some_and(|c| c.key == cache_key);
    if !cache_hit {
        let (lines, action_row, row_hits) =
            build_issue_inspector_lines_with_hits(detail, inner.width, app, ui, compact_drawer);
        ui.issue_render_cache = Some(CachedRenderedDoc {
            key: cache_key,
            lines: Arc::from(lines),
            action_row,
            hits: Arc::from(row_hits),
        });
    }
    let cached = ui
        .issue_render_cache
        .as_ref()
        .expect("issue render cache populated");
    let total_lines = cached.lines.len();
    let max_scroll = total_lines.saturating_sub(inner.height as usize) as u16;
    let clamped_scroll = scroll.min(max_scroll);
    let action_row_u16 = cached.action_row as u16;
    let scroll_usize = clamped_scroll as usize;
    let vis_end = scroll_usize
        .saturating_add(inner.height as usize)
        .min(total_lines);
    let visible_lines: Vec<Line<'static>> = cached.lines[scroll_usize..vis_end].to_vec();
    let hits = Arc::clone(&cached.hits);

    if ui.active_tab == WorkspaceTab::Board {
        ui.board_ui.detail_max_scroll = max_scroll;
        ui.board_ui.detail_scroll = clamped_scroll;
    } else {
        ui.issue_ui.detail_max_scroll = max_scroll;
        ui.issue_ui.detail_scroll = clamped_scroll;
    }

    // Register clickable action bar buttons whenever the action bar row is visible.
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
    for hit in hits.iter() {
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

    frame.render_widget(Paragraph::new(visible_lines).style(pane_surface), inner);
}

fn issue_render_cache_key(
    detail: &IssueDetail,
    width: u16,
    compact: bool,
    app: &AppState,
    ui: &UiState,
) -> u64 {
    let mut h = DefaultHasher::new();
    width.hash(&mut h);
    compact.hash(&mut h);
    (app.config.density == DensityMode::Compact).hash(&mut h);
    ui.theme.no_color.hash(&mut h);
    format!("{:?}", ui.theme.accent).hash(&mut h);
    ui.inline_images.hash(&mut h);
    ui.show_hrefs.hash(&mut h);
    app.image_cache.len().hash(&mut h);
    app.image_cache.total_bytes().hash(&mut h);
    let s = &detail.summary;
    s.key.hash(&mut h);
    s.project_key.hash(&mut h);
    s.parent_key.hash(&mut h);
    s.summary.hash(&mut h);
    s.status.id.hash(&mut h);
    s.status.name.hash(&mut h);
    (s.status.category as u8).hash(&mut h);
    s.priority.as_ref().map(|p| &p.name).hash(&mut h);
    s.issue_type.name.hash(&mut h);
    s.assignee.as_ref().map(|u| &u.display_name).hash(&mut h);
    s.reporter.as_ref().map(|u| &u.display_name).hash(&mut h);
    s.labels.hash(&mut h);
    s.sprint_name.hash(&mut h);
    s.story_points.map(f64::to_bits).hash(&mut h);
    s.updated_at.hash(&mut h);
    detail.fix_versions.hash(&mut h);
    detail.description_markdown.hash(&mut h);
    detail.description_blocks.len().hash(&mut h);
    for st in &detail.subtasks {
        st.key.hash(&mut h);
        st.summary.hash(&mut h);
        (st.status.category as u8).hash(&mut h);
    }
    for lp in &detail.linked_pages {
        lp.page_id.hash(&mut h);
        lp.title.hash(&mut h);
    }
    for li in &detail.linked_issues {
        li.key.hash(&mut h);
        li.summary.hash(&mut h);
        li.status.name.hash(&mut h);
    }
    for att in &detail.attachments {
        att.id.hash(&mut h);
        att.filename.hash(&mut h);
        att.size.hash(&mut h);
        att.mime_type.hash(&mut h);
    }
    for c in &detail.comments {
        c.id.hash(&mut h);
        c.author.display_name.hash(&mut h);
        c.created_at.hash(&mut h);
        c.body_markdown.hash(&mut h);
        c.blocks.len().hash(&mut h);
    }
    h.finish()
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

type IssueLineHit = CachedLineHit;

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
    lines.extend(wrap_styled_spans(
        &[
            Span::styled(format!("{type_icon} "), Style::default().fg(type_color)),
            Span::styled(
                format!("{} / {}{}", s.project_key, parent_crumb, s.key),
                theme.muted_style(),
            ),
        ],
        w,
        WrapPrefix {
            first: &[Span::raw(" ")],
            cont: &[Span::raw("   ")],
        },
    ));

    // 2. Bold Summary
    lines.extend(wrap_styled_spans(
        &[Span::styled(
            s.summary.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )],
        w,
        WrapPrefix {
            first: &[Span::raw(" ")],
            cont: &[Span::raw(" ")],
        },
    ));

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

    let push_wrapped_meta = |lines: &mut Vec<Line<'static>>,
                             label: &str,
                             label_style: Style,
                             value_spans: Vec<Span<'static>>| {
        let first_pfx = [Span::styled(label.to_string(), label_style)];
        let cont_pfx = [Span::raw("  ")];
        lines.extend(wrap_styled_spans(
            &value_spans,
            w,
            WrapPrefix {
                first: &first_pfx,
                cont: &cont_pfx,
            },
        ));
    };

    let status_span = theme.status_lozenge_span(&s.status.name, s.status.category);
    let prio_span = Span::styled(
        format!("{prio_icon} {prio_name}"),
        Style::default().fg(prio_color),
    );
    let stacked = w < 44;
    let status_prio_w = " Status: ".width()
        + status_span.content.width()
        + "   Priority: ".width()
        + prio_span.content.width();
    if !stacked && status_prio_w <= w {
        lines.push(Line::from(vec![
            Span::styled(" Status: ", theme.muted_style()),
            status_span,
            Span::raw("   "),
            Span::styled("Priority: ", theme.muted_style()),
            prio_span,
        ]));
    } else {
        push_wrapped_meta(&mut lines, " Status: ", theme.muted_style(), vec![status_span]);
        push_wrapped_meta(&mut lines, " Priority: ", theme.muted_style(), vec![prio_span]);
    }

    let col1_w = (w / 2).max(16);
    let col2_w = w.saturating_sub(col1_w);
    let mut push_meta_pair = |l_label: &str,
                              l_val: &str,
                              l_style: Style,
                              r_label: &str,
                              r_val: &str,
                              r_style: Style| {
        let left_full = format!(" {l_label}{l_val}");
        let right_full = format!("{r_label}{r_val}");
        if !stacked && left_full.width() < col1_w && right_full.width() <= col2_w {
            lines.push(Line::from(vec![
                Span::styled(pad_right_width(&left_full, col1_w), l_style),
                Span::styled(right_full, r_style),
            ]));
        } else {
            push_wrapped_meta(
                &mut lines,
                &format!(" {l_label}"),
                theme.muted_style(),
                vec![Span::styled(l_val.to_string(), l_style)],
            );
            push_wrapped_meta(
                &mut lines,
                &format!(" {r_label}"),
                theme.muted_style(),
                vec![Span::styled(r_val.to_string(), r_style)],
            );
        }
    };

    push_meta_pair(
        "Assignee: ",
        assignee,
        Style::default(),
        "Reporter: ",
        reporter,
        theme.muted_style(),
    );
    push_meta_pair(
        "Sprint: ",
        sprint,
        Style::default(),
        "Story Points: ",
        &pts,
        Style::default(),
    );
    push_meta_pair(
        "Labels: ",
        &labels,
        Style::default(),
        "Fix Versions: ",
        &fix_v,
        theme.muted_style(),
    );
    push_wrapped_meta(
        &mut lines,
        " Updated: ",
        theme.muted_style(),
        vec![Span::styled(s.updated_at.clone(), theme.muted_style())],
    );
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
        lines.extend(render_blocks_with_prefix(
            &desc_blocks,
            w as u16,
            theme,
            Some(&app.image_cache),
            ui.inline_images,
            ui.show_hrefs,
            &[Span::raw("  ")],
        ));
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
            let start_idx = lines.len();
            let wrapped = wrap_styled_spans(
                &[
                    Span::styled(format!("{} ", st.key), theme.accent_style()),
                    Span::raw(st.summary.clone()),
                ],
                w,
                WrapPrefix {
                    first: &[Span::styled(format!("  {chk} "), Style::default().fg(chk_col))],
                    cont: &[Span::raw("    ")],
                },
            );
            for offset in 0..wrapped.len() {
                row_hits.push(IssueLineHit {
                    line_idx: start_idx + offset,
                    x_offset: 0,
                    width: None,
                    target: HitTarget::DetailSubtask(idx),
                });
            }
            lines.extend(wrapped);
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
        lines.extend(wrap_styled_spans(
            &[Span::styled(
                "▾ Linked Issues & Confluence Pages ([g] to jump)",
                link_hdr_style,
            )],
            w,
            WrapPrefix {
                first: &[Span::raw(" ")],
                cont: &[Span::raw("   ")],
            },
        ));
        for (idx, lp) in detail.linked_pages.iter().enumerate() {
            let start_idx = lines.len();
            let wrapped = wrap_styled_spans(
                &[
                    Span::styled("[Confluence] ", link_hdr_style),
                    Span::raw(lp.title.clone()),
                ],
                w,
                WrapPrefix {
                    first: &[Span::raw("  ")],
                    cont: &[Span::raw("    ")],
                },
            );
            for offset in 0..wrapped.len() {
                row_hits.push(IssueLineHit {
                    line_idx: start_idx + offset,
                    x_offset: 0,
                    width: None,
                    target: HitTarget::DetailConfluenceLink(idx),
                });
            }
            lines.extend(wrapped);
        }
        for (idx, li) in detail.linked_issues.iter().enumerate() {
            let start_idx = lines.len();
            let wrapped = wrap_styled_spans(
                &[
                    Span::styled(format!("↳ {} ", li.relation_label), theme.muted_style()),
                    Span::styled(format!("{} ", li.key), theme.accent_style()),
                    theme.status_lozenge_span(&li.status.name, li.status.category),
                    Span::raw(format!(" {}", li.summary)),
                ],
                w,
                WrapPrefix {
                    first: &[Span::raw("  ")],
                    cont: &[Span::raw("    ")],
                },
            );
            for offset in 0..wrapped.len() {
                row_hits.push(IssueLineHit {
                    line_idx: start_idx + offset,
                    x_offset: 0,
                    width: None,
                    target: HitTarget::DetailIssueLink(idx),
                });
            }
            lines.extend(wrapped);
        }
        if !is_compact {
            lines.push(Line::from(""));
        }
    }

    // 8. Attachments
    if !detail.attachments.is_empty() {
        lines.extend(wrap_styled_spans(
            &[Span::styled(
                format!(
                    "▾ Attachments ({}) — [d] Download · [v] Preview",
                    detail.attachments.len()
                ),
                theme.accent_style(),
            )],
            w,
            WrapPrefix {
                first: &[Span::raw(" ")],
                cont: &[Span::raw("   ")],
            },
        ));
        let file_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.chip_fg)
        };
        for (idx, att) in detail.attachments.iter().enumerate() {
            let kb = (att.size / 1024).max(1);
            let start_idx = lines.len();
            let wrapped = wrap_styled_spans(
                &[
                    Span::styled(att.filename.clone(), file_style),
                    Span::styled(
                        format!(" ({kb} KB · {})", att.mime_type),
                        theme.muted_style(),
                    ),
                ],
                w,
                WrapPrefix {
                    first: &[Span::raw("  • ")],
                    cont: &[Span::raw("    ")],
                },
            );
            for offset in 0..wrapped.len() {
                row_hits.push(IssueLineHit {
                    line_idx: start_idx + offset,
                    x_offset: 0,
                    width: None,
                    target: HitTarget::Attachment(idx),
                });
            }
            lines.extend(wrapped);
        }
        if !is_compact {
            lines.push(Line::from(""));
        }
    }

    // 9. Comments Thread
    let comment_hdr_prefix = format!(" ▾ Comments ({}) — ", detail.comments.len());
    let comment_btn_lbl = "[c] Add Comment";
    if comment_hdr_prefix.width() + comment_btn_lbl.width() <= w {
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
    } else {
        lines.extend(wrap_styled_spans(
            &[Span::styled(
                format!("▾ Comments ({})", detail.comments.len()),
                theme.accent_style(),
            )],
            w,
            WrapPrefix {
                first: &[Span::raw(" ")],
                cont: &[Span::raw("   ")],
            },
        ));
        let btn_line_idx = lines.len();
        row_hits.push(IssueLineHit {
            line_idx: btn_line_idx,
            x_offset: 3,
            width: Some(comment_btn_lbl.width() as u16),
            target: HitTarget::Button(ButtonId::Comment),
        });
        lines.extend(wrap_styled_spans(
            &[Span::styled(comment_btn_lbl, theme.accent_style())],
            w,
            WrapPrefix {
                first: &[Span::raw("   ")],
                cont: &[Span::raw("   ")],
            },
        ));
    }
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
            lines.extend(render_blocks_with_prefix(
                &c_blocks,
                w as u16,
                theme,
                None,
                false,
                ui.show_hrefs,
                &[Span::raw("    ")],
            ));
            if !is_compact {
                lines.push(Line::from(""));
            }
        }
    }

    (lines, action_row, row_hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona},
        models::{
            AttachmentItem, IssuePriority, IssueStatus, IssueSummary, IssueType, LinkedIssueItem,
            UserSummary,
        },
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn test_app_ui() -> (AppState, UiState) {
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
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[tokio::test]
    async fn narrow_drawer_stacks_metadata_and_preserves_linked_issue_summary_and_attachments() {
        let (app, ui) = test_app_ui();
        let detail = IssueDetail {
            summary: IssueSummary {
                id: "1".into(),
                key: "ENG-42".into(),
                project_key: "ENG".into(),
                summary: "Kanban narrow drawer wrapping invariant".into(),
                status: IssueStatus {
                    id: "3".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
                priority: Some(IssuePriority {
                    id: "2".into(),
                    name: "High".into(),
                }),
                issue_type: IssueType {
                    id: "10001".into(),
                    name: "Story".into(),
                    subtask: false,
                    epic: false,
                },
                assignee: Some(UserSummary {
                    id: "u1".into(),
                    display_name: "Kim Minseo".into(),
                    ..Default::default()
                }),
                reporter: Some(UserSummary {
                    id: "u2".into(),
                    display_name: "Lee Jihun".into(),
                    ..Default::default()
                }),
                labels: vec![
                    "platform-core".into(),
                    "unicode-wrapping".into(),
                    "track-d".into(),
                ],
                story_points: Some(5.0),
                sprint_name: Some("Sprint 42 — Core Layout Hardening".into()),
                updated_at: "2026-11-25T10:00:00Z".into(),
                ..Default::default()
            },
            fix_versions: vec!["v2.4.0-release-candidate".into(), "v2.5.0".into()],
            linked_issues: vec![LinkedIssueItem {
                link_type: "Blocks".into(),
                direction: "outward".into(),
                relation_label: "blocks".into(),
                key: "ENG-99".into(),
                summary: "핵심 연결 이슈 요약이 32컬럼 드로어에서도 완전히 보존되어야 합니다".into(),
                status: IssueStatus {
                    id: "2".into(),
                    name: "To Do".into(),
                    category: StatusCategory::Todo,
                },
            }],
            attachments: vec![AttachmentItem {
                id: "a1".into(),
                filename: "very-long-architecture-specification-document-v3.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 4096,
                ..Default::default()
            }],
            ..Default::default()
        };

        let lines = build_issue_inspector_lines(&detail, 32, &app, &ui, true);
        let rendered_rows: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();

        // 1-col stacked metadata when w = 32 (< 44): Status and Priority on distinct lines.
        let status_row = rendered_rows
            .iter()
            .position(|r| r.contains("Status:"))
            .expect("Status row present");
        let prio_row = rendered_rows
            .iter()
            .position(|r| r.contains("Priority:"))
            .expect("Priority row present");
        assert_ne!(
            status_row, prio_row,
            "Status and Priority must stack at w = 32:\n{}",
            rendered_rows.join("\n")
        );

        let compact: String = rendered_rows
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for expected in [
            "INPROGRESS",
            "High",
            "KimMinseo",
            "LeeJihun",
            "Sprint42—CoreLayoutHardening",
            "5.0",
            "platform-core,unicode-wrapping,track-d",
            "v2.4.0-release-candidate,v2.5.0",
            "ENG-99",
            "핵심연결이슈요약이32컬럼드로어에서도완전히보존되어야합니다",
            "very-long-architecture-specification-document-v3.pdf",
        ] {
            assert!(
                compact.contains(expected),
                "expected `{expected}` preserved at w=32:\n{}",
                rendered_rows.join("\n")
            );
        }

        // Width invariant for metadata, linked issues, and attachments (skipping divider & action bar).
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.width() <= 32,
                "issue inspector line {i} exceeded 32 cols (width={}): {:?}",
                line.width(),
                line
            );
        }
    }

    #[tokio::test]
    async fn jql_and_local_filter_bars_window_100_char_queries_with_cursor_visible() {
        let (app, mut ui) = test_app_ui();
        let long_jql = format!("{}ORDER BY updated DESC END_TAIL_999", "project = ENG AND ".repeat(5));
        assert!(long_jql.len() >= 100);

        ui.focus = PaneFocus::JqlBar;
        ui.issue_ui.local_filter_active = false;
        ui.issue_ui.jql.set_text(long_jql);

        let mut terminal = Terminal::new(TestBackend::new(64, 4)).unwrap();
        terminal
            .draw(|f| draw_jql_bar(f, Rect::new(0, 0, 64, 2), &app, &mut ui))
            .unwrap();
        let buf = terminal.backend().buffer();
        let jql_row: String = (0..64).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            jql_row.contains("END_TAIL_999"),
            "expected JQL tail in row:\n{jql_row}"
        );
        assert!(jql_row.contains('▏'), "expected cursor `▏` in JQL row:\n{jql_row}");
        assert!(
            jql_row.contains('‹'),
            "expected left clip indicator `‹` in JQL row:\n{jql_row}"
        );
        assert!(
            jql_row.contains("[Enter] Run"),
            "expected right-aligned hint in JQL row:\n{jql_row}"
        );

        // Local Filter input mode with 100+ char query
        let long_filter = format!("{}FILTER_TAIL_777", "filter_term_".repeat(8));
        assert!(long_filter.len() >= 100);
        ui.issue_ui.local_filter_active = true;
        ui.issue_ui.local_filter.set_text(long_filter);

        terminal
            .draw(|f| draw_jql_bar(f, Rect::new(0, 0, 64, 2), &app, &mut ui))
            .unwrap();
        let buf = terminal.backend().buffer();
        let filter_row: String = (0..64).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            filter_row.contains("FILTER_TAIL_777"),
            "expected local filter tail in row:\n{filter_row}"
        );
        assert!(
            filter_row.contains('▏'),
            "expected cursor `▏` in local filter row:\n{filter_row}"
        );
    }
}
