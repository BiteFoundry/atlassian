//! Tab 3 (`Ctrl+4`): Confluence Space Selector, Expandable Page Tree, Wiki Reader & TOC Drawer.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    models::{DocBlock, InlineNode, InlineStyle, PageDetail},
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::{render_blocks, truncate_width},
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

    draw_wiki_reader(frame, main_area, app, ui);

    if let Some(toc_rect) = drawer_area {
        draw_toc_drawer(frame, toc_rect, app, ui);
    }
}

fn draw_space_and_tree(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Nav;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" 스페이스 & 페이지 트리 ", theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    ui.hitmap.add(area, HitTarget::NavPane);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    let space_h = (app.spaces.len() as u16 + 1).clamp(3, 7);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(space_h), Constraint::Min(2)])
        .split(inner);

    // Space Selector
    let mut s_lines = vec![Line::from(Span::styled(" Confluence 스페이스", theme.muted_style()))];
    if app.spaces.is_empty() {
        s_lines.push(Line::from(Span::styled("  (스페이스 없음)", theme.muted_style())));
    } else {
        for (idx, sp) in app
            .spaces
            .iter()
            .enumerate()
            .take(rows[0].height.saturating_sub(1) as usize)
        {
            let is_sel = ui.confluence_ui.selected_space == idx;
            let style = if focused && ui.confluence_ui.nav_section == 0 && is_sel {
                theme.selection()
            } else if is_sel {
                Style::default()
                    .fg(theme.confluence_teal)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let pfx = if is_sel { "▸ " } else { "  " };
            let pin = if sp.pinned { "★ " } else { "" };
            s_lines.push(Line::from(Span::styled(
                truncate_width(&format!("{pfx}{pin}{} ({})", sp.name, sp.key), rows[0].width as usize),
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
    frame.render_widget(Paragraph::new(s_lines), rows[0]);

    // Expandable Hierarchical Page Tree
    let tree = app.current_confluence_tree(ui);
    let tree_h = rows[1].height.saturating_sub(1) as usize;
    let mut t_lines = vec![Line::from(Span::styled(
        format!(" 페이지 트리 ({}) [Space/→ 확장]", tree.len()),
        theme.muted_style(),
    ))];
    if tree.is_empty() {
        t_lines.push(Line::from(Span::styled(
            "  (페이지 없음 — [n] 새 페이지)",
            theme.muted_style(),
        )));
    } else if tree_h > 0 {
        let sel = ui
            .confluence_ui
            .selected_tree_row
            .min(tree.len().saturating_sub(1));
        let start = if sel >= tree_h { sel - tree_h + 1 } else { 0 };
        for (vis_idx, row) in tree.iter().skip(start).take(tree_h).enumerate() {
            let abs_idx = start + vis_idx;
            let is_sel = abs_idx == sel;
            let style = if focused && ui.confluence_ui.nav_section == 1 && is_sel {
                theme.selection()
            } else if is_sel {
                Style::default()
                    .bg(theme.card_selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let indent = "  ".repeat(row.depth);
            let branch = if row.has_children {
                if row.expanded {
                    "▾ 📁 "
                } else {
                    "▸ 📁 "
                }
            } else {
                "  📄 "
            };
            let text = truncate_width(
                &format!("{indent}{branch}{}", row.page.title),
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
    frame.render_widget(Paragraph::new(t_lines), rows[1]);
}

fn draw_wiki_reader(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Main;
    let title = match &app.active_page {
        Some(p) => format!(" 📘 {} — Wiki Reader ", p.summary.title),
        None => " 📘 Confluence Wiki Reader ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(title, theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    ui.hitmap.add(area, HitTarget::DetailPane);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(page) = &app.active_page else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  좌측 트리에서 Confluence 페이지를 선택하거나 [n]으로 새 페이지를 작성하세요.",
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
        ("[e]편집($EDITOR)", ButtonId::Edit),
        ("[n]하위페이지생성", ButtonId::NewItem),
        ("[c]댓글", ButtonId::Comment),
        ("[t]목차(TOC)", ButtonId::ToggleToc),
        ("[u]첨부업로드", ButtonId::UploadAttachment),
        ("[d]첨부다운로드", ButtonId::DownloadAttachment),
        ("[v]이미지", ButtonId::ImagePreview),
        ("[g]연결이슈점프", ButtonId::CrossLinks),
        ("[o]브라우저", ButtonId::OpenBrowser),
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
    let w = (width as usize).max(20);
    let mut lines = Vec::new();
    let s = &page.summary;

    // 1. Breadcrumb: SPACE > Ancestor > Title
    let mut crumb_parts = vec![s.space_key.clone()];
    for anc in &page.ancestors {
        crumb_parts.push(anc.title.clone());
    }
    crumb_parts.push(s.title.clone());
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" {}", crumb_parts.join(" > ")), w),
        Style::default().fg(theme.confluence_teal),
    )));

    // 2. Title
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" {}", s.title), w),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    // 3. Metadata: v{N} · 작성자 · 수정시각 · #labels
    let author = s.author_name.as_deref().unwrap_or("알 수 없음");
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
    lines.push(Line::from(Span::styled(
        truncate_width(
            " [e]편집($EDITOR) [n]하위페이지생성 [c]댓글 [t]목차(TOC) [u]첨부업로드 [d]첨부다운로드 [v]이미지 [g]연결이슈점프 [o]브라우저",
            w,
        ),
        Style::default().fg(theme.accent),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(120)),
        Style::default().fg(theme.border),
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
        lines.push(Line::from(Span::styled(" (본문 없음)", theme.muted_style())));
    } else {
        for mut l in render_blocks(
            &body_blocks,
            w.saturating_sub(2) as u16,
            theme,
            Some(&app.image_cache),
            ui.inline_images,
        ) {
            l.spans.insert(0, Span::raw(" "));
            lines.push(l);
        }
    }
    lines.push(Line::from(""));

    // 6. Attachments
    if !page.attachments.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" ▾ 첨부파일 ({}) — [d]다운로드 [v]이미지미리보기", page.attachments.len()),
            Style::default()
                .fg(theme.confluence_teal)
                .add_modifier(Modifier::BOLD),
        )));
        for att in &page.attachments {
            let kb = (att.size / 1024).max(1);
            lines.push(Line::from(vec![
                Span::raw("  📎 "),
                Span::styled(att.filename.clone(), Style::default().fg(theme.chip_fg)),
                Span::styled(format!(" ({kb} KB · {})", att.mime_type), theme.muted_style()),
            ]));
        }
        lines.push(Line::from(""));
    }

    // 7. Footer comments
    lines.push(Line::from(Span::styled(
        format!(" ▾ 페이지 댓글 ({}) — [c]댓글 작성", page.comments.len()),
        Style::default()
            .fg(theme.confluence_teal)
            .add_modifier(Modifier::BOLD),
    )));
    if page.comments.is_empty() {
        lines.push(Line::from(Span::styled("  (댓글 없음)", theme.muted_style())));
    } else {
        for c in &page.comments {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  💬 {} ", c.author_name),
                    Style::default()
                        .fg(theme.mention_fg)
                        .add_modifier(Modifier::BOLD),
                ),
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
            for mut l in render_blocks(&c_blocks, w.saturating_sub(4) as u16, theme, None, false) {
                l.spans.insert(0, Span::raw("    "));
                lines.push(l);
            }
            lines.push(Line::from(""));
        }
    }

    lines
}

fn draw_toc_drawer(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Detail;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" 목차 (TOC) [t] ", theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(page) = &app.active_page else {
        return;
    };
    let toc: Vec<_> = page.toc.iter().filter(|t| t.level <= 4).collect();
    if toc.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" (목차 없음)", theme.muted_style())),
            inner,
        );
        return;
    }

    let mut lines = Vec::new();
    for (idx, entry) in toc.iter().enumerate().take(inner.height as usize) {
        let is_sel = ui.confluence_ui.selected_toc == idx;
        let style = if focused && is_sel {
            theme.selection()
        } else if is_sel {
            theme.accent_style()
        } else {
            Style::default()
        };
        let indent = "  ".repeat(entry.level.saturating_sub(1) as usize);
        lines.push(Line::from(Span::styled(
            truncate_width(&format!("{indent}• {}", entry.text), inner.width as usize),
            style,
        )));
        ui.hitmap.add_line(
            inner.x,
            inner.y + idx as u16,
            inner.width,
            HitTarget::TocItem(idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
