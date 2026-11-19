//! Tab 3 (`Ctrl+4`): Minimal-chrome Confluence Space Selector, Page Tree, Wiki Reader & TOC Drawer.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    models::{DocBlock, InlineNode, InlineStyle, PageDetail},
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::{render_blocks_with_options, truncate_width},
        draw_horizontal_hairline, fill_surface,
        hitmap::{ButtonId, HitMap, HitTarget},
    },
};

pub fn draw(
    frame: &mut Frame,
    nav_area: Option<Rect>,
    main_area: Rect,
    drawer_area: Option<Rect>,
    app: &AppState,
    ui: &mut UiState,
) {
    if let Some(nav_rect) = nav_area {
        draw_space_and_tree(frame, nav_rect, app, ui);
    }

    if main_area.width > 0 && main_area.height > 0 {
        draw_wiki_reader(frame, main_area, app, ui);
    }

    if let Some(toc_rect) = drawer_area {
        draw_toc_drawer(frame, toc_rect, app, ui);
    }
}

fn draw_space_and_tree(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
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
            truncate_width(&format!("{focus_mark} Spaces & Pages"), area.width as usize),
            theme.title(focused),
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

    let multi_site = app.is_multi_site_all_mode();
    let extra_group_rows = if multi_site {
        app.config.enabled_sites().len().max(2) as u16
    } else {
        0
    };
    let space_h = (app.spaces.len() as u16 + 1 + extra_group_rows)
        .clamp(3, (inner.height / 2).max(7).min(12));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(space_h), Constraint::Min(2)])
        .split(inner);

    // Space Selector (grouped by site with collapsible headers in multi-site All Sites mode)
    let mut s_lines = vec![Line::from(Span::styled(" SPACES", theme.muted_style()))];
    if app.spaces.is_empty() {
        s_lines.push(Line::from(Span::styled("  (no spaces)", theme.muted_style())));
    } else if multi_site {
        let mut cursor_y = rows[0].y + 1;
        let max_y = rows[0].bottom();
        let mut site_order: Vec<String> = Vec::new();
        for site in app.config.enabled_sites() {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(&site.id)) {
                site_order.push(site.id);
            }
        }
        for sid in &app.space_site_ids {
            if !site_order.iter().any(|s| s.eq_ignore_ascii_case(sid)) {
                site_order.push(sid.clone());
            }
        }
        for site_id in site_order {
            let group_indices: Vec<usize> = app
                .spaces
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    app.space_site_ids
                        .get(*i)
                        .map(|s| s.eq_ignore_ascii_case(&site_id))
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();
            if group_indices.is_empty() || cursor_y >= max_y {
                continue;
            }
            let collapse_key = format!("wiki:{site_id}");
            let collapsed = ui.collapsed_site_groups.contains(&collapse_key);
            let group_idx = ui.site_group_keys.len();
            ui.site_group_keys.push(collapse_key);
            let chevron = if collapsed { "▸" } else { "▾" };
            let site_label = app.site_display_label(&site_id);
            s_lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!(" {chevron} {site_label} ({})", group_indices.len()),
                    rows[0].width as usize,
                ),
                theme.accent_style().patch(sidebar_style),
            )));
            ui.hitmap.add_line(
                rows[0].x,
                cursor_y,
                rows[0].width,
                HitTarget::SiteGroupHeader(group_idx),
            );
            cursor_y += 1;

            if collapsed {
                continue;
            }
            for idx in group_indices {
                if cursor_y >= max_y {
                    break;
                }
                let sp = &app.spaces[idx];
                let is_sel = ui.confluence_ui.selected_space == idx;
                let style = if focused && ui.confluence_ui.nav_section == 0 && is_sel {
                    theme.nav_selection()
                } else if is_sel {
                    if theme.no_color {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(theme.confluence_teal)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    sidebar_style
                };
                let pfx = if is_sel { "  ▸ " } else { "    " };
                let pin = if sp.pinned { "★ " } else { "" };
                s_lines.push(Line::from(Span::styled(
                    truncate_width(
                        &format!("{pfx}{pin}{} ({})", sp.name, sp.key),
                        rows[0].width as usize,
                    ),
                    style,
                )));
                ui.hitmap.add_line(
                    rows[0].x,
                    cursor_y,
                    rows[0].width,
                    HitTarget::SpaceItem(idx),
                );
                cursor_y += 1;
            }
        }
    } else {
        for (idx, sp) in app
            .spaces
            .iter()
            .enumerate()
            .take(rows[0].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.confluence_ui.selected_space == idx;
            let style = if focused && ui.confluence_ui.nav_section == 0 && is_sel {
                theme.nav_selection()
            } else if is_sel {
                if theme.no_color {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.confluence_teal)
                        .add_modifier(Modifier::BOLD)
                }
            } else {
                sidebar_style
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let pin = if sp.pinned { "★ " } else { "" };
            s_lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!("{pfx}{pin}{} ({})", sp.name, sp.key),
                    rows[0].width as usize,
                ),
                style,
            )));
            ui.hitmap.add_line(
                rows[0].x,
                rows[0].y + 1 + idx as u16,
                rows[0].width,
                HitTarget::SpaceItem(idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(s_lines).style(sidebar_style), rows[0]);

    // Expandable Hierarchical Page Tree
    let tree = app.current_confluence_tree(ui);
    let tree_h = rows[1].height.saturating_sub(1) as usize;
    let mut t_lines = vec![Line::from(Span::styled(
        format!(" PAGE TREE ({}) [Space]", tree.len()),
        theme.muted_style(),
    ))];
    if tree.is_empty() {
        t_lines.push(Line::from(Span::styled(
            "  (no pages — [n] new page)",
            theme.muted_style(),
        )));
    } else if tree_h > 0 {
        let sel = ui
            .confluence_ui
            .selected_tree_row
            .min(tree.len().saturating_sub(1));
        let start = if sel >= tree_h { sel - tree_h + 1 } else { 0 };
        let indent_step = match app.config.density {
            DensityMode::Cozy => "  ",
            DensityMode::Compact => " ",
        };
        for (vis_idx, row) in tree.iter().skip(start).take(tree_h).enumerate() {
            let abs_idx = start + vis_idx;
            let is_sel = abs_idx == sel;
            let style = if focused && ui.confluence_ui.nav_section == 1 && is_sel {
                theme.nav_selection()
            } else if is_sel {
                theme.selection().add_modifier(Modifier::BOLD)
            } else {
                sidebar_style
            };
            let indent = indent_step.repeat(row.depth);
            let branch = if row.has_children {
                if row.expanded {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "· "
            };
            let text = truncate_width(
                &format!(" {indent}{branch}{}", row.page.title),
                rows[1].width as usize,
            );
            t_lines.push(Line::from(Span::styled(text, style)));
            ui.hitmap.add_line(
                rows[1].x,
                rows[1].y + 1 + vis_idx as u16,
                rows[1].width,
                HitTarget::TreeRow(abs_idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(t_lines).style(sidebar_style), rows[1]);
}

fn draw_wiki_reader(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Main;
    ui.hitmap.add(area, HitTarget::DetailPane);

    if area.width < 6 || area.height < 3 {
        return;
    }

    let title = match &app.active_page {
        Some(p) => format!(" {} — Wiki Reader", p.summary.title),
        None => " Confluence Wiki Reader".to_string(),
    };
    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let focus_mark = if focused { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&format!("{focus_mark}{title}"), area.width as usize),
            theme.title(focused),
        ))),
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

    let Some(page) = &app.active_page else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Select a Confluence page from the tree or press [n] to create a new page.",
                    theme.muted_style(),
                )),
            ]),
            inner,
        );
        return;
    };

    let lines = build_wiki_reader_lines(page, inner.width, app, ui);
    if ui.confluence_ui.reader_scroll == 0 && inner.height > 3 {
        register_wiki_action_buttons(inner.x, inner.y + 3, inner.width, &mut ui.hitmap);
    }
    frame.render_widget(
        Paragraph::new(lines).scroll((ui.confluence_ui.reader_scroll, 0)),
        inner,
    );
}

fn register_wiki_action_buttons(x: u16, row: u16, width: u16, hitmap: &mut HitMap) {
    let buttons = [
        ("[e]Edit", ButtonId::Edit),
        ("[n]Child Page", ButtonId::NewItem),
        ("[c]Comment", ButtonId::Comment),
        ("[t]TOC", ButtonId::ToggleToc),
        ("[u]Upload", ButtonId::UploadAttachment),
        ("[d]Download", ButtonId::DownloadAttachment),
        ("[v]Image", ButtonId::ImagePreview),
        ("[g]Links", ButtonId::CrossLinks),
        ("[o]Open", ButtonId::OpenBrowser),
    ];
    let mut cx = x + 1;
    for (label, btn) in buttons {
        let w = unicode_width::UnicodeWidthStr::width(label) as u16;
        if cx + w > x + width {
            break;
        }
        hitmap.add_line(cx, row, w, HitTarget::Button(btn));
        cx += w + 1;
    }
}

fn build_wiki_reader_lines(
    page: &PageDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
) -> Vec<Line<'static>> {
    let theme = &ui.theme;
    let is_compact = app.config.density == DensityMode::Compact;
    let w = (width as usize).max(20);
    let mut lines = Vec::new();
    let s = &page.summary;

    let teal_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.confluence_teal)
    };
    let teal_bold = teal_style.add_modifier(Modifier::BOLD);

    // 1. Breadcrumb: SPACE > Ancestor > Title
    let mut crumb_parts = vec![s.space_key.clone()];
    for anc in &page.ancestors {
        crumb_parts.push(anc.title.clone());
    }
    crumb_parts.push(s.title.clone());
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" {}", crumb_parts.join(" > ")), w),
        teal_style,
    )));

    // 2. Title
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" {}", s.title), w),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    // 3. Metadata: v{N} · Author · Updated · #labels
    let author = s.author_name.as_deref().unwrap_or("Unknown");
    let labels = if page.labels.is_empty() {
        String::new()
    } else {
        format!(
            " · {}",
            page.labels
                .iter()
                .map(|l| format!("#{l}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    lines.push(Line::from(Span::styled(
        truncate_width(
            &format!(" v{} · {author} · {}{labels}", s.version_number, s.updated_at),
            w,
        ),
        theme.muted_style(),
    )));

    // 4. Action bar
    let action_style = if theme.no_color {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(theme.accent)
    };
    lines.push(Line::from(Span::styled(
        truncate_width(
            " [e]Edit [n]Child Page [c]Comment [t]TOC [u]Upload [d]Download [v]Image [g]Links [o]Open",
            w,
        ),
        action_style,
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(120)),
        theme.subtle_style(),
    )));

    // 5. Wiki body
    let body_blocks = if !page.blocks.is_empty() {
        page.blocks.clone()
    } else if !page.body_markdown.trim().is_empty() {
        vec![DocBlock::Paragraph {
            spans: vec![InlineNode::Text {
                text: page.body_markdown.clone(),
                style: InlineStyle::default(),
            }],
        }]
    } else {
        Vec::new()
    };
    if body_blocks.is_empty() {
        lines.push(Line::from(Span::styled(
            " (empty page body)",
            theme.muted_style(),
        )));
    } else {
        for mut l in render_blocks_with_options(
            &body_blocks,
            w.saturating_sub(2) as u16,
            theme,
            Some(&app.image_cache),
            ui.inline_images,
            ui.show_hrefs,
        ) {
            l.spans.insert(0, Span::raw(" "));
            lines.push(l);
        }
    }
    if !is_compact {
        lines.push(Line::from(""));
    }

    // 6. Attachments
    if !page.attachments.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                " ▾ Attachments ({}) — [d] Download · [v] Preview",
                page.attachments.len()
            ),
            teal_bold,
        )));
        let file_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.chip_fg)
        };
        for att in &page.attachments {
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

    // 7. Footer comments
    lines.push(Line::from(Span::styled(
        format!(
            " ▾ Page Comments ({}) — [c] Add Comment",
            page.comments.len()
        ),
        teal_bold,
    )));
    if page.comments.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no comments)",
            theme.muted_style(),
        )));
    } else {
        for c in &page.comments {
            let author_style = if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.author(&c.author_name))
                    .add_modifier(Modifier::BOLD)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", c.author_name), author_style),
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

fn draw_toc_drawer(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Detail;
    let sidebar_style = theme.surface_style(theme.bg_sidebar);
    fill_surface(frame, area, sidebar_style);
    if area.width < 4 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let focus_mark = if focused { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(
                &format!("{focus_mark} Table of Contents [t]"),
                area.width as usize,
            ),
            theme.title(focused),
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
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(page) = &app.active_page else {
        return;
    };
    let toc: Vec<_> = page.toc.iter().filter(|t| t.level <= 4).collect();
    if toc.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" (no headings)", theme.muted_style()))
                .style(sidebar_style),
            inner,
        );
        return;
    }

    let mut lines = Vec::new();
    for (idx, entry) in toc.iter().enumerate().take(inner.height as usize) {
        let is_sel = ui.confluence_ui.selected_toc == idx;
        let style = if focused && is_sel {
            theme.nav_selection()
        } else if is_sel {
            theme.accent_style()
        } else {
            sidebar_style
        };
        let indent = "  ".repeat(entry.level.saturating_sub(1) as usize);
        lines.push(Line::from(Span::styled(
            truncate_width(&format!(" {indent}• {}", entry.text), inner.width as usize),
            style,
        )));
        ui.hitmap.add_line(
            inner.x,
            inner.y + idx as u16,
            inner.width,
            HitTarget::TocItem(idx),
        );
    }
    frame.render_widget(Paragraph::new(lines).style(sidebar_style), inner);
}
