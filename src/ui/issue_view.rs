//! Tab 1 (`Ctrl+2`): Minimal-chrome Jira Issue Explorer, JQL Navigator, and Native Issue Inspector.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    models::{DocBlock, InlineNode, InlineStyle, IssueDetail, StatusCategory},
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::{
            pad_right_width, render_blocks_with_options, truncate_width, wrap_plain_text,
        },
        draw_horizontal_hairline, fill_surface,
        hitmap::{ButtonId, HitMap, HitTarget},
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
        Line::from(vec![
            Span::styled(" Filter / ", theme.accent_style().patch(bar_surface)),
            Span::styled(ui.issue_ui.local_filter.text.clone(), bar_surface),
            Span::styled("▏", theme.accent_style().patch(bar_surface)),
            Span::styled("   (Esc to close)", theme.muted_style().patch(bar_surface)),
        ])
    } else {
        let prefix = " JQL> ";
        let mut spans = vec![
            Span::styled(prefix, theme.accent_style().patch(bar_surface)),
            Span::styled(ui.issue_ui.jql.text.clone(), bar_surface),
        ];
        let mut cx = input_rect
            .x
            .saturating_add(unicode_width::UnicodeWidthStr::width(prefix) as u16);
        let prefix_w = unicode_width::UnicodeWidthStr::width(prefix) as u16;
        if input_rect.x.saturating_add(prefix_w) <= input_rect.right() {
            ui.hitmap.add_line(
                input_rect.x,
                input_rect.y,
                prefix_w,
                HitTarget::Button(ButtonId::FocusJql),
            );
        }
        cx = cx.saturating_add(unicode_width::UnicodeWidthStr::width(ui.issue_ui.jql.text.as_str()) as u16);
        if jql_focused {
            spans.push(Span::styled("▏", theme.accent_style().patch(bar_surface)));
        } else {
            let sep = "   ";
            let edit_lbl = "[J] Edit";
            let tail = " · [Enter] Run · [/] Filter";
            let edit_x = cx.saturating_add(sep.len() as u16);
            let edit_w = edit_lbl.len() as u16;
            if edit_x.saturating_add(edit_w) <= input_rect.right() {
                ui.hitmap.add_line(
                    edit_x,
                    input_rect.y,
                    edit_w,
                    HitTarget::Button(ButtonId::FocusJql),
                );
            }
            spans.push(Span::styled(
                format!("{sep}{edit_lbl}{tail}"),
                theme.muted_style().patch(bar_surface),
            ));
        }
        if !ui.issue_ui.local_filter.text.trim().is_empty() {
            let filter_st = if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                bar_surface.fg(theme.warning)
            };
            spans.push(Span::styled(
                format!("   [Filter: {}]", ui.issue_ui.local_filter.text),
                filter_st,
            ));
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

    // Saved filters
    let mut f_lines = vec![Line::from(Span::styled(
        " SAVED FILTERS",
        theme.muted_style(),
    ))];
    for (idx, f) in app
        .saved_filters
        .iter()
        .enumerate()
        .take(rows[0].height.saturating_sub(1) as usize)
    {
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
            rows[0].y + 1 + idx as u16,
            rows[0].width,
            HitTarget::FilterItem(idx),
        );
    }
    frame.render_widget(Paragraph::new(f_lines).style(sidebar_style), rows[0]);

    // Issue list (density-aware: Cozy = 2 lines per issue, Compact = 1 line per issue)
    // In multi-site All Sites mode, renders collapsible site group headers.
    let issues_with_sites = app.filtered_issues_with_sites(ui);
    let list_h = rows[1].height.saturating_sub(1) as usize;
    let mut i_lines = vec![Line::from(Span::styled(
        format!(" ISSUES ({})", issues_with_sites.len()),
        theme.muted_style(),
    ))];
    if issues_with_sites.is_empty() {
        i_lines.push(Line::from(Span::styled(
            "  No issues to display",
            theme.muted_style(),
        )));
    } else if list_h > 0 && app.is_multi_site_all_mode() {
        let row_lines = match app.config.density {
            DensityMode::Cozy => 2usize,
            DensityMode::Compact => 1usize,
        };
        let sel = ui
            .issue_ui
            .selected_issue
            .min(issues_with_sites.len().saturating_sub(1));
        let w = rows[1].width as usize;
        let max_y = rows[1].bottom();
        let mut cursor_y = rows[1].y + 1;

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

        for site_id in site_order {
            let group_indices: Vec<usize> = issues_with_sites
                .iter()
                .enumerate()
                .filter(|(_, (sid, _))| sid.eq_ignore_ascii_case(&site_id))
                .map(|(i, _)| i)
                .collect();
            if group_indices.is_empty() || cursor_y >= max_y {
                continue;
            }
            let collapse_key = format!("issues:{site_id}");
            let collapsed = ui.collapsed_site_groups.contains(&collapse_key);
            let group_idx = ui.site_group_keys.len();
            ui.site_group_keys.push(collapse_key);
            let chevron = if collapsed { "▸" } else { "▾" };
            let site_label = app.site_display_label(&site_id);
            i_lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!(" {chevron} {site_label} ({})", group_indices.len()),
                    w,
                ),
                theme.accent_style().patch(sidebar_style),
            )));
            ui.hitmap.add_line(
                rows[1].x,
                cursor_y,
                rows[1].width,
                HitTarget::SiteGroupHeader(group_idx),
            );
            cursor_y += 1;

            if collapsed {
                continue;
            }
            for abs_idx in group_indices {
                if cursor_y >= max_y {
                    break;
                }
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
                    let summary_line = truncate_width(&format!("      {}", issue.summary), w);
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
        let start = if sel >= visible_items {
            sel - visible_items + 1
        } else {
            0
        };
        let mut cursor_y = rows[1].y + 1;
        for (vis_idx, issue) in issues.iter().skip(start).take(visible_items).enumerate() {
            let abs_idx = start + vis_idx;
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
                let summary_line = truncate_width(
                    &format!("    {}", issue.summary),
                    w,
                );
                i_lines.push(Line::from(Span::styled(summary_line, row_style)));
                ui.hitmap
                    .add_line(rows[1].x, cursor_y, rows[1].width, HitTarget::IssueRow(abs_idx));
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
                ui.hitmap
                    .add_line(rows[1].x, cursor_y, rows[1].width, HitTarget::IssueRow(abs_idx));
                cursor_y += 1;
            }
        }
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
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&format!("{focus_mark}{title}"), area.width as usize),
            theme.title(focused),
        )))
        .style(pane_surface),
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
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(detail) = &app.active_issue else {
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

    let lines = build_issue_inspector_lines(detail, inner.width, app, ui, compact_drawer);

    // Register clickable action bar buttons when visible and unscrolled.
    // Breadcrumb = 0, Summary lines = sum_count, Action bar = 1 + sum_count.
    let w = (inner.width as usize).max(20);
    let sum_lines = wrap_plain_text(&detail.summary.summary, w.saturating_sub(2)).len() as u16;
    let action_row = 1 + sum_lines;
    if scroll == 0 && inner.height > action_row {
        register_issue_action_buttons(
            inner.x,
            inner.y + action_row,
            inner.width,
            compact_drawer,
            &mut ui.hitmap,
        );
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(pane_surface)
            .scroll((scroll, 0)),
        inner,
    );
}

fn register_issue_action_buttons(
    x: u16,
    row: u16,
    width: u16,
    compact: bool,
    hitmap: &mut HitMap,
) {
    let buttons: &[(&str, ButtonId)] = if compact {
        &[
            ("[m]Move", ButtonId::Transition),
            ("[i]Me", ButtonId::AssignToMe),
            ("[a]Assign", ButtonId::Assignee),
            ("[c]Comment", ButtonId::Comment),
            ("[e]Edit", ButtonId::Edit),
            ("[H: HTML]", ButtonId::HtmlPreview),
            ("[g]Links", ButtonId::CrossLinks),
            ("[v]Img", ButtonId::ImagePreview),
            ("[o]Open", ButtonId::OpenBrowser),
        ]
    } else {
        &[
            ("[m]Transition", ButtonId::Transition),
            ("[i]Assign Me", ButtonId::AssignToMe),
            ("[a]Assignee", ButtonId::Assignee),
            ("[c]Comment", ButtonId::Comment),
            ("[e]Edit", ButtonId::Edit),
            ("[n]New", ButtonId::NewItem),
            ("[H: HTML]", ButtonId::HtmlPreview),
            ("[u]Attach", ButtonId::UploadAttachment),
            ("[v]Image", ButtonId::ImagePreview),
            ("[g]Links", ButtonId::CrossLinks),
            ("[o]Open", ButtonId::OpenBrowser),
        ]
    };
    let mut cx = x + 1;
    for &(label, btn) in buttons {
        let w = unicode_width::UnicodeWidthStr::width(label) as u16;
        if cx + w > x + width {
            break;
        }
        hitmap.add_line(cx, row, w, HitTarget::Button(btn));
        cx += w + 1;
    }
}

pub fn build_issue_inspector_lines(
    detail: &IssueDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
    compact: bool,
) -> Vec<Line<'static>> {
    let theme = &ui.theme;
    let is_compact = compact || app.config.density == DensityMode::Compact;
    let w = (width as usize).max(20);
    let mut lines = Vec::new();
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
    let action_bar = if compact {
        " [m]Move [i]Me [a]Assign [c]Comment [e]Edit [H: HTML] [g]Links [v]Img [o]Open"
    } else {
        " [m]Transition [i]Assign Me [a]Assignee [c]Comment [e]Edit [n]New [H: HTML] [u]Attach [v]Image [g]Links [o]Open"
    };
    let action_style = if theme.no_color {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(theme.accent_soft)
    };
    lines.push(Line::from(Span::styled(
        truncate_width(action_bar, w),
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
    let desc_blocks = if !detail.description_blocks.is_empty() {
        detail.description_blocks.clone()
    } else if !detail.description_markdown.trim().is_empty() {
        vec![DocBlock::Paragraph {
            spans: vec![InlineNode::Text {
                text: detail.description_markdown.clone(),
                style: InlineStyle::default(),
            }],
        }]
    } else {
        Vec::new()
    };
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
        for st in &detail.subtasks {
            let done = st.status.category == StatusCategory::Done;
            let chk = if done { "☑" } else { "☐" };
            let chk_col = if theme.no_color {
                ratatui::style::Color::Reset
            } else if done {
                theme.success
            } else {
                theme.fg_muted
            };
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
        for lp in &detail.linked_pages {
            lines.push(Line::from(vec![
                Span::styled("  [Confluence] ", link_hdr_style),
                Span::raw(truncate_width(&lp.title, w.saturating_sub(18))),
            ]));
        }
        for li in &detail.linked_issues {
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
        for att in &detail.attachments {
            let kb = (att.size / 1024).max(1);
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
    lines.push(Line::from(Span::styled(
        format!(" ▾ Comments ({}) — [c] Add Comment", detail.comments.len()),
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
            let c_blocks = if !c.blocks.is_empty() {
                c.blocks.clone()
            } else {
                vec![DocBlock::Paragraph {
                    spans: vec![InlineNode::Text {
                        text: c.body_markdown.clone(),
                        style: InlineStyle::default(),
                    }],
                }]
            };
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

    lines
}
