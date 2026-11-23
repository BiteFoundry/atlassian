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
    let mut lines = vec![Line::from(Span::styled(
        truncate_width(&title, area.width as usize),
        theme.muted_style(),
    ))];
    if show_filter {
        let cursor = if ui.confluence_ui.space_filter_active {
            "▏"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(" / ", theme.accent_style()),
            Span::raw(ui.confluence_ui.space_filter.text.clone()),
            Span::styled(cursor, theme.accent_style()),
            Span::styled(
                format!("  {matched}/{}", app.spaces.len()),
                theme.muted_style(),
            ),
        ]));
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
        ("[H: HTML]", ButtonId::HtmlPreview),
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
            " [e]Edit [n]Child Page [c]Comment [t]TOC [H: HTML] [u]Upload [d]Download [v]Image [g]Links [o]Open",
            w,
        ),
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
    let hdr_text = truncate_width(
        &format!("{focus_mark} Table of Contents [t]"),
        area.width as usize,
    );
    let hdr_w = unicode_width::UnicodeWidthStr::width(hdr_text.as_str()) as u16;
    ui.hitmap
        .add_line(hdr_rect.x, hdr_rect.y, hdr_w, HitTarget::Button(ButtonId::ToggleToc));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hdr_text, theme.title(focused))))
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
