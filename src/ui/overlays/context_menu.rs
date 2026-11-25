//! Reusable anchored right-click / keyboard context menu for Jira issues, Confluence pages, and global actions.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::ui::{
    app::{AppState, UiState, WorkspaceTab},
    doc_view::truncate_width,
    hitmap::{HitMap, HitTarget},
    keymap::Action,
    overlays::{anchored_rect, modal_block, register_modal_chrome, OverlayOutcome},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTarget {
    JiraIssue {
        key: String,
        url: String,
    },
    ConfluencePage {
        page_id: String,
        title: String,
        url: String,
    },
    Global,
}

/// Command executed when a context menu item is activated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuAction {
    /// Directly dispatchable `Action`.
    Action(Action),
    /// Open the attachment browser overlay for the current issue/page.
    OpenAttachments,
    /// Open the Settings modal (`Ctrl+,`).
    OpenSettings,
    /// Copy a string (e.g. issue key or web URL) to the system clipboard.
    CopyText {
        label: &'static str,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub command: ContextMenuAction,
}

impl MenuItem {
    pub fn new(
        label: impl Into<String>,
        shortcut: Option<&str>,
        command: ContextMenuAction,
    ) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            command,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenuOverlay {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

/// Alias aligned with `teams` / `outlook` overlay naming.
pub type ContextMenu = ContextMenuOverlay;

impl ContextMenuOverlay {
    pub fn for_jira_issue(
        key: impl Into<String>,
        url: impl Into<String>,
        anchor: (u16, u16),
    ) -> Self {
        let key = key.into();
        let url = url.into();
        let items = vec![
            MenuItem::new(
                "Transition status",
                Some("m"),
                ContextMenuAction::Action(Action::OpenTransition),
            ),
            MenuItem::new(
                "Assign to me",
                Some("i"),
                ContextMenuAction::Action(Action::AssignToMe),
            ),
            MenuItem::new(
                "Change assignee",
                Some("a"),
                ContextMenuAction::Action(Action::OpenAssignee),
            ),
            MenuItem::new(
                "Add comment",
                Some("c"),
                ContextMenuAction::Action(Action::OpenComment),
            ),
            MenuItem::new("Attachments", Some("A"), ContextMenuAction::OpenAttachments),
            MenuItem::new(
                "Cross-links",
                Some("g"),
                ContextMenuAction::Action(Action::OpenCrossLinks),
            ),
            MenuItem::new(
                "Copy key",
                Some("y"),
                ContextMenuAction::CopyText {
                    label: "Issue key",
                    value: key.clone(),
                },
            ),
            MenuItem::new(
                "Copy URL",
                Some("Y"),
                ContextMenuAction::CopyText {
                    label: "Issue URL",
                    value: url.clone(),
                },
            ),
            MenuItem::new(
                "Open in browser",
                Some("o"),
                ContextMenuAction::Action(Action::OpenInBrowser),
            ),
        ];

        Self {
            context: ContextTarget::JiraIssue { key, url },
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn for_confluence_page(
        page_id: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
        anchor: (u16, u16),
    ) -> Self {
        let page_id = page_id.into();
        let title = title.into();
        let url = url.into();
        let items = vec![
            MenuItem::new(
                "Edit page",
                Some("e"),
                ContextMenuAction::Action(Action::EditItem),
            ),
            MenuItem::new(
                "Add comment",
                Some("c"),
                ContextMenuAction::Action(Action::OpenComment),
            ),
            MenuItem::new(
                "New child page",
                Some("n"),
                ContextMenuAction::Action(Action::NewItem),
            ),
            MenuItem::new("Attachments", Some("A"), ContextMenuAction::OpenAttachments),
            MenuItem::new(
                "Cross-links",
                Some("g"),
                ContextMenuAction::Action(Action::OpenCrossLinks),
            ),
            MenuItem::new(
                "Copy URL",
                Some("Y"),
                ContextMenuAction::CopyText {
                    label: "Page URL",
                    value: url.clone(),
                },
            ),
            MenuItem::new(
                "Open in browser",
                Some("o"),
                ContextMenuAction::Action(Action::OpenInBrowser),
            ),
        ];

        Self {
            context: ContextTarget::ConfluencePage {
                page_id,
                title,
                url,
            },
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn for_global(anchor: (u16, u16)) -> Self {
        let items = vec![
            MenuItem::new(
                "Command palette",
                Some("Ctrl+P"),
                ContextMenuAction::Action(Action::OpenCommandPalette),
            ),
            MenuItem::new(
                "Sync all",
                Some("r"),
                ContextMenuAction::Action(Action::SyncAll),
            ),
            MenuItem::new("Settings", Some("Ctrl+,"), ContextMenuAction::OpenSettings),
            MenuItem::new(
                "Help",
                Some("?"),
                ContextMenuAction::Action(Action::OpenHelp),
            ),
            MenuItem::new(
                "Quit",
                Some("q"),
                ContextMenuAction::Action(Action::Quit),
            ),
        ];

        Self {
            context: ContextTarget::Global,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn build(target: ContextTarget, anchor: (u16, u16)) -> Self {
        match target {
            ContextTarget::JiraIssue { key, url } => Self::for_jira_issue(key, url, anchor),
            ContextTarget::ConfluencePage {
                page_id,
                title,
                url,
            } => Self::for_confluence_page(page_id, title, url, anchor),
            ContextTarget::Global => Self::for_global(anchor),
        }
    }

    /// Builds a context menu inferred from the active workspace tab and current selection.
    pub fn for_active_context(anchor: (u16, u16), app: &AppState, ui: &UiState) -> Self {
        match ui.active_tab {
            WorkspaceTab::MyWork => {
                if ui.my_work_ui.active_column == 1 {
                    if let Some(page) = app.my_pages.get(ui.my_work_ui.selected_page) {
                        return Self::for_confluence_page(
                            page.id.clone(),
                            page.title.clone(),
                            page.web_url.clone(),
                            anchor,
                        );
                    }
                    return Self::for_global(anchor);
                }
                if let Some(issue) = app.my_issues.get(ui.my_work_ui.selected_issue) {
                    let url = if !issue.web_url.is_empty() {
                        issue.web_url.clone()
                    } else {
                        let base = app.config.jira_base_url();
                        if base.is_empty() {
                            String::new()
                        } else {
                            format!("{base}/browse/{}", issue.key)
                        }
                    };
                    return Self::for_jira_issue(issue.key.clone(), url, anchor);
                }
                return Self::for_global(anchor);
            }
            WorkspaceTab::Confluence => {
                if ui.focus == crate::ui::app::PaneFocus::Nav {
                    if ui.confluence_ui.nav_section == 0 {
                        return Self::for_global(anchor);
                    }
                    let tree = app.current_confluence_tree(ui);
                    if let Some(row) = tree.get(ui.confluence_ui.selected_tree_row) {
                        return Self::for_confluence_page(
                            row.page.id.clone(),
                            row.page.title.clone(),
                            row.page.web_url.clone(),
                            anchor,
                        );
                    }
                }
                if let Some(page) = &app.active_page {
                    return Self::for_confluence_page(
                        page.summary.id.clone(),
                        page.summary.title.clone(),
                        page.summary.web_url.clone(),
                        anchor,
                    );
                }
                return Self::for_global(anchor);
            }
            WorkspaceTab::Search => {
                if let Some(hit) = app.search_hits.get(ui.search_ui.selected_hit) {
                    return match hit.kind {
                        crate::models::SearchHitKind::JiraIssue => {
                            let url = if !hit.web_url.is_empty() {
                                hit.web_url.clone()
                            } else {
                                let base = app.config.jira_base_url();
                                if base.is_empty() {
                                    String::new()
                                } else {
                                    format!("{base}/browse/{}", hit.id_or_key)
                                }
                            };
                            Self::for_jira_issue(hit.id_or_key.clone(), url, anchor)
                        }
                        crate::models::SearchHitKind::ConfluencePage => Self::for_confluence_page(
                            hit.id_or_key.clone(),
                            hit.title.clone(),
                            hit.web_url.clone(),
                            anchor,
                        ),
                    };
                }
                return Self::for_global(anchor);
            }
            WorkspaceTab::Directory => {
                return Self::for_global(anchor);
            }
            WorkspaceTab::Board => {
                if ui.focus == crate::ui::app::PaneFocus::Nav {
                    return Self::for_global(anchor);
                }
            }
            WorkspaceTab::Issues => {
                if ui.focus == crate::ui::app::PaneFocus::Nav && ui.issue_ui.nav_section == 0 {
                    return Self::for_global(anchor);
                }
            }
        }
        if let Some(key) = app.selected_issue_key(ui) {
            let url = app
                .active_issue
                .as_ref()
                .filter(|d| d.summary.key == key)
                .map(|d| d.summary.web_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| {
                    let base = app.config.jira_base_url();
                    if base.is_empty() {
                        String::new()
                    } else {
                        format!("{base}/browse/{key}")
                    }
                });
            return Self::for_jira_issue(key, url, anchor);
        }
        Self::for_global(anchor)
    }

    pub fn next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }

    pub fn selected_item(&self) -> Option<&MenuItem> {
        self.items.get(self.selected)
    }

    pub fn selected_command(&self) -> Option<&ContextMenuAction> {
        self.selected_item().map(|item| &item.command)
    }

    pub fn activate(&self) -> OverlayOutcome {
        let Some(item) = self.items.get(self.selected) else {
            return OverlayOutcome::Close;
        };
        match &item.command {
            ContextMenuAction::Action(act) => OverlayOutcome::CloseAndDispatch(act.clone()),
            ContextMenuAction::OpenAttachments
            | ContextMenuAction::OpenSettings
            | ContextMenuAction::CopyText { .. } => OverlayOutcome::Dispatch(Action::Submit),
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp | Action::CursorUp | Action::PrevField => {
                self.prev();
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::NextField => {
                self.next();
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Submit => self.activate(),
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
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
        let label_width = self
            .items
            .iter()
            .map(|item| {
                item.label.width()
                    + item
                        .shortcut
                        .as_deref()
                        .map_or(0, |sc| sc.width() + 3)
            })
            .max()
            .unwrap_or(20);
        let width = (label_width as u16 + 6).clamp(26, 48);
        let height = (self.items.len() as u16 + 3).min(area.height).max(4);
        let rect = anchored_rect(area, self.anchor, width, height);

        let title = match &self.context {
            ContextTarget::JiraIssue { key, .. } => format!(" Issue · {key} "),
            ContextTarget::ConfluencePage { .. } => " Confluence Page ".to_string(),
            ContextTarget::Global => " Actions ".to_string(),
        };

        frame.render_widget(Clear, rect);
        let block = modal_block(title, theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, false);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let max_rows = inner.height.saturating_sub(1) as usize;
        let total = self.items.len();
        let sel = self.selected.min(total.saturating_sub(1));
        let start = if max_rows > 0 && sel >= max_rows {
            sel - max_rows + 1
        } else {
            0
        };
        for (vis_idx, item) in self.items.iter().skip(start).take(max_rows).enumerate() {
            let actual_idx = start + vis_idx;
            let row = inner.y + vis_idx as u16;
            let selected = actual_idx == sel;
            let prefix = if selected { "▸ " } else { "  " };
            let label_style = if selected {
                theme.selection().remove_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            };
            let hint_style = if selected {
                theme.selection()
            } else {
                theme.muted_style()
            };

            let hint = item.shortcut.as_deref().unwrap_or("");
            let hint_w = hint.width();
            let max_label_w = (inner.width as usize).saturating_sub(hint_w + prefix.width() + 2);
            let label = truncate_width(&item.label, max_label_w);
            let used = prefix.width() + label.width();
            let pad = (inner.width as usize)
                .saturating_sub(used)
                .saturating_sub(hint_w)
                .saturating_sub(1);

            let line = Line::from(vec![
                Span::styled(format!("{prefix}{label}"), label_style),
                Span::styled(" ".repeat(pad), label_style),
                Span::styled(hint, hint_style),
                Span::styled(" ", label_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, row, inner.width, 1),
            );
            hitmap.add_line(inner.x, row, inner.width, HitTarget::OverlayItem(actual_idx));
        }

        if inner.height > 1 {
            let footer_y = inner.y + inner.height - 1;
            let footer_text = if total > max_rows && max_rows > 0 {
                format!(" {}/{total} · Enter confirm · Esc", sel + 1)
            } else {
                " Enter confirm · Esc cancel".to_string()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_width(&footer_text, inner.width as usize),
                    Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                )),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
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

    #[test]
    fn jira_issue_menu_contains_all_expected_actions() {
        let menu = ContextMenuOverlay::for_jira_issue(
            "ENG-101",
            "https://acme.atlassian.net/browse/ENG-101",
            (10, 5),
        );
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Transition status",
                "Assign to me",
                "Change assignee",
                "Add comment",
                "Attachments",
                "Cross-links",
                "Copy key",
                "Copy URL",
                "Open in browser",
            ]
        );
    }

    #[test]
    fn confluence_page_menu_contains_all_expected_actions() {
        let menu = ContextMenuOverlay::for_confluence_page(
            "42",
            "Architecture",
            "https://acme.atlassian.net/wiki/pages/42",
            (10, 5),
        );
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Edit page",
                "Add comment",
                "New child page",
                "Attachments",
                "Cross-links",
                "Copy URL",
                "Open in browser",
            ]
        );
    }

    #[test]
    fn menu_navigation_wraps_and_activates_commands() {
        let mut menu = ContextMenuOverlay::for_jira_issue(
            "ENG-101",
            "https://acme.atlassian.net/browse/ENG-101",
            (5, 5),
        );
        assert_eq!(menu.selected, 0);
        assert_eq!(
            menu.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenTransition)
        );

        menu.handle(&Action::MoveUp);
        assert_eq!(menu.selected, menu.items.len() - 1);
        assert_eq!(
            menu.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenInBrowser)
        );

        // Copy key is index 6
        menu.select(6);
        assert_eq!(
            menu.selected_command(),
            Some(&ContextMenuAction::CopyText {
                label: "Issue key",
                value: "ENG-101".to_string(),
            })
        );
        assert_eq!(
            menu.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::Submit)
        );

        assert_eq!(menu.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[tokio::test]
    async fn context_menu_renders_and_clamps_to_viewport_edges() {
        let (app, ui) = test_app();
        let menu = ContextMenuOverlay::for_jira_issue(
            "ENG-7",
            "https://acme.atlassian.net/browse/ENG-7",
            (75, 22),
        );
        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| menu.draw(f, f.area(), &app, &ui, &mut hitmap))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("ENG-7"));
        assert!(text.contains("Transition status"));
        assert!(text.contains("Copy key"));
        assert!(text.contains("Open in browser"));
        assert!(!hitmap.is_empty());
    }
}
