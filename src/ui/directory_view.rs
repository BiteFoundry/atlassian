//! Tab 5 (`Ctrl+6`): Minimal-chrome Connected Sites & Accounts, Projects & Spaces Directory, Cache Stats, Sync & Settings.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    auth::StoredCredentials,
    config::{DensityMode, DeploymentKind},
    models::SiteScope,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        draw_horizontal_hairline, draw_vertical_hairline, fill_surface,
        hitmap::{ButtonId, HitTarget},
        my_work_view::format_scope_badge,
        site_display_english, spinner_glyph, visible_window,
    },
};

/// Table row rendered in the Directory view's **Connected Sites & Accounts** section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedSiteRow {
    pub site_id: String,
    pub name: String,
    pub site_url: String,
    pub deployment_label: String,
    pub auth_label: String,
    pub enabled: bool,
    pub is_active: bool,
    pub sync_status: String,
}

impl ConnectedSiteRow {
    pub fn display_url(&self) -> &str {
        self.site_url
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
    }
}

/// Builds a fallback single-site row from legacy `AppState` config + auth when multi-site rows
/// are not explicitly passed via [`draw_with_sites`].
pub fn default_connected_sites(app: &AppState) -> Vec<ConnectedSiteRow> {
    let raw_url = app.config.site_url.trim();
    if raw_url.is_empty() {
        return Vec::new();
    }
    let host = raw_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let name = host
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(host)
        .to_string();
    let deployment_label = match app.config.deployment {
        DeploymentKind::Cloud => "Cloud".to_string(),
        DeploymentKind::DataCenter => "Data Center".to_string(),
    };
    let creds = app.auth.load_credentials().ok().flatten();
    let auth_label = match creds {
        Some(StoredCredentials::ApiToken { deployment, .. }) => match deployment {
            DeploymentKind::Cloud => "Cloud Token".to_string(),
            DeploymentKind::DataCenter => "DC Basic".to_string(),
        },
        Some(StoredCredentials::BearerPat { deployment, .. }) => match deployment {
            DeploymentKind::Cloud => "Cloud Bearer".to_string(),
            DeploymentKind::DataCenter => "DC PAT".to_string(),
        },
        Some(StoredCredentials::CloudOAuth { .. }) => "Cloud OAuth".to_string(),
        Some(StoredCredentials::CloudAccountToken { .. }) => "Cloud Token".to_string(),
        None => "Not signed in".to_string(),
    };
    let sync_status = if app.syncing {
        format!("{} Syncing…", spinner_glyph())
    } else if !app.sync_summary.trim().is_empty() {
        app.sync_summary.clone()
    } else if creds.is_some() {
        "Idle".to_string()
    } else {
        "Offline".to_string()
    };

    vec![ConnectedSiteRow {
        site_id: "default".to_string(),
        name,
        site_url: raw_url.to_string(),
        deployment_label,
        auth_label,
        enabled: true,
        is_active: true,
        sync_status,
    }]
}

pub fn draw(frame: &mut Frame, main_area: Rect, app: &AppState, ui: &mut UiState) {
    let default_sites = default_connected_sites(app);
    draw_with_sites(frame, main_area, app, ui, &default_sites, None);
}

/// Draws the Workspace / Directory view with explicit multi-site rows and optional active scope label.
pub fn draw_with_sites(
    frame: &mut Frame,
    main_area: Rect,
    app: &AppState,
    ui: &mut UiState,
    sites: &[ConnectedSiteRow],
    active_scope_label: Option<&str>,
) {
    if main_area.width < 10 || main_area.height < 6 {
        return;
    }

    let (stats_h, sites_h) = if main_area.height >= 18 {
        let wanted_sites = (sites.len().clamp(1, 4) as u16 + 4)
            .min(main_area.height.saturating_sub(11))
            .max(5);
        (6u16, wanted_sites)
    } else if main_area.height >= 12 {
        (5u16, 4u16)
    } else {
        (5u16, 0u16)
    };

    let rows = if sites_h > 0 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(stats_h),
                Constraint::Length(sites_h),
                Constraint::Min(3),
            ])
            .split(main_area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(stats_h), Constraint::Min(3)])
            .split(main_area)
    };

    draw_stats_and_controls(frame, rows[0], app, ui, sites);

    let list_area = if sites_h > 0 {
        draw_connected_sites(frame, rows[1], ui, sites, active_scope_label);
        rows[2]
    } else {
        rows[1]
    };

    if list_area.width >= 80 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(list_area);
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
            .split(list_area);
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

fn draw_connected_sites(
    frame: &mut Frame,
    area: Rect,
    ui: &mut UiState,
    sites: &[ConnectedSiteRow],
    active_scope_label: Option<&str>,
) {
    let theme = &ui.theme;
    // Register Connected Sites container BEFORE header buttons & site rows.
    ui.hitmap.add(area, HitTarget::DirectorySitesPane);
    if area.width < 10 || area.height < 3 {
        return;
    }

    let content_rect = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(1),
    );
    draw_horizontal_hairline(
        frame,
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        theme.subtle_style(),
    );
    if content_rect.width == 0 || content_rect.height < 2 {
        return;
    }

    let w = content_rect.width as usize;
    let enabled_count = sites.iter().filter(|s| s.enabled).count();
    let available_body = content_rect.height.saturating_sub(2) as usize;
    let max_site_rows = if available_body >= 2 {
        available_body.saturating_sub(1)
    } else {
        0
    };
    let (scroll, window) = visible_window(
        ui.directory_ui.selected_site,
        ui.directory_ui.site_scroll,
        max_site_rows,
        sites.len(),
    );
    ui.directory_ui.site_scroll = scroll;

    let mut lines = Vec::new();

    // Section title + scroll hint + clickable active scope badge
    let mut title = format!(" Connected Sites & Accounts ({enabled_count}/{})", sites.len());
    if max_site_rows > 0 && (window.start > 0 || window.end < sites.len()) {
        let up = if window.start > 0 { "▲" } else { "" };
        let down = if window.end < sites.len() { "▼" } else { "" };
        title.push_str(&format!(" {up}{down}"));
    }
    let scope_badge = format_scope_badge(active_scope_label, theme);
    let title_w = title.width();
    let badge_w = scope_badge.width();
    if w > title_w + badge_w + 2 {
        let badge_x = content_rect.x.saturating_add((title_w + 2) as u16);
        ui.hitmap.add_line(
            badge_x,
            content_rect.y,
            badge_w as u16,
            HitTarget::Button(ButtonId::SiteSwitcher),
        );
        lines.push(Line::from(vec![
            Span::styled(title, theme.title(true)),
            Span::raw("  "),
            scope_badge,
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            truncate_width(&title, w),
            theme.title(true),
        )));
    }

    if available_body >= 2 {
        let cols = ConnectedSitesColumns::for_width(w);
        lines.push(cols.header_line(theme));

        if sites.is_empty() {
            lines.push(Line::from(Span::styled(
                truncate_width("  No connected sites configured yet.", w),
                theme.muted_style(),
            )));
        } else {
            for (offset, idx) in window.enumerate() {
                let site = &sites[idx];
                lines.push(cols.row_line(site, theme));
                let row_y = content_rect.y + 2 + offset as u16;
                ui.hitmap.add_line(
                    content_rect.x,
                    row_y,
                    content_rect.width,
                    HitTarget::DirectorySiteRow(idx),
                );
            }
        }
    } else if available_body == 1 {
        let summary = if sites.is_empty() {
            "  No connected sites configured yet.".to_string()
        } else {
            let names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
            format!("  Sites: {}", names.join(", "))
        };
        lines.push(Line::from(Span::styled(
            truncate_width(&summary, w),
            theme.muted_style(),
        )));
    }

    let body_rect = Rect::new(
        content_rect.x,
        content_rect.y,
        content_rect.width,
        content_rect.height.saturating_sub(1),
    );
    frame.render_widget(Paragraph::new(lines), body_rect);

    let hint_y = content_rect.y + content_rect.height.saturating_sub(1);
    let switch_lbl = "Ctrl+S switch site";
    let switch_x = content_rect.x.saturating_add(1);
    if switch_x < content_rect.right() {
        let switch_w = (switch_lbl.width() as u16).min(content_rect.right().saturating_sub(switch_x));
        ui.hitmap.add_line(
            switch_x,
            hint_y,
            switch_w,
            HitTarget::Button(ButtonId::SiteSwitcher),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(
                " Ctrl+S switch site · atlassian sites add / atlassian login --oauth",
                w,
            ),
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        )),
        Rect::new(content_rect.x, hint_y, content_rect.width, 1),
    );
}

struct ConnectedSitesColumns {
    name_w: usize,
    url_w: usize,
    deploy_w: usize,
    auth_w: usize,
    state_w: usize,
    sync_w: usize,
}

impl ConnectedSitesColumns {
    fn for_width(total_w: usize) -> Self {
        // Reserve 3 chars for leading indent + active/enabled mark (` ✓ ` / `   `) and 1-space gaps.
        if total_w >= 104 {
            let fixed = 3 + 14 + 1 + 12 + 1 + 14 + 1 + 6 + 1 + 16;
            let extra = total_w.saturating_sub(fixed);
            let url_w = extra.clamp(20, 34);
            let sync_w = total_w.saturating_sub(3 + 14 + 1 + url_w + 1 + 12 + 1 + 14 + 1 + 6 + 1);
            Self {
                name_w: 14,
                url_w,
                deploy_w: 12,
                auth_w: 14,
                state_w: 6,
                sync_w: sync_w.max(10),
            }
        } else if total_w >= 76 {
            let url_w = total_w.saturating_sub(3 + 12 + 1 + 11 + 1 + 12 + 1 + 6 + 1 + 12).max(12);
            Self {
                name_w: 12,
                url_w,
                deploy_w: 11,
                auth_w: 12,
                state_w: 6,
                sync_w: 12,
            }
        } else {
            let sync_w = total_w.saturating_sub(3 + 12 + 1 + 11 + 1 + 12 + 1 + 6 + 1).max(6);
            Self {
                name_w: 12,
                url_w: 0,
                deploy_w: 11,
                auth_w: 12,
                state_w: 6,
                sync_w,
            }
        }
    }

    fn header_line(&self, theme: &crate::ui::theme::Theme) -> Line<'static> {
        let mut s = format!("   {:<nw$} ", "Name", nw = self.name_w);
        if self.url_w > 0 {
            s.push_str(&format!("{:<uw$} ", "URL", uw = self.url_w));
        }
        s.push_str(&format!(
            "{:<dw$} {:<aw$} {:<sw$} {:<yw$}",
            "Deployment",
            "Auth",
            "State",
            "Sync",
            dw = self.deploy_w,
            aw = self.auth_w,
            sw = self.state_w,
            yw = self.sync_w,
        ));
        let style = if theme.no_color {
            Style::default().add_modifier(Modifier::DIM | Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.fg_muted)
                .add_modifier(Modifier::BOLD)
        };
        Line::from(Span::styled(s, style))
    }

    fn row_line(&self, site: &ConnectedSiteRow, theme: &crate::ui::theme::Theme) -> Line<'static> {
        let base_style = if site.enabled {
            Style::default().fg(theme.fg)
        } else {
            theme.muted_style()
        };
        let active_mark = if site.is_active { " ✓ " } else { "   " };
        let active_style = if site.is_active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            theme.muted_style()
        };

        let name = pad_col(&site.name, self.name_w);
        let deploy = pad_col(&site.deployment_label, self.deploy_w);
        let auth = pad_col(&site.auth_label, self.auth_w);
        let state_raw = if site.enabled { "● On" } else { "○ Off" };
        let state = pad_col(state_raw, self.state_w);
        let sync = pad_col(&site.sync_status, self.sync_w);

        let state_style = if site.enabled {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.success)
            }
        } else {
            theme.muted_style()
        };

        let auth_style = if theme.no_color {
            base_style
        } else if site.auth_label.to_ascii_lowercase().contains("oauth") {
            Style::default().fg(theme.accent_soft)
        } else if site.auth_label.to_ascii_lowercase().contains("not signed") {
            Style::default().fg(theme.warning)
        } else {
            Style::default().fg(theme.fg_secondary)
        };

        let sync_style = if theme.no_color {
            base_style
        } else if site.sync_status.to_ascii_lowercase().contains("err")
            || site.sync_status.to_ascii_lowercase().contains("fail")
        {
            Style::default().fg(theme.error)
        } else if site.sync_status.to_ascii_lowercase().contains("syncing") {
            Style::default().fg(theme.accent_soft)
        } else {
            theme.muted_style()
        };

        let mut spans = vec![
            Span::styled(active_mark, active_style),
            Span::styled(
                format!("{name} "),
                if site.is_active {
                    theme.accent_style()
                } else {
                    base_style.add_modifier(Modifier::BOLD)
                },
            ),
        ];
        if self.url_w > 0 {
            let url = pad_col(site.display_url(), self.url_w);
            spans.push(Span::styled(format!("{url} "), theme.muted_style()));
        }
        spans.push(Span::styled(format!("{deploy} "), base_style));
        spans.push(Span::styled(format!("{auth} "), auth_style));
        spans.push(Span::styled(format!("{state} "), state_style));
        spans.push(Span::styled(sync, sync_style));
        Line::from(spans)
    }
}

fn pad_col(text: &str, width: usize) -> String {
    let fitted = truncate_width(text, width);
    let pad = width.saturating_sub(fitted.width());
    if pad == 0 {
        fitted
    } else {
        format!("{fitted}{}", " ".repeat(pad))
    }
}

fn draw_stats_and_controls(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    sites: &[ConnectedSiteRow],
) {
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
    let (site_label, deploy_label, auth_status) = match &app.site_scope {
        SiteScope::Only(site_id) => {
            if let Some(row) = sites.iter().find(|s| s.site_id.eq_ignore_ascii_case(site_id)) {
                let url = row.display_url();
                let lbl = if url.is_empty() || url.eq_ignore_ascii_case(&row.name) {
                    row.name.clone()
                } else {
                    format!("{} · {url}", row.name)
                };
                (lbl, row.deployment_label.clone(), row.auth_label.clone())
            } else if let Some(prof) = app.config.find_effective_site(site_id) {
                (
                    prof.display_name().to_string(),
                    prof.deployment.as_str().to_string(),
                    if app.auth.is_logged_in() {
                        "Authenticated".to_string()
                    } else {
                        "Not signed in".to_string()
                    },
                )
            } else {
                (
                    site_id.clone(),
                    app.config.deployment.as_str().to_string(),
                    if app.auth.is_logged_in() {
                        "Authenticated".to_string()
                    } else {
                        "Not signed in".to_string()
                    },
                )
            }
        }
        SiteScope::All => {
            if sites.len() > 1 {
                let enabled_n = sites.iter().filter(|s| s.enabled).count();
                let signed_in = sites
                    .iter()
                    .filter(|s| {
                        !s.auth_label.eq_ignore_ascii_case("Offline")
                            && !s.auth_label.eq_ignore_ascii_case("Not signed in")
                    })
                    .count();
                (
                    format!("All Sites ({enabled_n} enabled)"),
                    format!("{} configured", sites.len()),
                    format!("{signed_in}/{} authenticated", sites.len()),
                )
            } else if let Some(only) = sites.first() {
                let url = only.display_url();
                let lbl = if url.is_empty() {
                    only.name.clone()
                } else {
                    url.to_string()
                };
                (lbl, only.deployment_label.clone(), only.auth_label.clone())
            } else {
                (
                    site_display_english(app),
                    app.config.deployment.as_str().to_string(),
                    if app.auth.is_logged_in() {
                        "Authenticated".to_string()
                    } else {
                        "Not signed in".to_string()
                    },
                )
            }
        }
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

    // Row 1: clickable Site label opens SiteSwitcher
    let site_prefix = " Site: ";
    let site_x = content_rect.x.saturating_add(site_prefix.width() as u16);
    if site_x < content_rect.right() {
        let site_w = (site_label.width() as u16).min(content_rect.right().saturating_sub(site_x));
        ui.hitmap.add_line(
            site_x,
            content_rect.y + 1,
            site_w,
            HitTarget::Button(ButtonId::SiteSwitcher),
        );
    }

    // Row 3: Status | Version | Theme | Density | Images | Drawer (all 4 settings clickable)
    let status_prefix_1 = " Status: ";
    let status_prefix_2 = "  |  Version: ";
    let status_pipe = "  |  ";
    let theme_pill = format!("Theme: {}", app.config.theme.label());
    let density_pill = format!("Density: {}", app.config.density.label());
    let images_pill = format!("Images: {}", if ui.inline_images { "ON" } else { "OFF" });
    let drawer_pill = format!("Drawer: {}", if ui.auto_detail_drawer { "ON" } else { "OFF" });
    let status_sep = "  ";
    let status_row_y = content_rect.y + 3;

    let mut cx = content_rect
        .x
        .saturating_add(status_prefix_1.width() as u16)
        .saturating_add(sync_lbl.width() as u16)
        .saturating_add(status_prefix_2.width() as u16)
        .saturating_add(update_lbl.width() as u16)
        .saturating_add(status_pipe.width() as u16);
    let theme_w = theme_pill.width() as u16;
    if cx < content_rect.right() {
        let clipped = theme_w.min(content_rect.right().saturating_sub(cx));
        ui.hitmap.add_line(
            cx,
            status_row_y,
            clipped,
            HitTarget::Button(ButtonId::CycleTheme),
        );
    }
    cx = cx.saturating_add(theme_w).saturating_add(status_sep.width() as u16);

    let density_w = density_pill.width() as u16;
    if cx < content_rect.right() {
        let clipped = density_w.min(content_rect.right().saturating_sub(cx));
        ui.hitmap.add_line(
            cx,
            status_row_y,
            clipped,
            HitTarget::Button(ButtonId::ToggleDensity),
        );
    }
    cx = cx.saturating_add(density_w).saturating_add(status_sep.width() as u16);

    let img_w = images_pill.width() as u16;
    if cx < content_rect.right() {
        let clipped = img_w.min(content_rect.right().saturating_sub(cx));
        ui.hitmap.add_line(
            cx,
            status_row_y,
            clipped,
            HitTarget::Button(ButtonId::ToggleInlineImages),
        );
    }
    cx = cx.saturating_add(img_w).saturating_add(status_sep.width() as u16);

    let drawer_w = drawer_pill.width() as u16;
    if cx < content_rect.right() {
        let clipped = drawer_w.min(content_rect.right().saturating_sub(cx));
        ui.hitmap.add_line(
            cx,
            status_row_y,
            clipped,
            HitTarget::Button(ButtonId::ToggleDrawer),
        );
    }

    let sync_btn_text = " [r] Sync All ";
    let update_btn_text = " [U] Check Update ";
    let pin_btn_text = " [p] Toggle Pin ";
    let lines = vec![
        Line::from(Span::styled(
            " Sync · Cache Stats · Settings",
            theme.title(true),
        )),
        Line::from(vec![
            Span::styled(site_prefix, theme.muted_style()),
            Span::styled(site_label, theme.accent_style()),
            Span::styled(format!(" ({deploy_label})  |  Auth: "), theme.muted_style()),
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
            Span::styled(status_prefix_1, theme.muted_style()),
            Span::raw(sync_lbl),
            Span::styled(status_prefix_2, theme.muted_style()),
            Span::raw(update_lbl),
            Span::styled(status_pipe, theme.muted_style()),
            Span::styled(theme_pill, theme.muted_style()),
            Span::styled(status_sep, theme.muted_style()),
            Span::styled(density_pill, theme.muted_style()),
            Span::styled(status_sep, theme.muted_style()),
            Span::styled(images_pill, sync_btn_style),
            Span::styled(status_sep, theme.muted_style()),
            Span::styled(drawer_pill, update_btn_style),
        ]),
        Line::from(vec![
            Span::styled(sync_btn_text, sync_btn_style),
            Span::raw(" "),
            Span::styled(update_btn_text, update_btn_style),
            Span::raw(" "),
            Span::styled(pin_btn_text, pin_btn_style),
            Span::raw(" "),
            Span::styled(" [h/l] Switch List ", theme.muted_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).style(card_style), content_rect);

    if content_rect.height >= 5 {
        let row = content_rect.y + 4;
        let sync_w = sync_btn_text.width() as u16;
        let update_x = content_rect.x.saturating_add(sync_w + 1);
        let update_w = update_btn_text.width() as u16;
        let pin_x = update_x.saturating_add(update_w + 1);
        let pin_w = pin_btn_text.width() as u16;
        if content_rect.x < content_rect.right() {
            let clipped = sync_w.min(content_rect.right().saturating_sub(content_rect.x));
            ui.hitmap.add_line(
                content_rect.x,
                row,
                clipped,
                HitTarget::Button(ButtonId::SyncAll),
            );
        }
        if update_x < content_rect.right() {
            let clipped = update_w.min(content_rect.right().saturating_sub(update_x));
            ui.hitmap.add_line(
                update_x,
                row,
                clipped,
                HitTarget::Button(ButtonId::CheckUpdate),
            );
        }
        if pin_x < content_rect.right() {
            let clipped = pin_w.min(content_rect.right().saturating_sub(pin_x));
            ui.hitmap.add_line(
                pin_x,
                row,
                clipped,
                HitTarget::Button(ButtonId::TogglePin),
            );
        }
    }
}

fn draw_projects(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let theme = &ui.theme;
    let active = ui.directory_ui.section == 0;
    ui.hitmap.add(area, HitTarget::DirectoryProjectsPane);
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
    ui.hitmap.add(area, HitTarget::DirectorySpacesPane);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona, ThemePreset},
        models::{ProjectSummary, SpaceSummary},
        storage::Storage,
        sync,
        ui::theme::Theme,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn test_app() -> (AppState, UiState) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            deployment: DeploymentKind::Cloud,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::ApiToken {
                site_url: "https://acme.atlassian.net".into(),
                email: "dev@acme.io".into(),
                api_token: "secret".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        app.projects = vec![ProjectSummary {
            id: "100".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            project_type: "software".into(),
            pinned: true,
            ..Default::default()
        }];
        app.spaces = vec![SpaceSummary {
            id: "200".into(),
            key: "DOCS".into(),
            name: "Documentation".into(),
            space_type: "global".into(),
            ..Default::default()
        }];
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[tokio::test]
    async fn directory_renders_connected_sites_table_and_preserves_projects_and_spaces() {
        let (app, mut ui) = test_app();
        let sites = vec![
            ConnectedSiteRow {
                site_id: "acme-cloud".into(),
                name: "acme".into(),
                site_url: "https://acme.atlassian.net".into(),
                deployment_label: "Cloud".into(),
                auth_label: "Cloud OAuth".into(),
                enabled: true,
                is_active: true,
                sync_status: "Synced 2m ago".into(),
            },
            ConnectedSiteRow {
                site_id: "corp-dc".into(),
                name: "corp-dc".into(),
                site_url: "https://jira.corp.internal".into(),
                deployment_label: "Data Center".into(),
                auth_label: "DC PAT".into(),
                enabled: false,
                is_active: false,
                sync_status: "Paused".into(),
            },
        ];

        let backend = TestBackend::new(124, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_with_sites(f, f.area(), &app, &mut ui, &sites, None))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            text.contains("Connected Sites & Accounts (1/2)"),
            "missing section header:\n{text}"
        );
        assert!(text.contains("acme.atlassian.net"), "missing URL column:\n{text}");
        assert!(text.contains("Cloud OAuth"), "missing Auth column:\n{text}");
        assert!(text.contains("Data Center"), "missing Deployment column:\n{text}");
        assert!(text.contains("● On"), "missing enabled state:\n{text}");
        assert!(text.contains("○ Off"), "missing disabled state:\n{text}");
        assert!(text.contains("Synced 2m ago"), "missing sync status:\n{text}");
        assert!(
            text.contains("Ctrl+S switch site") && text.contains("atlassian login --oauth"),
            "missing site management hint:\n{text}"
        );
        assert!(text.contains("Jira Projects (1)"), "missing projects:\n{text}");
        assert!(text.contains("Confluence Spaces (1)"), "missing spaces:\n{text}");

        // Narrow viewport + NoColor preset renders cleanly without panic
        ui.theme = Theme::detect(ThemePreset::NoColor);
        let narrow_backend = TestBackend::new(66, 22);
        let mut narrow_term = Terminal::new(narrow_backend).unwrap();
        narrow_term
            .draw(|f| draw(f, f.area(), &app, &mut ui))
            .unwrap();
        let narrow_text: String = narrow_term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(narrow_text.contains("Connected Sites & Accounts"));
        assert!(narrow_text.contains("Jira Projects (1)"));
    }
}
