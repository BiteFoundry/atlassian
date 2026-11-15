//! Tab 1 (`Ctrl+2`): Jira Issue Explorer, JQL Navigator, and Full Native Issue Inspector.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    models::{DocBlock, InlineNode, InlineStyle, IssueDetail, StatusCategory},
    ui::{
        app::{AppState, PaneFocus, UiState},
        doc_view::{pad_right_width, render_blocks, truncate_width, wrap_plain_text},
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
    // Top JQL bar inside main_area (or split above nav+main if we put JQL at top of main).
    let main_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .split(main_area);

    draw_jql_bar(frame, main_rows[0], app, ui);

    if let Some(nav_rect) = nav_area {
        draw_issue_nav(frame, nav_rect, app, ui);
    }

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

    let title = if filter_focused {
        " 로컬 필터 (/) — Esc: 종료 "
    } else {
        " JQL 검색바 (J: 포커스 · Enter: 실행 · /: 로컬필터) "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            title,
            theme.title(jql_focused || filter_focused),
        )))
        .border_style(theme.border_style(jql_focused || filter_focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    ui.hitmap.add(area, HitTarget::JqlBar);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let line = if filter_focused {
        Line::from(vec![
            Span::styled(" / ", theme.accent_style()),
            Span::raw(ui.issue_ui.local_filter.text.clone()),
            Span::styled("▏", theme.accent_style()),
        ])
    } else {
        let mut spans = vec![
            Span::styled(" JQL> ", theme.accent_style()),
            Span::raw(ui.issue_ui.jql.text.clone()),
        ];
        if jql_focused {
            spans.push(Span::styled("▏", theme.accent_style()));
        }
        if !ui.issue_ui.local_filter.text.trim().is_empty() {
            spans.push(Span::styled(
                format!("   [필터: {}]", ui.issue_ui.local_filter.text),
                Style::default().fg(theme.warning),
            ));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), inner);
}

fn draw_issue_nav(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let focused = ui.focus == PaneFocus::Nav;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" 필터 & 이슈 목록 ", theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    ui.hitmap.add(area, HitTarget::NavPane);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    let filter_h = (app.saved_filters.len() as u16 + 1).clamp(3, 7);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(filter_h), Constraint::Min(2)])
        .split(inner);

    // Saved filters
    let mut f_lines = vec![Line::from(Span::styled(" 저장된 필터", theme.muted_style()))];
    for (idx, f) in app.saved_filters.iter().enumerate().take(rows[0].height as usize - 1) {
        let is_sel = ui.issue_ui.selected_filter == idx;
        let is_active_focus = focused && ui.issue_ui.nav_section == 0 && is_sel;
        let style = if is_active_focus {
            theme.selection()
        } else if is_sel {
            theme.accent_style()
        } else {
            Style::default()
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
    frame.render_widget(Paragraph::new(f_lines), rows[0]);

    // Issue list
    let issues = app.filtered_issues(ui);
    let list_h = rows[1].height.saturating_sub(1) as usize;
    let mut i_lines = vec![Line::from(Span::styled(
        format!(" 이슈 ({})", issues.len()),
        theme.muted_style(),
    ))];
    if issues.is_empty() {
        i_lines.push(Line::from(Span::styled("  표시할 이슈가 없습니다", theme.muted_style())));
    } else if list_h > 0 {
        let sel = ui.issue_ui.selected_issue.min(issues.len().saturating_sub(1));
        let start = if sel >= list_h { sel - list_h + 1 } else { 0 };
        for (vis_idx, issue) in issues.iter().skip(start).take(list_h).enumerate() {
            let abs_idx = start + vis_idx;
            let is_sel = abs_idx == sel;
            let (icon, icon_color) = theme.issue_type_badge(&issue.issue_type);
            let row_style = if is_sel && focused && ui.issue_ui.nav_section == 1 {
                theme.selection()
            } else if is_sel {
                Style::default()
                    .bg(theme.card_selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let title_budget = rows[1]
                .width
                .saturating_sub(icon.len() as u16 + issue.key.len() as u16 + 4)
                as usize;
            let summary = truncate_width(&issue.summary, title_budget);
            i_lines.push(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(format!("{} ", issue.key), theme.accent_style().patch(row_style)),
                Span::styled(summary, row_style),
            ]));
            ui.hitmap.add_line(
                rows[1].x,
                rows[1].y + 1 + vis_idx as u16,
                rows[1].width,
                HitTarget::IssueRow(abs_idx),
            );
        }
    }
    frame.render_widget(Paragraph::new(i_lines), rows[1]);
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
    let title = match &app.active_issue {
        Some(d) => format!(" {} — 이슈 상세 ", d.summary.key),
        None => " 이슈 상세 ".to_string(),
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

    let Some(detail) = &app.active_issue else {
        let empty = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  선택된 이슈가 없습니다. 좌측 목록에서 이슈를 선택하거나 [n]으로 새 이슈를 생성하세요.",
                theme.muted_style(),
            )),
        ];
        frame.render_widget(Paragraph::new(empty), inner);
        return;
    };

    let lines = build_issue_inspector_lines(detail, inner.width, app, ui, compact_drawer);

    // Register clickable action bar buttons on row 2 (breadcrumb=0, summary=1, action_bar=2) when visible and unscrolled.
    if scroll == 0 && inner.height > 2 {
        register_issue_action_buttons(inner.x, inner.y + 2, inner.width, &mut ui.hitmap);
    }

    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

fn register_issue_action_buttons(x: u16, row: u16, width: u16, hitmap: &mut HitMap) {
    let buttons = [
        ("[m]전환", ButtonId::Transition),
        ("[i]내게할당", ButtonId::AssignToMe),
        ("[a]담당자", ButtonId::Assignee),
        ("[c]댓글", ButtonId::Comment),
        ("[e]수정", ButtonId::Edit),
        ("[n]새이슈", ButtonId::NewItem),
        ("[u]파일첨부", ButtonId::UploadAttachment),
        ("[v]이미지", ButtonId::ImagePreview),
        ("[g]연결문서점프", ButtonId::CrossLinks),
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

pub fn build_issue_inspector_lines(
    detail: &IssueDetail,
    width: u16,
    app: &AppState,
    ui: &UiState,
    compact: bool,
) -> Vec<Line<'static>> {
    let theme = &ui.theme;
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
        " [m]전환 [i]내게할당 [a]담당자 [c]댓글 [e]수정 [g]연결점프 [v]이미지 [o]열기"
    } else {
        " [m]전환 [i]내게할당 [a]담당자 [c]댓글 [e]수정 [n]새이슈 [u]파일첨부 [v]이미지 [g]연결문서점프 [o]브라우저"
    };
    lines.push(Line::from(Span::styled(
        truncate_width(action_bar, w),
        Style::default().fg(theme.confluence_teal),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(120)),
        Style::default().fg(theme.border),
    )));

    // 4. 2-Column Metadata Box
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
        .unwrap_or("미할당");
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
        Span::styled(" 상태: ", theme.muted_style()),
        theme.status_lozenge_span(&s.status.name, s.status.category),
        Span::raw("   "),
        Span::styled("우선순위: ", theme.muted_style()),
        Span::styled(format!("{prio_icon} {prio_name}"), Style::default().fg(prio_color)),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" 담당자: {assignee}"), col1_w),
            col1_w,
        )),
        Span::styled(
            truncate_width(&format!("보고자: {reporter}"), w.saturating_sub(col1_w)),
            theme.muted_style(),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" 스프린트: {sprint}"), col1_w),
            col1_w,
        )),
        Span::raw(truncate_width(
            &format!("스토리 포인트: {pts}"),
            w.saturating_sub(col1_w),
        )),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad_right_width(
            &truncate_width(&format!(" 라벨: {labels}"), col1_w),
            col1_w,
        )),
        Span::styled(
            truncate_width(&format!("수정버전: {fix_v}"), w.saturating_sub(col1_w)),
            theme.muted_style(),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        truncate_width(&format!(" 업데이트: {}", s.updated_at), w),
        theme.muted_style(),
    )));
    lines.push(Line::from(""));

    // 5. Description
    lines.push(Line::from(Span::styled(
        " ▾ 설명",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
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
        lines.push(Line::from(Span::styled("  (설명 없음)", theme.muted_style())));
    } else {
        for mut l in render_blocks(
            &desc_blocks,
            w.saturating_sub(2) as u16,
            theme,
            Some(&app.image_cache),
            ui.inline_images,
        ) {
            l.spans.insert(0, Span::raw("  "));
            lines.push(l);
        }
    }
    lines.push(Line::from(""));

    // 6. Subtasks
    if !detail.subtasks.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" ▾ 하위 작업 ({})", detail.subtasks.len()),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for st in &detail.subtasks {
            let done = st.status.category == StatusCategory::Done;
            let chk = if done { "☑" } else { "☐" };
            let chk_col = if done { theme.success } else { theme.muted };
            lines.push(Line::from(vec![
                Span::styled(format!("  {chk} "), Style::default().fg(chk_col)),
                Span::styled(format!("{} ", st.key), theme.accent_style()),
                Span::raw(truncate_width(&st.summary, w.saturating_sub(18))),
            ]));
        }
        lines.push(Line::from(""));
    }

    // 7. Linked Issues & Linked Confluence Pages
    if !detail.linked_issues.is_empty() || !detail.linked_pages.is_empty() {
        lines.push(Line::from(Span::styled(
            " ▾ 연결된 이슈 & Confluence 문서 ([g]로 점프)",
            Style::default()
                .fg(theme.confluence_teal)
                .add_modifier(Modifier::BOLD),
        )));
        for lp in &detail.linked_pages {
            lines.push(Line::from(vec![
                Span::raw("  📄 "),
                Span::styled(
                    "[Confluence] ",
                    Style::default()
                        .fg(theme.confluence_teal)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(truncate_width(&lp.title, w.saturating_sub(18))),
            ]));
        }
        for li in &detail.linked_issues {
            lines.push(Line::from(vec![
                Span::styled(format!("  🔗 {} ", li.relation_label), theme.muted_style()),
                Span::styled(format!("{} ", li.key), theme.accent_style()),
                theme.status_lozenge_span(&li.status.name, li.status.category),
                Span::raw(format!(
                    " {}",
                    truncate_width(&li.summary, w.saturating_sub(36))
                )),
            ]));
        }
        lines.push(Line::from(""));
    }

    // 8. Attachments
    if !detail.attachments.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" ▾ 첨부파일 ({}) — [d]다운로드 [v]이미지미리보기", detail.attachments.len()),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for att in &detail.attachments {
            let kb = (att.size / 1024).max(1);
            lines.push(Line::from(vec![
                Span::raw("  📎 "),
                Span::styled(att.filename.clone(), Style::default().fg(theme.chip_fg)),
                Span::styled(format!(" ({kb} KB · {})", att.mime_type), theme.muted_style()),
            ]));
        }
        lines.push(Line::from(""));
    }

    // 9. Comments Thread
    lines.push(Line::from(Span::styled(
        format!(" ▾ 댓글 ({}) — [c]댓글 작성", detail.comments.len()),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    if detail.comments.is_empty() {
        lines.push(Line::from(Span::styled("  (댓글 없음)", theme.muted_style())));
    } else {
        for c in &detail.comments {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  💬 {} ", c.author.display_name),
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
