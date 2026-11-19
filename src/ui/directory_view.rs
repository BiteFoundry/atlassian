//! Tab 5 (`Ctrl+6`): Minimal-chrome Projects & Spaces Directory, Cache Stats, Sync & Settings.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    config::DensityMode,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, draw_vertical_hairline, fill_surface,
        hitmap::{ButtonId, HitTarget},
        site_display_english, spinner_glyph,
    },
};

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    if main_area.width < 10 || main_area.height < 6 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(4)])
        .split(main_area);

    draw_stats_and_controls(frame, rows[0], app, ui);

    if rows[1].width >= 80 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(rows[1]);
        draw_projects(frame, cols[0], app, ui);
        draw_vertical_hairline(
            frame,
            cols[1].x,
            cols[1].y,
            cols[1].height,
            ui.theme.subtle_style(),
        );
        draw_spaces(frame, cols[2], app, ui);
    } else {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(3),
            ])
            .split(rows[1]);
        draw_projects(frame, split[0], app, ui);
        draw_horizontal_hairline(
            frame,
            split[1].x,
            split[1].y,
            split[1].width,
            ui.theme.subtle_style(),
        );
        draw_spaces(frame, split[2], app, ui);
    }
}

fn draw_stats_and_controls(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let card_style = theme.surface_style(theme.bg_card);
    let content_rect = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(1),
    );
    fill_surface(frame, content_rect, card_style);
    draw_horizontal_hairline(
        frame,
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        theme.subtle_style(),
    );

    if content_rect.width == 0 || content_rect.height < 4 {
        return;
    }

    let c = &app.cache_counts;
    let auth_status = if app.auth.is_logged_in() {
        "Authenticated"
    } else {
        "Not signed in"
    };
    let sync_lbl = if app.syncing {
        format!("{} Syncing…", spinner_glyph())
    } else if app.sync_summary.is_empty() {
        "Idle".to_string()
    } else {
        app.sync_summary.clone()
    };
    let update_lbl = match &ui.directory_ui.last_update {
        Some(u) if u.update_available => format!("Update available: {}", u.latest_version),
        Some(u) => format!("Up to date ({})", u.current_version),
        None => format!("v{}", env!("CARGO_PKG_VERSION")),
    };

    let sync_btn_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    let update_btn_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.confluence_teal)
            .add_modifier(Modifier::BOLD)
    };
    let pin_btn_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD)
    };

    let lines = vec![
        Line::from(Span::styled(
            " Sync · Cache Stats · Settings",
            theme.title(true),
        )),
        Line::from(vec![
            Span::styled(" Site: ", theme.muted_style()),
            Span::styled(site_display_english(app), theme.accent_style()),
            Span::styled(
                format!(" ({})  |  Auth: ", app.config.deployment.as_str()),
                theme.muted_style(),
            ),
            Span::raw(auth_status),
        ]),
        Line::from(vec![
            Span::styled(" SQLite Cache: ", theme.muted_style()),
            Span::raw(format!(
                "Projects {} · Boards {} · Issues {} · Spaces {} · Pages {}",
                c.projects, c.boards, c.issues, c.spaces, c.pages
            )),
        ]),
        Line::from(vec![
            Span::styled(" Status: ", theme.muted_style()),
            Span::raw(sync_lbl),
            Span::styled("  |  Version: ", theme.muted_style()),
            Span::raw(update_lbl),
            Span::styled(
                format!(
                    "  |  Theme: {}  Density: {}  Images: {}  Drawer: {}",
                    app.config.theme.label(),
                    app.config.density.label(),
                    if ui.inline_images { "ON" } else { "OFF" },
                    if ui.auto_detail_drawer { "ON" } else { "OFF" }
                ),
                theme.muted_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [r] Sync All ", sync_btn_style),
            Span::raw(" "),
            Span::styled(" [U] Check Update ", update_btn_style),
            Span::raw(" "),
            Span::styled(" [p] Toggle Pin ", pin_btn_style),
            Span::raw(" "),
            Span::styled(" [h/l] Switch List ", theme.muted_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).style(card_style), content_rect);

    if content_rect.height >= 5 {
        let row = content_rect.y + 4;
        ui.hitmap.add_line(
            content_rect.x + 1,
            row,
            12,
            HitTarget::Button(ButtonId::SyncAll),
        );
        ui.hitmap.add_line(
            content_rect.x + 16,
            row,
            16,
            HitTarget::Button(ButtonId::CheckUpdate),
        );
    }
}

fn draw_projects(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.directory_ui.section == 0;
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(
                &format!("{mark} Jira Projects ({})", app.projects.len()),
                area.width as usize,
            ),
            theme.title(active),
        ))),
        hdr_rect,
    );
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

    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (no projects)", theme.muted_style())),
            inner,
        );
        return;
    }

    let row_lines = match app.config.density {
        DensityMode::Cozy => 2usize,
        DensityMode::Compact => 1usize,
    };
    let max_items = ((inner.height as usize) / row_lines).max(1);
    let sel = ui
        .directory_ui
        .selected_project
        .min(app.projects.len().saturating_sub(1));
    let start = if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    };

    let mut lines = Vec::new();
    let mut cursor_y = inner.y;
    for (vis_idx, proj) in app.projects.iter().skip(start).take(max_items).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let style = if active && is_sel {
            theme.nav_selection()
        } else if is_sel {
            theme.selection()
        } else {
            Style::default()
        };
        let pin = if proj.pinned { "★ " } else { "  " };
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!("{}{pin}[{}] {}", if is_sel { "▸ " } else { "  " }, proj.key, proj.name),
                    w,
                ),
                style,
            )));
            lines.push(Line::from(Span::styled(
                truncate_width(&format!("      Type: {}", proj.project_type), w),
                theme.muted_style().patch(style),
            )));
            ui.hitmap.add_line(
                inner.x,
                cursor_y,
                inner.width,
                HitTarget::DirectoryProject(abs_idx),
            );
            ui.hitmap.add_line(
                inner.x,
                cursor_y + 1,
                inner.width,
                HitTarget::DirectoryProject(abs_idx),
            );
            cursor_y += 2;
        } else {
            lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!(
                        "{}{pin}[{}] {} ({})",
                        if is_sel { "▸ " } else { "  " },
                        proj.key,
                        proj.name,
                        proj.project_type
                    ),
                    w,
                ),
                style,
            )));
            ui.hitmap.add_line(
                inner.x,
                cursor_y,
                inner.width,
                HitTarget::DirectoryProject(abs_idx),
            );
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_spaces(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.directory_ui.section == 1;
    if area.width < 6 || area.height < 3 {
        return;
    }

    let hdr_rect = Rect::new(area.x, area.y, area.width, 1);
    let mark = if active { "▸" } else { " " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(
                &format!("{mark} Confluence Spaces ({})", app.spaces.len()),
                area.width as usize,
            ),
            theme.title(active),
        ))),
        hdr_rect,
    );
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

    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.spaces.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (no spaces)", theme.muted_style())),
            inner,
        );
        return;
    }

    let row_lines = match app.config.density {
        DensityMode::Cozy => 2usize,
        DensityMode::Compact => 1usize,
    };
    let max_items = ((inner.height as usize) / row_lines).max(1);
    let sel = ui
        .directory_ui
        .selected_space
        .min(app.spaces.len().saturating_sub(1));
    let start = if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    };

    let mut lines = Vec::new();
    let mut cursor_y = inner.y;
    for (vis_idx, sp) in app.spaces.iter().skip(start).take(max_items).enumerate() {
        let abs_idx = start + vis_idx;
        let is_sel = abs_idx == sel;
        let style = if active && is_sel {
            theme.nav_selection()
        } else if is_sel {
            theme.selection()
        } else {
            Style::default()
        };
        let pin = if sp.pinned { "★ " } else { "  " };
        let w = inner.width as usize;

        if row_lines == 2 && cursor_y + 1 < inner.bottom() {
            lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!("{}{pin}[{}] {}", if is_sel { "▸ " } else { "  " }, sp.key, sp.name),
                    w,
                ),
                style,
            )));
            lines.push(Line::from(Span::styled(
                truncate_width(&format!("      Type: {}", sp.space_type), w),
                theme.muted_style().patch(style),
            )));
            ui.hitmap.add_line(
                inner.x,
                cursor_y,
                inner.width,
                HitTarget::DirectorySpace(abs_idx),
            );
            ui.hitmap.add_line(
                inner.x,
                cursor_y + 1,
                inner.width,
                HitTarget::DirectorySpace(abs_idx),
            );
            cursor_y += 2;
        } else {
            lines.push(Line::from(Span::styled(
                truncate_width(
                    &format!(
                        "{}{pin}[{}] {} ({})",
                        if is_sel { "▸ " } else { "  " },
                        sp.key,
                        sp.name,
                        sp.space_type
                    ),
                    w,
                ),
                style,
            )));
            ui.hitmap.add_line(
                inner.x,
                cursor_y,
                inner.width,
                HitTarget::DirectorySpace(abs_idx),
            );
            cursor_y += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
