//! Site Switcher modal (`Ctrl+S`) for multi-site Atlassian federation.
//!
//! Displays a filterable list with a top `All Sites (Federated)` aggregate row followed by each
//! connected Cloud or Data Center site, minimal-chrome auth/deployment badges, enabled toggles,
//! active checkmark, keyboard shortcuts, and `HitTarget::OverlayItem` mouse support.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::DeploymentKind,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_block, modal_pill_secondary, read_clipboard_text,
            register_modal_chrome, render_button_bar, sanitize_single_line, OverlayOutcome,
        },
        text_edit::TextBuffer,
        theme::Theme,
    },
};

/// A connected Atlassian site entry displayed inside `SiteSwitcherOverlay`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteSwitcherEntry {
    pub site_id: String,
    pub name: String,
    pub site_url: String,
    pub deployment_label: String,
    pub auth_label: String,
    pub enabled: bool,
    pub is_active: bool,
    pub issue_count: usize,
    pub page_count: usize,
}

impl SiteSwitcherEntry {
    /// Returns a compact, scheme-free display host/URL (e.g. `acme.atlassian.net`).
    pub fn display_host(&self) -> &str {
        self.site_url
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
    }

    /// Resolves the minimal-chrome badge label (`Cloud OAuth`, `Cloud Token`, `Data Center`, etc.).
    pub fn badge_label(&self) -> String {
        normalize_site_badge(&self.deployment_label, &self.auth_label)
    }
}

/// High-level command emitted by `SiteSwitcherOverlay` for clean coordinator dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteSwitcherCommand {
    SelectAllSites,
    SelectSite(String),
    ToggleSiteEnabled(String),
    DiscoverCloudSites,
    SyncAllSites,
}

/// Visible row kind inside the filtered site switcher list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteSwitcherRow {
    AllSites {
        enabled_sites: usize,
        total_sites: usize,
        total_issues: usize,
        total_pages: usize,
        is_active: bool,
    },
    Site(usize),
}

#[derive(Debug, Clone, Default)]
pub struct SiteSwitcherOverlay {
    pub entries: Vec<SiteSwitcherEntry>,
    pub query: TextBuffer,
    pub selected: usize,
    /// When true, `d` and `r` are treated as filter text instead of quick actions.
    pub filter_mode: bool,
    /// Last command triggered on activation/shortcut for coordinator dispatch.
    pub pending_command: Option<SiteSwitcherCommand>,
}

/// Alias aligned with other overlay state naming conventions.
pub type SiteSwitcherState = SiteSwitcherOverlay;

impl SiteSwitcherOverlay {
    pub fn new(entries: Vec<SiteSwitcherEntry>) -> Self {
        Self {
            entries,
            query: TextBuffer::new(),
            selected: 0,
            filter_mode: false,
            pending_command: None,
        }
    }

    /// Builds a fallback single-site switcher overlay from legacy `AppState` configuration.
    pub fn from_app(app: &AppState) -> Self {
        let mut entries = Vec::new();
        let raw_url = app.config.site_url.trim();
        if !raw_url.is_empty() {
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
            let auth_label = if app.auth.is_logged_in() {
                match app.config.deployment {
                    DeploymentKind::Cloud => "Cloud Token".to_string(),
                    DeploymentKind::DataCenter => "Data Center".to_string(),
                }
            } else {
                "Offline".to_string()
            };
            entries.push(SiteSwitcherEntry {
                site_id: "default".to_string(),
                name,
                site_url: raw_url.to_string(),
                deployment_label,
                auth_label,
                enabled: true,
                is_active: false,
                issue_count: app.cache_counts.issues,
                page_count: app.cache_counts.pages,
            });
        }
        Self::new(entries)
    }

    /// Returns whether `All Sites (Federated)` is the currently active scope.
    pub fn is_all_sites_active(&self) -> bool {
        !self.entries.iter().any(|e| e.is_active)
    }

    /// Aggregate `(enabled_sites, total_issues, total_pages)` across all enabled sites.
    pub fn aggregate_enabled_counts(&self) -> (usize, usize, usize) {
        self.entries
            .iter()
            .filter(|e| e.enabled)
            .fold((0usize, 0usize, 0usize), |(sites, issues, pages), entry| {
                (
                    sites + 1,
                    issues + entry.issue_count,
                    pages + entry.page_count,
                )
            })
    }

    /// Returns the filtered visible rows (with `All Sites (Federated)` at index 0 when matching).
    pub fn visible_rows(&self) -> Vec<SiteSwitcherRow> {
        let q = self.query.text.trim().to_ascii_lowercase();
        let mut rows = Vec::with_capacity(self.entries.len() + 1);

        let (enabled_sites, total_issues, total_pages) = self.aggregate_enabled_counts();
        let all_row = SiteSwitcherRow::AllSites {
            enabled_sites,
            total_sites: self.entries.len(),
            total_issues,
            total_pages,
            is_active: self.is_all_sites_active(),
        };
        if q.is_empty() || "all sites (federated)".contains(&q) {
            rows.push(all_row);
        }

        for (idx, entry) in self.entries.iter().enumerate() {
            if q.is_empty() || entry_matches_query(entry, &q) {
                rows.push(SiteSwitcherRow::Site(idx));
            }
        }
        rows
    }

    /// Selects a visible row by index (e.g. from mouse hitmap clicks).
    pub fn select(&mut self, index: usize) {
        let len = self.visible_rows().len();
        if index < len {
            self.selected = index;
        }
    }

    /// Takes and clears the pending coordinator command, if any.
    pub fn take_pending_command(&mut self) -> Option<SiteSwitcherCommand> {
        self.pending_command.take()
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the filter query.
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        self.query.insert_str(&cleaned);
        self.filter_mode = true;
        self.selected = 0;
        true
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn activate_selected(&mut self) -> OverlayOutcome {
        let rows = self.visible_rows();
        let Some(row) = rows.get(self.selected) else {
            return OverlayOutcome::Close;
        };
        match row {
            SiteSwitcherRow::AllSites { .. } => {
                for entry in &mut self.entries {
                    entry.is_active = false;
                }
                self.pending_command = Some(SiteSwitcherCommand::SelectAllSites);
                OverlayOutcome::Close
            }
            SiteSwitcherRow::Site(idx) => {
                let Some(site_id) = self.entries.get(*idx).map(|e| e.site_id.clone()) else {
                    return OverlayOutcome::Close;
                };
                for (i, entry) in self.entries.iter_mut().enumerate() {
                    entry.is_active = i == *idx;
                }
                self.pending_command = Some(SiteSwitcherCommand::SelectSite(site_id));
                OverlayOutcome::Close
            }
        }
    }

    /// Selects a visible row by index and toggles its enabled flag without closing.
    pub fn toggle_item(&mut self, idx: usize) -> OverlayOutcome {
        let len = self.visible_rows().len();
        if idx < len {
            self.selected = idx;
            return self.toggle_selected_enabled();
        }
        OverlayOutcome::Handled
    }

    fn toggle_selected_enabled(&mut self) -> OverlayOutcome {
        let rows = self.visible_rows();
        if let Some(SiteSwitcherRow::Site(idx)) = rows.get(self.selected) {
            if let Some(entry) = self.entries.get_mut(*idx) {
                entry.enabled = !entry.enabled;
                self.pending_command =
                    Some(SiteSwitcherCommand::ToggleSiteEnabled(entry.site_id.clone()));
            }
        }
        OverlayOutcome::Handled
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let row_count = self.visible_rows().len();
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::CursorUp | Action::MoveUp | Action::PrevField => {
                if row_count > 0 {
                    self.selected = (self.selected + row_count - 1) % row_count;
                }
                OverlayOutcome::Handled
            }
            Action::CursorDown | Action::MoveDown | Action::NextField => {
                if row_count > 0 {
                    self.selected = (self.selected + 1) % row_count;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.query.move_left();
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.query.move_right();
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.query.move_home();
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.query.move_end();
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => self.activate_selected(),
            Action::ExpandOrToggle => self.toggle_selected_enabled(),
            Action::SyncAll => {
                self.pending_command = Some(SiteSwitcherCommand::SyncAllSites);
                OverlayOutcome::Close
            }
            Action::DiscoverCloudSites | Action::DownloadAttachment => {
                self.pending_command = Some(SiteSwitcherCommand::DiscoverCloudSites);
                OverlayOutcome::Close
            }
            Action::Input(' ') => self.toggle_selected_enabled(),
            Action::Input('d') if self.query.text.is_empty() && !self.filter_mode => {
                self.pending_command = Some(SiteSwitcherCommand::DiscoverCloudSites);
                OverlayOutcome::Close
            }
            Action::Input('r') if self.query.text.is_empty() && !self.filter_mode => {
                self.pending_command = Some(SiteSwitcherCommand::SyncAllSites);
                OverlayOutcome::Close
            }
            Action::Input('/') if self.query.text.is_empty() && !self.filter_mode => {
                self.filter_mode = true;
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.query.insert_char(*ch);
                self.filter_mode = true;
                self.selected = 0;
                self.clamp_selection();
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.backspace();
                if self.query.text.is_empty() {
                    self.filter_mode = false;
                }
                self.selected = 0;
                self.clamp_selection();
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.query.delete();
                if self.query.text.is_empty() {
                    self.filter_mode = false;
                }
                self.selected = 0;
                self.clamp_selection();
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                let cleaned = sanitize_single_line(s);
                if !cleaned.is_empty() {
                    self.query.insert_str(&cleaned);
                    self.filter_mode = true;
                    self.selected = 0;
                    self.clamp_selection();
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rows = self.visible_rows();
        let desired_h = (rows.len().max(2) as u16 + 6)
            .clamp(10, 22)
            .min(area.height.saturating_sub(2))
            .max(8);
        let desired_w = 82.min(area.width.saturating_sub(4)).max(34);
        let rect = centered_rect(area, desired_w, desired_h);
        frame.render_widget(Clear, rect);

        let block = modal_block(" Switch Site (Ctrl+S) ", theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 4 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        // Filter bar + divider hairline
        let mut query_spans = vec![Span::styled(" > ", theme.accent_style())];
        if self.query.text.is_empty() {
            query_spans.push(Span::styled(
                truncate_width(
                    "Type to filter sites (name, URL, Cloud/DC)…",
                    chunks[0].width.saturating_sub(4) as usize,
                ),
                theme.muted_style(),
            ));
        } else {
            query_spans.push(Span::raw(self.query.text.as_str()));
            query_spans.push(Span::styled("▏", theme.accent_style()));
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(query_spans),
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let max_rows = chunks[1].height as usize;
        let sel = self.selected.min(rows.len().saturating_sub(1));
        let start = if sel >= max_rows && max_rows > 0 {
            sel - max_rows + 1
        } else {
            0
        };

        let mut lines = Vec::new();
        if rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No matching sites",
                theme.muted_style(),
            )));
        } else {
            let row_w = chunks[1].width as usize;
            for (vis_idx, row) in rows.iter().skip(start).take(max_rows).enumerate() {
                let abs_idx = start + vis_idx;
                let is_sel = abs_idx == sel;
                let line = match row {
                    SiteSwitcherRow::AllSites {
                        enabled_sites,
                        total_sites,
                        total_issues,
                        total_pages,
                        is_active,
                    } => render_all_sites_row(
                        *enabled_sites,
                        *total_sites,
                        *total_issues,
                        *total_pages,
                        *is_active,
                        is_sel,
                        row_w,
                        theme,
                    ),
                    SiteSwitcherRow::Site(entry_idx) => {
                        render_site_row(&self.entries[*entry_idx], is_sel, row_w, theme)
                    }
                };
                lines.push(line);
                let row_y = chunks[1].y + vis_idx as u16;
                hitmap.add_line(
                    chunks[1].x,
                    row_y,
                    chunks[1].width,
                    HitTarget::OverlayItem(abs_idx),
                );
                if matches!(row, SiteSwitcherRow::Site(_)) {
                    let toggle_w = 6u16.min(chunks[1].width);
                    hitmap.add_line(
                        chunks[1].x,
                        row_y,
                        toggle_w,
                        HitTarget::OverlayItemToggle(abs_idx),
                    );
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);

        render_button_bar(
            frame,
            chunks[2],
            &[
                ("[d] Discover", ButtonId::OverlayDiscoverSites, modal_pill_secondary(theme)),
                ("[r] Sync All", ButtonId::SyncAll, modal_pill_secondary(theme)),
            ],
            Some("· Enter select · Space toggle enabled · Esc close"),
            theme,
            hitmap,
        );
    }
}

fn entry_matches_query(entry: &SiteSwitcherEntry, q: &str) -> bool {
    entry.name.to_ascii_lowercase().contains(q)
        || entry.site_id.to_ascii_lowercase().contains(q)
        || entry.site_url.to_ascii_lowercase().contains(q)
        || entry.deployment_label.to_ascii_lowercase().contains(q)
        || entry.auth_label.to_ascii_lowercase().contains(q)
        || entry.badge_label().to_ascii_lowercase().contains(q)
}

/// Normalizes deployment + auth strings into minimal-chrome badge labels
/// (`Cloud OAuth`, `Cloud Token`, `Data Center`).
pub fn normalize_site_badge(deployment_label: &str, auth_label: &str) -> String {
    let dep = deployment_label.trim();
    let auth = auth_label.trim();
    let dep_l = dep.to_ascii_lowercase();
    let auth_l = auth.to_ascii_lowercase();

    if dep_l.contains("data center")
        || dep_l.contains("datacenter")
        || dep_l == "dc"
        || dep_l.contains("server")
        || auth_l.contains("data center")
        || auth_l.contains("datacenter")
    {
        return "Data Center".to_string();
    }
    if auth_l.contains("oauth") {
        return "Cloud OAuth".to_string();
    }
    if auth_l.contains("token") || auth_l.contains("basic") || auth_l.contains("api") {
        return "Cloud Token".to_string();
    }
    if !auth.is_empty() && !dep.is_empty() {
        if auth_l.starts_with(&dep_l) {
            return auth.to_string();
        }
        return format!("{dep} {auth}");
    }
    if !auth.is_empty() {
        auth.to_string()
    } else if !dep.is_empty() {
        dep.to_string()
    } else {
        "Cloud".to_string()
    }
}

/// Renders a minimal-chrome badge span for `Cloud OAuth`, `Cloud Token`, or `Data Center`.
pub fn format_site_badge_span(badge_label: &str, theme: &Theme) -> Span<'static> {
    let text = format!(" {badge_label} ");
    if theme.no_color {
        return Span::styled(
            format!("[{badge_label}]"),
            Style::default().add_modifier(Modifier::BOLD),
        );
    }
    let lower = badge_label.to_ascii_lowercase();
    let style = if lower.contains("oauth") {
        Style::default()
            .bg(theme.chip_bg)
            .fg(theme.chip_fg)
            .add_modifier(Modifier::BOLD)
    } else if lower.contains("data center") || lower.contains("datacenter") {
        Style::default()
            .bg(theme.bg_elevated)
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.wiki_chip_bg)
            .fg(theme.confluence_teal)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(text, style)
}

#[allow(clippy::too_many_arguments)]
fn render_all_sites_row(
    enabled_sites: usize,
    total_sites: usize,
    total_issues: usize,
    total_pages: usize,
    is_active: bool,
    is_sel: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let row_style = if is_sel {
        theme.selection()
    } else {
        Style::default().fg(theme.fg)
    };
    let meta_style = if is_sel {
        theme.selection().remove_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };
    let check_style = if theme.no_color {
        row_style.add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.success)
            .add_modifier(Modifier::BOLD)
            .patch(row_style)
    };

    let cursor = if is_sel { "▸ " } else { "  " };
    let check = if is_active { "✓ " } else { "  " };
    let dot = if enabled_sites > 0 { "● " } else { "○ " };
    let dot_style = if enabled_sites > 0 {
        if theme.no_color {
            row_style
        } else {
            Style::default().fg(theme.success).patch(row_style)
        }
    } else {
        meta_style
    };

    let title = "All Sites (Federated)";
    let counts = if total_sites == enabled_sites {
        format!("{enabled_sites} sites · {total_issues} issues · {total_pages} pages")
    } else {
        format!("{enabled_sites}/{total_sites} enabled · {total_issues} issues · {total_pages} pages")
    };

    let prefix_w = cursor.width() + check.width() + dot.width();
    let counts_text = if width > prefix_w + title.width() + 8 {
        truncate_width(&counts, width.saturating_sub(prefix_w + title.width() + 3))
    } else {
        String::new()
    };
    let title_budget = width.saturating_sub(prefix_w + counts_text.width() + 2);
    let title_text = truncate_width(title, title_budget);
    let pad = width.saturating_sub(prefix_w + title_text.width() + counts_text.width() + 1);

    let mut spans = vec![
        Span::styled(cursor, row_style),
        Span::styled(check, check_style),
        Span::styled(dot, dot_style),
        Span::styled(title_text, row_style.add_modifier(Modifier::BOLD)),
        Span::styled(" ".repeat(pad), row_style),
    ];
    if !counts_text.is_empty() {
        spans.push(Span::styled(counts_text, meta_style));
        spans.push(Span::styled(" ", row_style));
    }
    Line::from(spans)
}

fn render_site_row(
    entry: &SiteSwitcherEntry,
    is_sel: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let row_style = if is_sel {
        theme.selection()
    } else if entry.enabled {
        Style::default().fg(theme.fg)
    } else {
        theme.muted_style()
    };
    let meta_style = if is_sel {
        theme.selection().remove_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };
    let check_style = if theme.no_color {
        row_style.add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.success)
            .add_modifier(Modifier::BOLD)
            .patch(row_style)
    };
    let dot_style = if entry.enabled {
        if theme.no_color {
            row_style
        } else {
            Style::default().fg(theme.success).patch(row_style)
        }
    } else {
        meta_style
    };

    let cursor = if is_sel { "▸ " } else { "  " };
    let check = if entry.is_active { "✓ " } else { "  " };
    let dot = if entry.enabled { "● " } else { "○ " };

    let badge_label = entry.badge_label();
    let badge_span = format_site_badge_span(&badge_label, theme);
    let badge_w = badge_span.width();

    let counts = if width >= 72 {
        format!("{}i · {}p", entry.issue_count, entry.page_count)
    } else {
        String::new()
    };
    let counts_w = if counts.is_empty() {
        0
    } else {
        counts.width() + 2
    };

    let prefix_w = cursor.width() + check.width() + dot.width();
    let right_w = badge_w + counts_w + 1;
    let left_budget = width.saturating_sub(prefix_w + right_w + 1);

    let host = entry.display_host();
    let name_and_host = if !host.is_empty() && left_budget > entry.name.width() + 8 {
        let host_budget = left_budget.saturating_sub(entry.name.width() + 2);
        let short_host = truncate_width(host, host_budget);
        (entry.name.clone(), format!("  {short_host}"))
    } else {
        (truncate_width(&entry.name, left_budget), String::new())
    };

    let used_left = prefix_w + name_and_host.0.width() + name_and_host.1.width();
    let pad = width.saturating_sub(used_left + right_w);

    let mut spans = vec![
        Span::styled(cursor, row_style),
        Span::styled(check, check_style),
        Span::styled(dot, dot_style),
        Span::styled(
            name_and_host.0,
            if entry.is_active || is_sel {
                row_style.add_modifier(Modifier::BOLD)
            } else {
                row_style
            },
        ),
    ];
    if !name_and_host.1.is_empty() {
        spans.push(Span::styled(name_and_host.1, meta_style));
    }
    spans.push(Span::styled(" ".repeat(pad.max(1)), row_style));
    spans.push(badge_span);
    if !counts.is_empty() {
        spans.push(Span::styled(format!("  {counts}"), meta_style));
    }
    spans.push(Span::styled(" ", row_style));

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona, ThemePreset},
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn sample_entries() -> Vec<SiteSwitcherEntry> {
        vec![
            SiteSwitcherEntry {
                site_id: "acme-cloud".into(),
                name: "acme".into(),
                site_url: "https://acme.atlassian.net".into(),
                deployment_label: "Cloud".into(),
                auth_label: "OAuth".into(),
                enabled: true,
                is_active: false,
                issue_count: 14,
                page_count: 9,
            },
            SiteSwitcherEntry {
                site_id: "partner-cloud".into(),
                name: "partner".into(),
                site_url: "https://partner.atlassian.net".into(),
                deployment_label: "Cloud".into(),
                auth_label: "API Token".into(),
                enabled: true,
                is_active: false,
                issue_count: 6,
                page_count: 4,
            },
            SiteSwitcherEntry {
                site_id: "corp-dc".into(),
                name: "corp-dc".into(),
                site_url: "https://jira.corp.internal".into(),
                deployment_label: "Data Center".into(),
                auth_label: "PAT".into(),
                enabled: false,
                is_active: false,
                issue_count: 25,
                page_count: 12,
            },
        ]
    }

    fn test_app() -> (AppState, UiState) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            deployment: DeploymentKind::Cloud,
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

    fn render_screen(ov: &SiteSwitcherOverlay, app: &AppState, ui: &UiState, w: u16, h: u16) -> (String, HitMap) {
        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| ov.draw(f, f.area(), app, ui, &mut hitmap))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        (text, hitmap)
    }

    #[test]
    fn aggregate_counts_only_include_enabled_sites() {
        let ov = SiteSwitcherOverlay::new(sample_entries());
        assert!(ov.is_all_sites_active());
        assert_eq!(ov.aggregate_enabled_counts(), (2, 20, 13));
        assert_eq!(ov.visible_rows().len(), 4);
    }

    #[test]
    fn badge_normalization_maps_cloud_oauth_token_and_data_center() {
        assert_eq!(normalize_site_badge("Cloud", "OAuth 2.0"), "Cloud OAuth");
        assert_eq!(normalize_site_badge("Cloud", "API Token"), "Cloud Token");
        assert_eq!(normalize_site_badge("Data Center", "PAT"), "Data Center");
        assert_eq!(normalize_site_badge("datacenter", "Basic"), "Data Center");
    }

    #[test]
    fn keyboard_navigation_selection_toggle_and_quick_actions_work() {
        let mut ov = SiteSwitcherOverlay::new(sample_entries());
        assert_eq!(ov.selected, 0);

        // MoveDown wraps through all 4 rows
        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected, 1);

        // Space toggles enabled on selected site (acme-cloud)
        assert_eq!(ov.handle(&Action::Input(' ')), OverlayOutcome::Handled);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::ToggleSiteEnabled("acme-cloud".into()))
        );
        assert!(!ov.entries[0].enabled);
        assert_eq!(ov.aggregate_enabled_counts(), (1, 6, 4));

        // Space again re-enables acme-cloud
        ov.handle(&Action::Input(' '));
        assert!(ov.entries[0].enabled);
        let _ = ov.take_pending_command();

        // Enter selects acme-cloud and closes
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::SelectSite("acme-cloud".into()))
        );
        assert!(ov.entries[0].is_active);
        assert!(!ov.is_all_sites_active());

        // MoveUp to All Sites and press Enter
        ov.handle(&Action::MoveUp);
        assert_eq!(ov.selected, 0);
        assert_eq!(ov.handle(&Action::Submit), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::SelectAllSites)
        );
        assert!(ov.is_all_sites_active());

        // Quick actions d / r when query is empty
        assert_eq!(ov.handle(&Action::Input('d')), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::DiscoverCloudSites)
        );

        assert_eq!(ov.handle(&Action::Input('r')), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::SyncAllSites)
        );

        // Esc closes cleanly
        assert_eq!(ov.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[test]
    fn type_to_filter_narrows_rows_and_preserves_selection_commands() {
        let mut ov = SiteSwitcherOverlay::new(sample_entries());
        for ch in "corp-dc".chars() {
            ov.handle(&Action::Input(ch));
        }
        assert_eq!(ov.query.text, "corp-dc");
        let rows = ov.visible_rows();
        assert_eq!(rows, vec![SiteSwitcherRow::Site(2)]);

        // Space still toggles enabled on the filtered site
        ov.handle(&Action::Input(' '));
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::ToggleSiteEnabled("corp-dc".into()))
        );
        assert!(ov.entries[2].enabled);

        // Enter selects the filtered site
        assert_eq!(ov.handle(&Action::Submit), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(SiteSwitcherCommand::SelectSite("corp-dc".into()))
        );
    }

    #[tokio::test]
    async fn site_switcher_renders_all_badges_indicators_footer_and_hitmap() {
        let (app, mut ui) = test_app();
        let mut entries = sample_entries();
        entries[0].is_active = true;
        let ov = SiteSwitcherOverlay::new(entries);

        let (text, hitmap) = render_screen(&ov, &app, &ui, 100, 26);
        assert!(text.contains("Switch Site (Ctrl+S)"), "missing title:\n{text}");
        assert!(
            text.contains("All Sites (Federated)"),
            "missing All Sites row:\n{text}"
        );
        assert!(text.contains("Cloud OAuth"), "missing Cloud OAuth badge:\n{text}");
        assert!(text.contains("Cloud Token"), "missing Cloud Token badge:\n{text}");
        assert!(text.contains("Data Center"), "missing Data Center badge:\n{text}");
        assert!(text.contains('●'), "missing enabled dot:\n{text}");
        assert!(text.contains('○'), "missing disabled dot:\n{text}");
        assert!(text.contains('✓'), "missing active checkmark:\n{text}");
        assert!(
            text.contains("Enter select") && text.contains("Space toggle enabled"),
            "missing footer actions:\n{text}"
        );
        assert!(hitmap.region_of(HitTarget::OverlayBody).is_some());
        assert!(hitmap.region_of(HitTarget::OverlayClose).is_some());
        assert!(hitmap.region_of(HitTarget::OverlayItem(0)).is_some());
        assert!(hitmap.region_of(HitTarget::OverlayItem(3)).is_some());
        assert!(hitmap.region_of(HitTarget::OverlayItemToggle(1)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::OverlayDiscoverSites)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::SyncAll)).is_some());

        // Narrow viewport + NoColor preset should render cleanly without panic
        ui.theme = Theme::detect(ThemePreset::NoColor);
        let (narrow_text, narrow_hits) = render_screen(&ov, &app, &ui, 44, 14);
        assert!(narrow_text.contains("All Sites"));
        assert!(!narrow_hits.is_empty());
    }
}
