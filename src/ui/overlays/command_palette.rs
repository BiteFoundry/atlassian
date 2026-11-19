//! Command palette (`Ctrl+P` / `:`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::{HitMap, HitTarget},
    keymap::Action,
    overlays::{
        centered_rect, modal_block, read_clipboard_text, sanitize_single_line, OverlayOutcome,
    },
    text_edit::TextBuffer,
};

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: TextBuffer,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, index: usize) {
        let len = self.filtered_entries().len();
        if index < len {
            self.selected = index;
        }
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the palette query.
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        self.query.insert_str(&cleaned);
        self.selected = 0;
        true
    }

    pub fn entries() -> Vec<PaletteEntry> {
        vec![
            PaletteEntry { label: "Switch to Kanban Board", shortcut: "Ctrl+1", action: Action::SwitchTab(0) },
            PaletteEntry { label: "Switch to Issues & JQL Explorer", shortcut: "Ctrl+2", action: Action::SwitchTab(1) },
            PaletteEntry { label: "Switch to My Work", shortcut: "Ctrl+3", action: Action::SwitchTab(2) },
            PaletteEntry { label: "Switch to Confluence Wiki & Tree", shortcut: "Ctrl+4", action: Action::SwitchTab(3) },
            PaletteEntry { label: "Switch to Unified Search", shortcut: "Ctrl+5", action: Action::SwitchTab(4) },
            PaletteEntry { label: "Switch to Directory & Sync", shortcut: "Ctrl+6", action: Action::SwitchTab(5) },
            PaletteEntry { label: "Transition Issue Status", shortcut: "m", action: Action::OpenTransition },
            PaletteEntry { label: "Assign Issue to Me", shortcut: "i", action: Action::AssignToMe },
            PaletteEntry { label: "Change Issue Assignee", shortcut: "a", action: Action::OpenAssignee },
            PaletteEntry { label: "Add Comment", shortcut: "c", action: Action::OpenComment },
            PaletteEntry { label: "Create New Issue / Child Page", shortcut: "n", action: Action::NewItem },
            PaletteEntry { label: "Edit Issue / Page ($EDITOR)", shortcut: "e", action: Action::EditItem },
            PaletteEntry { label: "Jump to Cross-Linked Jira / Confluence", shortcut: "g", action: Action::OpenCrossLinks },
            PaletteEntry { label: "Navigate Back in History", shortcut: "Backspace", action: Action::PopNavHistory },
            PaletteEntry { label: "Preview Image Attachments", shortcut: "v", action: Action::OpenImagePreview },
            PaletteEntry { label: "Upload Attachment", shortcut: "u", action: Action::UploadAttachment },
            PaletteEntry { label: "Download Attachment", shortcut: "d", action: Action::DownloadAttachment },
            PaletteEntry { label: "Open in Browser", shortcut: "o", action: Action::OpenInBrowser },
            PaletteEntry { label: "Focus JQL Query Bar", shortcut: "J", action: Action::FocusJql },
            PaletteEntry { label: "Sync All Workspace Data", shortcut: "r", action: Action::SyncAll },
            PaletteEntry { label: "Toggle Inline Image Previews", shortcut: "", action: Action::ToggleInlineImages },
            PaletteEntry { label: "Toggle Left Sidebar", shortcut: "", action: Action::ToggleNavRail },
            PaletteEntry { label: "Check for Updates", shortcut: "U", action: Action::CheckUpdate },
            PaletteEntry { label: "Open Keyboard Shortcuts Help", shortcut: "?", action: Action::OpenHelp },
            PaletteEntry { label: "Quit Atlassian TUI", shortcut: "q", action: Action::Quit },
        ]
    }

    pub fn filtered_entries(&self) -> Vec<PaletteEntry> {
        let q = self.query.text.trim().to_ascii_lowercase();
        let all = Self::entries();
        if q.is_empty() {
            return all;
        }
        all.into_iter()
            .filter(|e| {
                e.label.to_ascii_lowercase().contains(&q)
                    || e.shortcut.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let items = self.filtered_entries();
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::CursorUp | Action::MoveUp | Action::PrevField => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::CursorDown | Action::MoveDown | Action::NextField => {
                if self.selected + 1 < items.len() {
                    self.selected += 1;
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
            Action::Input(ch) => {
                self.query.insert_char(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.backspace();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.query.delete();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                let cleaned = sanitize_single_line(s);
                if !cleaned.is_empty() {
                    self.query.insert_str(&cleaned);
                    self.selected = 0;
                }
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => {
                if let Some(entry) = items.get(self.selected) {
                    OverlayOutcome::CloseAndDispatch(entry.action.clone())
                } else {
                    OverlayOutcome::Close
                }
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
        let rect = centered_rect(
            area,
            68.min(area.width.saturating_sub(4)).max(30),
            21.min(area.height.saturating_sub(2)).max(10),
        );
        frame.render_widget(Clear, rect);

        let block = modal_block(" Command Palette (Ctrl+P / :) ", theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
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

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" > ", theme.accent_style()),
                    Span::raw(&self.query.text),
                    Span::styled("▏", theme.accent_style()),
                ]),
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let items = self.filtered_entries();
        let max_rows = chunks[1].height as usize;
        let start = if self.selected >= max_rows && max_rows > 0 {
            self.selected - max_rows + 1
        } else {
            0
        };

        let mut lines = Vec::new();
        if items.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No matching commands",
                theme.muted_style(),
            )));
        } else {
            for (vis_idx, entry) in items.iter().skip(start).take(max_rows).enumerate() {
                let abs_idx = start + vis_idx;
                let is_sel = abs_idx == self.selected;
                let row_style = if is_sel {
                    theme.selection()
                } else {
                    Style::default().fg(theme.fg)
                };
                let sc_style = if is_sel {
                    theme.selection().remove_modifier(Modifier::BOLD)
                } else {
                    theme.muted_style()
                };
                let prefix = if is_sel { "▸ " } else { "  " };
                let sc = entry.shortcut;
                let sc_w = sc.width();
                let label_budget = (chunks[1].width as usize)
                    .saturating_sub(prefix.width() + sc_w + 2);
                let label = truncate_width(entry.label, label_budget);
                let pad = (chunks[1].width as usize)
                    .saturating_sub(prefix.width() + label.width() + sc_w + 1);

                lines.push(Line::from(vec![
                    Span::styled(format!("{prefix}{label}"), row_style),
                    Span::styled(" ".repeat(pad), row_style),
                    Span::styled(sc, sc_style),
                    Span::styled(" ", row_style),
                ]));
                hitmap.add_line(
                    chunks[1].x,
                    chunks[1].y + vis_idx as u16,
                    chunks[1].width,
                    HitTarget::OverlayItem(abs_idx),
                );
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);

        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(
                    " ↑/↓ select · Enter confirm · Esc cancel",
                    chunks[2].width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )),
            chunks[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_filters_and_dispatches_english_commands() {
        let mut palette = CommandPalette::new();
        assert!(!palette.filtered_entries().is_empty());

        palette.handle(&Action::Paste("Transition\n".into()));
        let filtered = palette.filtered_entries();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].action, Action::OpenTransition);

        assert_eq!(
            palette.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenTransition)
        );
    }
}
