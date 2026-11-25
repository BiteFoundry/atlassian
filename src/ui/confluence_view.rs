//! Tab 3 (`Ctrl+4`): Minimal-chrome Confluence Space Selector, Page Tree, Wiki Reader & TOC Drawer.

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
    models::PageDetail,
    ui::{
        app::{space_group_key, AppState, PaneFocus, SpacePaneRow, UiState},
        doc_view::{blocks_or_markdown, render_blocks_with_options, truncate_width},
        draw_horizontal_hairline, fill_surface,
        hitmap::{ButtonId, HitMap, HitTarget},
        visible_window,
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
    let filter_rows = u16::from(space_filter_shown(ui));
    let space_h = u16::try_from(app.spaces.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1 + filter_rows + extra_group_rows)
        .clamp(3, (inner.height / 2).clamp(7, 12));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(space_h), Constraint::Min(2)])
        .split(inner);

    draw_spaces_pane(frame, rows[0], app, ui, multi_site);

    // Register Page Tree container BEFORE tree item rows.
    ui.hitmap.add(rows[1], HitTarget::WikiNavTree);

    // Expandable Hierarchical Page Tree
    let theme = &ui.theme;
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
            let row_y = rows[1].y + 1 + vis_idx as u16;
            ui.hitmap.add_line(
                rows[1].x,
                row_y,
                rows[1].width,
                HitTarget::TreeRow(abs_idx),
            );
            if row.has_children {
                let indent_w = indent.width() as u16;
                let chev_x = rows[1].x.saturating_add(indent_w);
                if chev_x < rows[1].right() {
                    let chev_w = 3u16.min(rows[1].right().saturating_sub(chev_x));
                    ui.hitmap.add_line(
                        chev_x,
                        row_y,
                        chev_w,
                        HitTarget::TreeChevron(abs_idx),
                    );
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(t_lines).style(sidebar_style), rows[1]);
}

/// True when the SPACES pane shows its filter row (typing, or a query is kept).
fn space_filter_shown(ui: &UiState) -> bool {
    ui.confluence_ui.space_filter_active || !ui.confluence_ui.space_filter.text.is_empty()
}

/// SPACES pane: title, optional filter row, and the window of rows that keeps the
/// selected space on screen (grouped under site headers in All Sites mode).
fn draw_spaces_pane(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    multi_site: bool,
) {
    // Register Spaces nav section container BEFORE header buttons & item rows.
    ui.hitmap.add(area, HitTarget::WikiNavSpaces);

    let pane_rows = app.space_pane_rows(ui);
    let show_filter = space_filter_shown(ui);
    let selected = ui.confluence_ui.selected_space;
    let sel_row = pane_rows
        .iter()
        .position(|row| *row == SpacePaneRow::Space(selected));
    let list_h = (area.height as usize).saturating_sub(1 + usize::from(show_filter));
    let (mut scroll, mut window) = visible_window(
        sel_row.unwrap_or(ui.confluence_ui.space_scroll),
        ui.confluence_ui.space_scroll,
        list_h,
        pane_rows.len(),
    );
    // Scrolling up onto a group's first space also brings its site header into view.
    if let Some(r) = sel_row.filter(|&r| r == scroll && r > 0 && list_h > 1) {
        if matches!(pane_rows[r - 1], SpacePaneRow::SiteHeader { .. }) {
            scroll = r - 1;
            window = scroll..(scroll + list_h).min(pane_rows.len());
        }
    }
    ui.confluence_ui.space_scroll = scroll;

    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Nav;
    let sidebar_style = theme.surface_style(theme.bg_sidebar);
    let is_space = |row: &SpacePaneRow| matches!(row, SpacePaneRow::Space(_));
    let listed = pane_rows.iter().filter(|row| is_space(row)).count();
    let matched: usize = if multi_site {
        pane_rows
            .iter()
            .map(|row| match row {
                SpacePaneRow::SiteHeader { spaces, .. } => *spaces,
                SpacePaneRow::Space(_) => 0,
            })
            .sum()
    } else {
        listed
    };

    let mut title = " SPACES".to_string();
    if window.start > 0 || window.end < pane_rows.len() {
        let up = if window.start > 0 { "▲" } else { "" };
        let down = if window.end < pane_rows.len() {
            "▼"
        } else {
            ""
        };
        match sel_row {
            Some(r) => {
                let pos = pane_rows[..=r].iter().filter(|row| is_space(row)).count();
                title.push_str(&format!(" {up}{down} {pos}/{listed}"));
            }
            None => title.push_str(&format!(" {up}{down} {listed}")),
        }
    }
    let filter_btn = "[/]";
    let filter_btn_w = filter_btn.width();
    let header_line = if (area.width as usize) > title.width() + filter_btn_w {
        let pad = (area.width as usize).saturating_sub(title.width() + filter_btn_w);
        let btn_x = area.right().saturating_sub(filter_btn_w as u16);
        ui.hitmap.add_line(
            btn_x,
            area.y,
            filter_btn_w as u16,
            HitTarget::Button(ButtonId::FocusLocalFilter),
        );
        Line::from(vec![
            Span::styled(title, theme.muted_style()),
            Span::raw(" ".repeat(pad)),
            Span::styled(filter_btn, theme.accent_style()),
        ])
    } else {
        Line::from(Span::styled(
            truncate_width(&title, area.width as usize),
            theme.muted_style(),
        ))
    };
    let mut lines = vec![header_line];
    if show_filter {
        let filter_y = area.y + 1;
        // Clicking the filter bar focuses the filter instead of falling through to NavPane.
        ui.hitmap.add_line(
            area.x,
            filter_y,
            area.width,
            HitTarget::Button(ButtonId::FocusLocalFilter),
        );
        let cursor = if ui.confluence_ui.space_filter_active {
            "▏"
        } else {
            ""
        };
        let count_str = format!("  {matched}/{}", app.spaces.len());
        let mut filter_spans = vec![
            Span::styled(" / ", theme.accent_style()),
            Span::raw(ui.confluence_ui.space_filter.text.clone()),
            Span::styled(cursor, theme.accent_style()),
            Span::styled(count_str.clone(), theme.muted_style()),
        ];
        if ui.confluence_ui.space_filter_active {
            let used_w = " / ".width()
                + ui.confluence_ui.space_filter.text.width()
                + cursor.width()
                + count_str.width();
            let esc_lbl = "(Esc)";
            let esc_w = esc_lbl.width();
            let esc_x = area.x.saturating_add((used_w + 1) as u16);
            if esc_x < area.right() {
                let clipped_w = (esc_w as u16).min(area.right().saturating_sub(esc_x));
                ui.hitmap.add_line(
                    esc_x,
                    filter_y,
                    clipped_w,
                    HitTarget::Button(ButtonId::CloseLocalFilter),
                );
            }
            filter_spans.push(Span::styled(" ", theme.muted_style()));
            filter_spans.push(Span::styled(esc_lbl, theme.muted_style()));
        }
        lines.push(Line::from(filter_spans));
    }

    let list_y = area.y + 1 + u16::from(show_filter);
    if app.spaces.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no spaces)",
            theme.muted_style(),
        )));
    } else if pane_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matching spaces)",
            theme.muted_style(),
        )));
    }
    for (offset, row) in pane_rows[window].iter().enumerate() {
        let y = list_y + offset as u16;
        match row {
            SpacePaneRow::SiteHeader {
                site_id,
                spaces,
                collapsed,
            } => {
                let group_idx = ui.site_group_keys.len();
                ui.site_group_keys.push(space_group_key(site_id));
                let chevron = if *collapsed { "▸" } else { "▾" };
                let site_label = app.site_display_label(site_id);
                lines.push(Line::from(Span::styled(
                    truncate_width(
                        &format!(" {chevron} {site_label} ({spaces})"),
                        area.width as usize,
                    ),
                    theme.accent_style().patch(sidebar_style),
                )));
                ui.hitmap
                    .add_line(area.x, y, area.width, HitTarget::SiteGroupHeader(group_idx));
            }
            SpacePaneRow::Space(idx) => {
                let sp = &app.spaces[*idx];
                let is_sel = selected == *idx;
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
                let pfx = match (multi_site, is_sel) {
                    (true, true) => "  ▸ ",
                    (true, false) => "    ",
                    (false, true) => "▸ ",
                    (false, false) => "  ",
                };
                let pin = if sp.pinned { "★ " } else { "" };
                lines.push(Line::from(Span::styled(
                    truncate_width(
                        &format!("{pfx}{pin}{} ({})", sp.name, sp.key),
                        area.width as usize,
                    ),
                    style,
                )));
                ui.hitmap
                    .add_line(area.x, y, area.width, HitTarget::SpaceItem(*idx));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).style(sidebar_style), area);
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
        ui.confluence_ui.reader_max_scroll = 0;
        ui.confluence_ui.reader_scroll = 0;
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

    let (lines, action_row, row_hits) = build_wiki_reader_lines_with_hits(page, inner.width, app, ui);
    let max_scroll = lines.len().saturating_sub(inner.height as usize) as u16;
    let clamped_scroll = ui.confluence_ui.reader_scroll.min(max_scroll);
    ui.confluence_ui.reader_max_scroll = max_scroll;
    ui.confluence_ui.reader_scroll = clamped_scroll;

    // Register clickable action bar buttons whenever the action bar row is visible.
    let action_row_u16 = action_row as u16;
    if clamped_scroll <= action_row_u16 {
        let vis_row = action_row_u16 - clamped_scroll;
        if vis_row < inner.height {
            register_wiki_action_buttons(inner.x, inner.y + vis_row, inner.width, &mut ui.hitmap);
        }
    }

    // Register visible breadcrumb segments, attachment rows, and [c] Add Comment prompt.
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
        Paragraph::new(lines).scroll((clamped_scroll, 0)),
        inner,
    );
}

const WIKI_ACTION_BUTTONS: &[(&str, ButtonId)] = &[
    ("[e]Edit", ButtonId::Edit),
    ("[n]Child Page", ButtonId::NewItem),
    ("[c]Comment", ButtonId::Comment),
    ("[t]TOC", ButtonId::ToggleToc),
    ("[H: HTML]", ButtonId::HtmlPreview),
    ("[Ctrl+F]Upload", ButtonId::UploadAttachment),
    ("[d]Download", ButtonId::DownloadAttachment),
    ("[v]Image", ButtonId::ImagePreview),
    ("[g]Links", ButtonId::CrossLinks),
    ("[o]Open", ButtonId::OpenBrowser),
];

fn register_wiki_action_buttons(x: u16, row: u16, width: u16, hitmap: &mut HitMap) {
    let mut cx = x + 1;
    let max_x = x.saturating_add(width);
    for &(label, btn) in WIKI_ACTION_BUTTONS {
        let w = label.width() as u16;
        if cx.saturating_add(w) > max_x {
            break;
        }
        hitmap.add_line(cx, row, w, HitTarget::Button(btn));
        cx = cx.saturating_add(w + 1);
    }
}

struct WikiLineHit {
    line_idx: usize,
    x_offset: u16,
    width: Option<u16>,
    target: HitTarget,
}

fn build_wiki_reader_lines_with_hits(
    page: &PageDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
) -> (Vec<Line<'static>>, usize, Vec<WikiLineHit>) {
    let theme = &ui.theme;
    let is_compact = app.config.density == DensityMode::Compact;
    let w = (width as usize).max(20);
    let mut lines = Vec::new();
    let mut row_hits = Vec::new();
    let s = &page.summary;

    let teal_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.confluence_teal)
    };
    let teal_bold = teal_style.add_modifier(Modifier::BOLD);

    // 1. Breadcrumb: SPACE > Ancestor > Title
    let breadcrumb_line_idx = lines.len();
    let mut crumb_parts = vec![s.space_key.clone()];
    let mut cx = 1usize + s.space_key.width() + 3; // leading " " + space_key + " > "
    for (idx, anc) in page.ancestors.iter().enumerate() {
        let anc_w = anc.title.width();
        if cx < w && anc_w > 0 {
            let clipped_w = anc_w.min(w.saturating_sub(cx));
            if clipped_w > 0 {
                row_hits.push(WikiLineHit {
                    line_idx: breadcrumb_line_idx,
                    x_offset: cx as u16,
                    width: Some(clipped_w as u16),
                    target: HitTarget::WikiBreadcrumb(idx),
                });
            }
        }
        cx = cx.saturating_add(anc_w + 3);
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
    let action_row = lines.len();
    let action_style = if theme.no_color {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(theme.accent)
    };
    let action_bar = format!(
        " {}",
        WIKI_ACTION_BUTTONS
            .iter()
            .map(|(lbl, _)| *lbl)
            .collect::<Vec<_>>()
            .join(" ")
    );
    lines.push(Line::from(Span::styled(
        truncate_width(&action_bar, w),
        action_style,
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(120)),
        theme.subtle_style(),
    )));

    // 5. Wiki body
    let body_blocks = blocks_or_markdown(
        &page.blocks,
        (!page.body_markdown.trim().is_empty()).then_some(page.body_markdown.as_str()),
    );
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
        for (idx, att) in page.attachments.iter().enumerate() {
            let kb = (att.size / 1024).max(1);
            row_hits.push(WikiLineHit {
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

    // 7. Footer comments
    let comment_hdr_prefix = format!(" ▾ Page Comments ({}) — ", page.comments.len());
    let comment_btn_lbl = "[c] Add Comment";
    row_hits.push(WikiLineHit {
        line_idx: lines.len(),
        x_offset: comment_hdr_prefix.width() as u16,
        width: Some(comment_btn_lbl.width() as u16),
        target: HitTarget::Button(ButtonId::Comment),
    });
    lines.push(Line::from(Span::styled(
        format!("{comment_hdr_prefix}{comment_btn_lbl}"),
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

fn draw_toc_drawer(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Detail;
    let sidebar_style = theme.surface_style(theme.bg_sidebar);
    fill_surface(frame, area, sidebar_style);
    // Register TOC pane container BEFORE close pill & TOC items.
    ui.hitmap.add(area, HitTarget::TocPane);
    if area.width < 4 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let focus_mark = if focused { "▸" } else { " " };
    let close_pill = "[✕]";
    let close_w = close_pill.width() as u16;
    if area.width > close_w + 3 {
        let title_budget = (area.width as usize).saturating_sub(close_w as usize + 1);
        let title_text = truncate_width(&format!("{focus_mark} Table of Contents"), title_budget);
        let pad = (area.width as usize).saturating_sub(title_text.width() + close_w as usize);
        let close_x = hdr_rect.right().saturating_sub(close_w);
        ui.hitmap.add_line(
            close_x,
            hdr_rect.y,
            close_w,
            HitTarget::Button(ButtonId::ToggleToc),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(title_text, theme.title(focused)),
                Span::raw(" ".repeat(pad)),
                Span::styled(close_pill, theme.muted_style()),
            ]))
            .style(sidebar_style),
            hdr_rect,
        );
    } else {
        let hdr_text = truncate_width(
            &format!("{focus_mark} TOC"),
            area.width as usize,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hdr_text, theme.title(focused))))
                .style(sidebar_style),
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

    let Some(page) = &app.active_page else {
        ui.confluence_ui.toc_scroll = 0;
        return;
    };
    let toc: Vec<_> = page.toc.iter().filter(|t| t.level <= 4).collect();
    if toc.is_empty() {
        ui.confluence_ui.toc_scroll = 0;
        frame.render_widget(
            Paragraph::new(Span::styled(" (no headings)", theme.muted_style()))
                .style(sidebar_style),
            inner,
        );
        return;
    }

    let (scroll, window) = visible_window(
        ui.confluence_ui.selected_toc,
        ui.confluence_ui.toc_scroll,
        inner.height as usize,
        toc.len(),
    );
    ui.confluence_ui.toc_scroll = scroll;

    let mut lines = Vec::new();
    for (offset, idx) in window.enumerate() {
        let entry = toc[idx];
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
            inner.y + offset as u16,
            inner.width,
            HitTarget::TocItem(idx),
        );
    }
    frame.render_widget(Paragraph::new(lines).style(sidebar_style), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, DeploymentKind, Persona, SiteProfile},
        models::{SiteScope, SpaceSummary},
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn app_and_ui(cfg: AppConfig) -> (AppState, UiState) {
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Confluence,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);
        ui.focus = PaneFocus::Nav;
        (app, ui)
    }

    fn single_site() -> (AppState, UiState) {
        app_and_ui(AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            ..Default::default()
        })
    }

    fn space(key: &str, name: &str) -> SpaceSummary {
        SpaceSummary {
            id: format!("id-{key}"),
            key: key.into(),
            name: name.into(),
            space_type: "global".into(),
            ..Default::default()
        }
    }

    fn numbered_space(i: usize) -> SpaceSummary {
        space(&format!("S{i:03}"), &format!("Space {i:03}"))
    }

    /// Draws the Confluence nav pane into a `width` x `height` buffer; returns its rows.
    fn render_nav(app: &AppState, ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
        ui.hitmap.clear();
        ui.site_group_keys.clear();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| draw_space_and_tree(f, Rect::new(0, 0, width, height), app, ui))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn row_of(rows: &[String], text: &str) -> Option<u16> {
        rows.iter().position(|r| r.contains(text)).map(|y| y as u16)
    }

    #[tokio::test]
    async fn spaces_pane_scrolls_to_selected_row_beyond_height() {
        let (mut app, mut ui) = single_site();
        app.spaces = (0..300).map(numbered_space).collect();
        ui.confluence_ui.selected_space = 250;

        let rows = render_nav(&app, &mut ui, 40, 20);
        let y = row_of(&rows, "Space 250 (S250)").expect("selected space 250 is on screen");
        assert!(
            rows[y as usize].contains("▸ Space 250"),
            "row is marked selected"
        );
        assert_eq!(ui.hitmap.hit(y, 4), Some(HitTarget::SpaceItem(250)));
        assert!(
            row_of(&rows, "Space 000").is_none(),
            "top of the list scrolled away"
        );
        assert!(
            row_of(&rows, "SPACES ▲▼ 251/300").is_some(),
            "clipped title shows position:\n{}",
            rows.join("\n")
        );

        // Back to the top: the window follows the selection up again.
        ui.confluence_ui.selected_space = 0;
        let rows = render_nav(&app, &mut ui, 40, 20);
        let y = row_of(&rows, "Space 000 (S000)").expect("space 0 is on screen");
        assert_eq!(ui.hitmap.hit(y, 4), Some(HitTarget::SpaceItem(0)));
        assert!(row_of(&rows, "Space 250").is_none());
        assert!(
            row_of(&rows, "SPACES ▼ 1/300").is_some(),
            "{}",
            rows.join("\n")
        );
    }

    #[tokio::test]
    async fn spaces_pane_filter_narrows_rows_and_keeps_real_indices() {
        let (mut app, mut ui) = single_site();
        app.spaces = vec![
            space("ENG", "Engineering"),
            space("OPS", "Operations"),
            space("~jdoe", "John Doe"),
            space("DOCS", "Engineering Docs"),
            space("ENGX", "Experiments"),
        ];
        ui.confluence_ui.space_filter.set_text("eng");
        ui.confluence_ui.space_filter_active = true;
        ui.confluence_ui.selected_space = 3;

        // Ranked: exact key, key prefix, then name word prefix; indices stay real.
        assert_eq!(app.visible_space_indices(&ui), vec![0, 4, 3]);

        let rows = render_nav(&app, &mut ui, 40, 24);
        assert!(
            row_of(&rows, "/ eng▏  3/5").is_some(),
            "filter row:\n{}",
            rows.join("\n")
        );
        let eng = row_of(&rows, "Engineering (ENG)").unwrap();
        let engx = row_of(&rows, "Experiments (ENGX)").unwrap();
        let docs = row_of(&rows, "Engineering Docs (DOCS)").unwrap();
        assert!(eng < engx && engx < docs, "rows follow the ranking");
        assert_eq!(ui.hitmap.hit(eng, 4), Some(HitTarget::SpaceItem(0)));
        assert_eq!(ui.hitmap.hit(engx, 4), Some(HitTarget::SpaceItem(4)));
        assert_eq!(ui.hitmap.hit(docs, 4), Some(HitTarget::SpaceItem(3)));
        assert!(
            rows[docs as usize].contains("▸ Engineering Docs"),
            "selection kept"
        );
        assert!(row_of(&rows, "Operations").is_none());
        assert!(row_of(&rows, "John Doe").is_none());

        // `~` keeps personal spaces only.
        ui.confluence_ui.space_filter.set_text("~");
        assert_eq!(app.visible_space_indices(&ui), vec![2]);
        let rows = render_nav(&app, &mut ui, 40, 24);
        let jdoe = row_of(&rows, "John Doe (~jdoe)").unwrap();
        assert_eq!(ui.hitmap.hit(jdoe, 4), Some(HitTarget::SpaceItem(2)));
    }

    #[tokio::test]
    async fn spaces_pane_multi_site_scrolls_with_group_headers() {
        let site = |id: &str, name: &str| SiteProfile {
            id: id.into(),
            name: name.into(),
            site_url: format!("https://{id}.example.com"),
            deployment: DeploymentKind::Cloud,
            account_id: "default".into(),
            enabled: true,
            ..Default::default()
        };
        let (mut app, mut ui) = app_and_ui(AppConfig {
            sites: vec![site("site-a", "Alpha"), site("site-b", "Beta")],
            default_site: Some("site-a".into()),
            ..Default::default()
        });
        app.site_scope = SiteScope::All;
        app.spaces = (0..80).map(numbered_space).collect();
        app.space_site_ids = (0..80)
            .map(|i| if i < 40 { "site-a" } else { "site-b" }.to_string())
            .collect();
        assert!(app.is_multi_site_all_mode());

        // Deep in the second group: Beta's space 75 scrolls into view.
        ui.confluence_ui.selected_space = 75;
        let rows = render_nav(&app, &mut ui, 40, 20);
        let y = row_of(&rows, "Space 075 (S075)").expect("space 75 is on screen");
        assert_eq!(ui.hitmap.hit(y, 4), Some(HitTarget::SpaceItem(75)));
        assert!(
            row_of(&rows, "Alpha (40)").is_none(),
            "first group header scrolled away"
        );
        assert!(row_of(&rows, "76/80").is_some(), "{}", rows.join("\n"));

        // First space of the second group: its header row is on screen and clickable.
        ui.confluence_ui.selected_space = 40;
        let rows = render_nav(&app, &mut ui, 40, 20);
        let header = row_of(&rows, "▾ Beta (40)").expect("Beta header on screen");
        let first = row_of(&rows, "Space 040 (S040)").unwrap();
        assert_eq!(first, header + 1);
        assert_eq!(ui.hitmap.hit(first, 4), Some(HitTarget::SpaceItem(40)));
        match ui.hitmap.hit(header, 4) {
            Some(HitTarget::SiteGroupHeader(idx)) => {
                assert_eq!(ui.site_group_keys[idx], "wiki:site-b")
            }
            other => panic!("expected Beta group header, got {other:?}"),
        }

        // Navigation order is the display order: collapsing Alpha hides its spaces.
        ui.collapsed_site_groups.insert("wiki:site-a".into());
        let visible = app.visible_space_indices(&ui);
        assert_eq!(visible.first(), Some(&40));
        assert_eq!(visible.len(), 40);
    }
}
