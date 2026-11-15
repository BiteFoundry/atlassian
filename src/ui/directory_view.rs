//! Tab 5 (`Ctrl+6`): Projects & Spaces Directory, Cache Stats, Sync & Self-Updater.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::{ButtonId, HitTarget},
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(4)])
        .split(main_area);

    draw_stats_and_controls(frame, rows[0], app, ui);

    let cols = if rows[1].width >= 80 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1])
    };

    draw_projects(frame, cols[0], app, ui);
    draw_spaces(frame, cols[1], app, ui);
}

fn draw_stats_and_controls(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            " 동기화 · 캐시 통계 · 설정 ",
            theme.title(true),
        )))
        .border_style(theme.border_style(false));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let c = &app.cache_counts;
    let auth_status = app.auth.status_summary();
    let sync_lbl = if app.syncing {
        "동기화 중…".to_string()
    } else if app.sync_summary.is_empty() {
        "대기".to_string()
    } else {
        app.sync_summary.clone()
    };
    let update_lbl = match &ui.directory_ui.last_update {
        Some(u) if u.update_available => format!("새 버전 {} 있음!", u.latest_version),
        Some(u) => format!("최신 ({})", u.current_version),
        None => format!("v{}", env!("CARGO_PKG_VERSION")),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(" 사이트: ", theme.muted_style()),
            Span::styled(app.site_display(), theme.accent_style()),
            Span::styled(format!(" ({})  |  인증: ", app.config.deployment.as_str()), theme.muted_style()),
            Span::raw(auth_status),
        ]),
        Line::from(vec![
            Span::styled(" SQLite 캐시: ", theme.muted_style()),
            Span::raw(format!(
                "프로젝트 {} · 보드 {} · 이슈 {} · 스페이스 {} · 페이지 {}",
                c.projects, c.boards, c.issues, c.spaces, c.pages
            )),
        ]),
        Line::from(vec![
            Span::styled(" 상태: ", theme.muted_style()),
            Span::raw(sync_lbl),
            Span::styled("  |  버전: ", theme.muted_style()),
            Span::raw(update_lbl),
            Span::styled(
                format!(
                    "  |  인라인이미지:{}  자동서랍:{}",
                    if ui.inline_images { "ON" } else { "OFF" },
                    if ui.auto_detail_drawer { "ON" } else { "OFF" }
                ),
                theme.muted_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [r]전체동기화 ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(" [U]업데이트확인 ", Style::default().fg(theme.confluence_teal).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(" [p]고정(Pin)토글 ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(" [h/l]목록전환 ", theme.muted_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    if inner.height >= 4 {
        let row = inner.y + 3;
        ui.hitmap.add_line(inner.x + 1, row, 13, HitTarget::Button(ButtonId::SyncAll));
        ui.hitmap.add_line(inner.x + 16, row, 15, HitTarget::Button(ButtonId::CheckUpdate));
    }
}

fn draw_projects(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.directory_ui.section == 0;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 🔷 Jira 프로젝트 ({}) ", app.projects.len()),
            theme.title(active),
        )))
        .border_style(theme.border_style(active));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (프로젝트 없음)", theme.muted_style())),
            inner,
        );
        return;
    }

    let max_rows = inner.height as usize;
    let sel = ui
        .directory_ui
        .selected_project
        .min(app.projects.len().saturating_sub(1));
    let start = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
    let mut lines = Vec::new();
    for (vis_idx, proj) in app.projects.iter().skip(start).take(max_rows).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let style = if active && is_sel {
            theme.selection()
        } else if is_sel {
            Style::default().bg(theme.card_selected_bg)
        } else {
            Style::default()
        };
        let pin = if proj.pinned { "★ " } else { "  " };
        lines.push(Line::from(Span::styled(
            truncate_width(
                &format!("{}{pin}[{}] {} ({})", if is_sel { "▸ " } else { "  " }, proj.key, proj.name, proj.project_type),
                inner.width as usize,
            ),
            style,
        )));
        ui.hitmap.add_line(
            inner.x,
            inner.y + vis_idx as u16,
            inner.width,
            HitTarget::DirectoryProject(abs_idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_spaces(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.directory_ui.section == 1;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 📘 Confluence 스페이스 ({}) ", app.spaces.len()),
            theme.title(active),
        )))
        .border_style(theme.border_style(active));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.spaces.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (스페이스 없음)", theme.muted_style())),
            inner,
        );
        return;
    }

    let max_rows = inner.height as usize;
    let sel = ui
        .directory_ui
        .selected_space
        .min(app.spaces.len().saturating_sub(1));
    let start = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
    let mut lines = Vec::new();
    for (vis_idx, sp) in app.spaces.iter().skip(start).take(max_rows).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let style = if active && is_sel {
            theme.selection()
        } else if is_sel {
            Style::default().bg(theme.card_selected_bg)
        } else {
            Style::default()
        };
        let pin = if sp.pinned { "★ " } else { "  " };
        lines.push(Line::from(Span::styled(
            truncate_width(
                &format!("{}{pin}[{}] {} ({})", if is_sel { "▸ " } else { "  " }, sp.key, sp.name, sp.space_type),
                inner.width as usize,
            ),
            style,
        )));
        ui.hitmap.add_line(
            inner.x,
            inner.y + vis_idx as u16,
            inner.width,
            HitTarget::DirectorySpace(abs_idx),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
