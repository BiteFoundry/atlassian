//! Live Settings modal (`Ctrl+,`).
//! Displays and coordinates `crate::config::AppConfig` (`config.toml`) and TUI preferences.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::{self, DeploymentKind},
    img_preview::{self, TerminalGraphicsProtocol},
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{centered_rect, modal_block, register_modal_chrome, OverlayOutcome},
    },
};

pub const SETTINGS_COUNT: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    Notifications = 0,
    LeftSidebar = 1,
    InlineImages = 2,
    ImageProtocol = 3,
    ViewDensity = 4,
    ColorTheme = 5,
    DownloadFolder = 6,
    KeyboardShortcuts = 7,
    SiteUrl = 8,
    DeploymentKind = 9,
    ConfigFile = 10,
}

impl SettingsItem {
    pub const ALL: [Self; SETTINGS_COUNT] = [
        Self::Notifications,
        Self::LeftSidebar,
        Self::InlineImages,
        Self::ImageProtocol,
        Self::ViewDensity,
        Self::ColorTheme,
        Self::DownloadFolder,
        Self::KeyboardShortcuts,
        Self::SiteUrl,
        Self::DeploymentKind,
        Self::ConfigFile,
    ];

    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(Self::ConfigFile)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Notifications => "Notifications",
            Self::LeftSidebar => "Left sidebar",
            Self::InlineImages => "Inline image previews",
            Self::ImageProtocol => "Image protocol",
            Self::ViewDensity => "View density",
            Self::ColorTheme => "Color theme",
            Self::DownloadFolder => "Download folder",
            Self::KeyboardShortcuts => "Keyboard shortcuts",
            Self::SiteUrl => "Site URL",
            Self::DeploymentKind => "Deployment kind",
            Self::ConfigFile => "Config file path",
        }
    }
}

/// High-level command emitted when activating a row in `SettingsOverlay`.
/// Coordinator wiring in `action.rs` can match this enum to persist settings into `AppConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCommand {
    ToggleNotifications,
    ToggleLeftSidebar,
    ToggleInlineImages,
    CycleImageProtocol,
    ToggleViewDensity,
    CycleColorTheme,
    ShowDownloadFolder,
    OpenKeyboardShortcuts,
    ShowSiteUrl,
    ShowDeploymentKind,
    ShowConfigFilePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRow {
    pub item: SettingsItem,
    pub label: &'static str,
    pub value: String,
    pub is_toggle_on: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct SettingsOverlay {
    pub selected: usize,
    /// Optional display override for image protocol (e.g. set by coordinator after cycling).
    pub image_protocol_label: Option<String>,
    /// Optional display override for view density (e.g. `"Cozy"` / `"Compact"`).
    pub density_label: String,
    /// Optional display override for active color theme preset (e.g. `"Atlassian Dark"`).
    pub theme_label: String,
    /// Last command triggered on activation for coordinator inspection.
    pub pending_command: Option<SettingsCommand>,
}

/// Alias aligned with `teams` / `outlook` overlay naming.
pub type SettingsState = SettingsOverlay;

impl Default for SettingsOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsOverlay {
    pub fn new() -> Self {
        Self {
            selected: 0,
            image_protocol_label: None,
            density_label: "Cozy".to_string(),
            theme_label: "Atlassian Dark".to_string(),
            pending_command: None,
        }
    }

    pub fn with_display_labels(
        mut self,
        protocol: impl Into<String>,
        density: impl Into<String>,
        theme: impl Into<String>,
    ) -> Self {
        self.image_protocol_label = Some(protocol.into());
        self.density_label = density.into();
        self.theme_label = theme.into();
        self
    }

    pub fn from_config(app: &AppState, ui: &UiState) -> Self {
        let mut ov = Self::new();
        ov.sync_from_caps(app, ui.graphics_caps);
        ov
    }

    pub fn sync_from_caps(
        &mut self,
        app: &AppState,
        graphics_caps: img_preview::TerminalGraphicsCaps,
    ) {
        let proto = match app.config.image_protocol {
            config::ImageProtocolPreference::Auto => {
                format!("Auto ({})", graphics_caps.protocol.label())
            }
            explicit => explicit.label().to_string(),
        };
        self.image_protocol_label = Some(proto);
        self.density_label = app.config.density.label().to_string();
        self.theme_label = app.config.theme.label().to_string();
    }

    pub fn select(&mut self, index: usize) {
        if index < SETTINGS_COUNT {
            self.selected = index;
        }
    }

    pub fn selected_item(&self) -> SettingsItem {
        SettingsItem::from_index(self.selected)
    }

    pub fn command_for(item: SettingsItem) -> SettingsCommand {
        match item {
            SettingsItem::Notifications => SettingsCommand::ToggleNotifications,
            SettingsItem::LeftSidebar => SettingsCommand::ToggleLeftSidebar,
            SettingsItem::InlineImages => SettingsCommand::ToggleInlineImages,
            SettingsItem::ImageProtocol => SettingsCommand::CycleImageProtocol,
            SettingsItem::ViewDensity => SettingsCommand::ToggleViewDensity,
            SettingsItem::ColorTheme => SettingsCommand::CycleColorTheme,
            SettingsItem::DownloadFolder => SettingsCommand::ShowDownloadFolder,
            SettingsItem::KeyboardShortcuts => SettingsCommand::OpenKeyboardShortcuts,
            SettingsItem::SiteUrl => SettingsCommand::ShowSiteUrl,
            SettingsItem::DeploymentKind => SettingsCommand::ShowDeploymentKind,
            SettingsItem::ConfigFile => SettingsCommand::ShowConfigFilePath,
        }
    }

    pub fn selected_command(&self) -> SettingsCommand {
        Self::command_for(self.selected_item())
    }

    pub fn rows(&self, app: &AppState, ui: &UiState) -> Vec<SettingsRow> {
        let protocol_value = self
            .image_protocol_label
            .clone()
            .unwrap_or_else(|| protocol_label(img_preview::detect_stdout_protocol()).to_string());
        let site_value = if app.config.site_url.trim().is_empty() {
            "(not configured)".to_string()
        } else {
            app.config.site_url.clone()
        };
        let deployment_value = match app.config.deployment {
            DeploymentKind::Cloud => "Cloud".to_string(),
            DeploymentKind::DataCenter => "Data Center / Server".to_string(),
        };

        vec![
            SettingsRow {
                item: SettingsItem::Notifications,
                label: SettingsItem::Notifications.label(),
                value: toggle(app.config.notify),
                is_toggle_on: Some(app.config.notify),
            },
            SettingsRow {
                item: SettingsItem::LeftSidebar,
                label: SettingsItem::LeftSidebar.label(),
                value: toggle(ui.show_nav_rail),
                is_toggle_on: Some(ui.show_nav_rail),
            },
            SettingsRow {
                item: SettingsItem::InlineImages,
                label: SettingsItem::InlineImages.label(),
                value: toggle(ui.inline_images),
                is_toggle_on: Some(ui.inline_images),
            },
            SettingsRow {
                item: SettingsItem::ImageProtocol,
                label: SettingsItem::ImageProtocol.label(),
                value: protocol_value,
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::ViewDensity,
                label: SettingsItem::ViewDensity.label(),
                value: self.density_label.clone(),
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::ColorTheme,
                label: SettingsItem::ColorTheme.label(),
                value: self.theme_label.clone(),
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::DownloadFolder,
                label: SettingsItem::DownloadFolder.label(),
                value: app.config.resolved_download_dir().to_string_lossy().to_string(),
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::KeyboardShortcuts,
                label: SettingsItem::KeyboardShortcuts.label(),
                value: "View help (?)".to_string(),
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::SiteUrl,
                label: SettingsItem::SiteUrl.label(),
                value: site_value,
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::DeploymentKind,
                label: SettingsItem::DeploymentKind.label(),
                value: deployment_value,
                is_toggle_on: None,
            },
            SettingsRow {
                item: SettingsItem::ConfigFile,
                label: SettingsItem::ConfigFile.label(),
                value: config::config_file_path().to_string_lossy().to_string(),
                is_toggle_on: None,
            },
        ]
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp | Action::CursorUp | Action::PrevField => {
                self.selected = (self.selected + SETTINGS_COUNT - 1) % SETTINGS_COUNT;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::NextField => {
                self.selected = (self.selected + 1) % SETTINGS_COUNT;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Submit | Action::Input(' ') => {
                let cmd = self.selected_command();
                self.pending_command = Some(cmd);
                match cmd {
                    SettingsCommand::ToggleLeftSidebar => {
                        OverlayOutcome::Dispatch(Action::ToggleNavRail)
                    }
                    SettingsCommand::ToggleInlineImages => {
                        OverlayOutcome::Dispatch(Action::ToggleInlineImages)
                    }
                    SettingsCommand::OpenKeyboardShortcuts => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenHelp)
                    }
                    _ => OverlayOutcome::Handled,
                }
            }
            Action::CloseOverlay | Action::OpenSettings | Action::Input('q') => {
                OverlayOutcome::Close
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            76.min(area.width.saturating_sub(4)).max(30),
            (SETTINGS_COUNT as u16 + 4).min(area.height.saturating_sub(2)).max(8),
        );
        frame.render_widget(Clear, rect);

        let block = modal_block(" Settings (Ctrl+,) ", theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let rows = self.rows(app, ui);
        let visible_rows = inner.height.saturating_sub(1) as usize;
        let start = if self.selected >= visible_rows && visible_rows > 0 {
            self.selected - visible_rows + 1
        } else {
            0
        };

        for (slot, row) in rows.iter().skip(start).take(visible_rows).enumerate() {
            let index = start + slot;
            let y = inner.y + slot as u16;
            let selected = index == self.selected;
            let prefix = if selected { "▸ " } else { "  " };
            let label_style = if selected {
                theme.selection()
            } else {
                Style::default().fg(theme.fg)
            };
            let value_style = if selected {
                theme.selection().remove_modifier(Modifier::BOLD)
            } else {
                match row.is_toggle_on {
                    Some(true) => Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
                    Some(false) => theme.muted_style(),
                    None => theme.muted_style(),
                }
            };

            let label_col_w = 24usize;
            let padded_label = format!("{prefix}{:<width$}", row.label, width = label_col_w);
            let used = padded_label.width() + 2;
            let val_max = (inner.width as usize).saturating_sub(used);
            let val_text = truncate_width(&row.value, val_max);
            let tail_pad = (inner.width as usize)
                .saturating_sub(padded_label.width() + val_text.width() + 1);

            let line = Line::from(vec![
                Span::styled(padded_label, label_style),
                Span::styled(" ", label_style),
                Span::styled(val_text, value_style),
                Span::styled(" ".repeat(tail_pad), label_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem(index));
        }

        let hint_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(
                    " ↑/↓ navigate · Enter confirm · Esc cancel · saved to config.toml",
                    inner.width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
}

fn toggle(on: bool) -> String {
    if on {
        "On".to_string()
    } else {
        "Off".to_string()
    }
}

fn protocol_label(protocol: TerminalGraphicsProtocol) -> &'static str {
    match protocol {
        TerminalGraphicsProtocol::Kitty => "Kitty",
        TerminalGraphicsProtocol::ITerm2 => "iTerm2",
        TerminalGraphicsProtocol::Sixel => "Sixel",
        TerminalGraphicsProtocol::HalfBlocks => "Half-Blocks (24-bit)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona},
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn test_app() -> (AppState, UiState) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            deployment: DeploymentKind::Cloud,
            notify: true,
            show_nav_rail: true,
            inline_images: false,
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

    #[tokio::test]
    async fn settings_rows_expose_all_eleven_entries() {
        let (app, ui) = test_app();
        let ov = SettingsOverlay::new().with_display_labels("Kitty", "Compact", "Atlassian Light");
        let rows = ov.rows(&app, &ui);
        assert_eq!(rows.len(), SETTINGS_COUNT);

        let labels: Vec<&str> = rows.iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            vec![
                "Notifications",
                "Left sidebar",
                "Inline image previews",
                "Image protocol",
                "View density",
                "Color theme",
                "Download folder",
                "Keyboard shortcuts",
                "Site URL",
                "Deployment kind",
                "Config file path",
            ]
        );
        assert_eq!(rows[0].value, "On");
        assert_eq!(rows[1].value, "On");
        assert_eq!(rows[2].value, "Off");
        assert_eq!(rows[3].value, "Kitty");
        assert_eq!(rows[4].value, "Compact");
        assert_eq!(rows[5].value, "Atlassian Light");
        assert_eq!(rows[8].value, "https://acme.atlassian.net");
        assert_eq!(rows[9].value, "Cloud");
    }

    #[test]
    fn settings_navigation_wraps_and_dispatches_live_actions() {
        let mut ov = SettingsOverlay::new();
        assert_eq!(ov.selected, 0);

        ov.handle(&Action::MoveUp);
        assert_eq!(ov.selected, SETTINGS_COUNT - 1);
        assert_eq!(ov.selected_item(), SettingsItem::ConfigFile);

        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected, 0);
        assert_eq!(ov.selected_item(), SettingsItem::Notifications);

        ov.select(SettingsItem::LeftSidebar as usize);
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::ToggleNavRail)
        );
        assert_eq!(ov.pending_command, Some(SettingsCommand::ToggleLeftSidebar));

        ov.select(SettingsItem::InlineImages as usize);
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::ToggleInlineImages)
        );

        ov.select(SettingsItem::KeyboardShortcuts as usize);
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenHelp)
        );

        assert_eq!(ov.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[tokio::test]
    async fn settings_renders_cleanly_in_test_backend() {
        let (app, ui) = test_app();
        let ov = SettingsOverlay::new();
        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| ov.draw(f, f.area(), &app, &ui, &mut hitmap))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Settings"));
        assert!(text.contains("Notifications"));
        assert!(text.contains("Left sidebar"));
        assert!(text.contains("Inline image previews"));
        assert!(text.contains("Image protocol"));
        assert!(text.contains("View density"));
        assert!(text.contains("Color theme"));
        assert!(text.contains("Download folder"));
        assert!(text.contains("Keyboard shortcuts"));
        assert!(text.contains("Site URL"));
        assert!(text.contains("Deployment kind"));
        assert!(text.contains("Config file path"));
        assert!(!hitmap.is_empty());
    }
}
