//! Jira issue assignee live search picker modal (`a`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::{
    models::UserSummary,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_block, read_clipboard_text, sanitize_single_line, OverlayOutcome,
        },
        text_edit::TextBuffer,
    },
};

#[derive(Debug, Clone)]
pub struct AssigneeOverlay {
    pub issue_key: String,
    pub project_or_issue_key: String,
    pub query: TextBuffer,
    pub users: Vec<UserSummary>,
    /// Index `0` is always "Unassigned"; `1..` map to `users[idx - 1]`.
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl AssigneeOverlay {
    pub fn new(issue_key: impl Into<String>, project_key: impl Into<String>) -> Self {
        let key = issue_key.into();
        let proj = project_key.into();
        Self {
            project_or_issue_key: if proj.is_empty() { key.clone() } else { proj },
            issue_key: key,
            query: TextBuffer::new(),
            users: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
        }
    }

    pub fn set_users(&mut self, users: Vec<UserSummary>) {
        self.users = users;
        self.loading = false;
        self.error = None;
        let max_idx = self.users.len();
        if self.selected > max_idx {
            self.selected = 0;
        }
    }

    pub fn set_error(&mut self, err: String) {
        self.loading = false;
        self.error = Some(err);
    }

    pub fn select(&mut self, index: usize) {
        if index <= self.users.len() {
            self.selected = index;
        }
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the search query.
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        self.query.insert_str(&cleaned);
        self.loading = true;
        true
    }

    /// Returns `Some(None)` for Unassigned, `Some(Some(user))` for a user, or `None` if invalid.
    pub fn selected_assignee(&self) -> Option<Option<&UserSummary>> {
        if self.selected == 0 {
            Some(None)
        } else {
            self.users.get(self.selected - 1).map(Some)
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let total = self.users.len() + 1;
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::CursorUp | Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::CursorDown | Action::MoveDown => {
                if self.selected + 1 < total {
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
                self.loading = true;
                OverlayOutcome::Dispatch(Action::RemoteSearch)
            }
            Action::Backspace => {
                self.query.backspace();
                self.loading = true;
                OverlayOutcome::Dispatch(Action::RemoteSearch)
            }
            Action::Delete => {
                self.query.delete();
                self.loading = true;
                OverlayOutcome::Dispatch(Action::RemoteSearch)
            }
            Action::Paste(s) => {
                let cleaned = sanitize_single_line(s);
                if !cleaned.is_empty() {
                    self.query.insert_str(&cleaned);
                    self.loading = true;
                    return OverlayOutcome::Dispatch(Action::RemoteSearch);
                }
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => OverlayOutcome::Dispatch(Action::Submit),
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
            62.min(area.width.saturating_sub(4)).max(28),
            18.min(area.height.saturating_sub(2)).max(9),
        );
        frame.render_widget(Clear, rect);

        let block = modal_block(format!(" Assign Issue — {} ", self.issue_key), theme, true);
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

        let q_line = Line::from(vec![
            Span::styled(" Search: ", theme.accent_style()),
            Span::raw(&self.query.text),
            Span::styled("▏", theme.accent_style()),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                q_line,
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let mut lines = Vec::new();
        let max_rows = chunks[1].height as usize;

        if self.loading {
            let sel0 = self.selected == 0;
            lines.push(Line::from(Span::styled(
                format!("{}○ Unassigned", if sel0 { "▸ " } else { "  " }),
                if sel0 {
                    theme.selection()
                } else {
                    theme.muted_style()
                },
            )));
            hitmap.add_line(chunks[1].x, chunks[1].y, chunks[1].width, HitTarget::OverlayItem(0));
            lines.push(Line::from(Span::styled(
                "  Searching assignable users…",
                theme.muted_style(),
            )));
        } else if let Some(err) = &self.error {
            let sel0 = self.selected == 0;
            lines.push(Line::from(Span::styled(
                format!("{}○ Unassigned", if sel0 { "▸ " } else { "  " }),
                if sel0 {
                    theme.selection()
                } else {
                    theme.muted_style()
                },
            )));
            hitmap.add_line(chunks[1].x, chunks[1].y, chunks[1].width, HitTarget::OverlayItem(0));
            lines.push(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(theme.error),
            )));
        } else {
            let total = self.users.len() + 1;
            let start = if self.selected >= max_rows && max_rows > 0 {
                self.selected - max_rows + 1
            } else {
                0
            };
            for (vis_idx, row_idx) in (start..total).take(max_rows).enumerate() {
                let is_sel = self.selected == row_idx;
                let line = if row_idx == 0 {
                    Line::from(Span::styled(
                        format!("{}○ Unassigned", if is_sel { "▸ " } else { "  " }),
                        if is_sel {
                            theme.selection()
                        } else {
                            theme.muted_style()
                        },
                    ))
                } else {
                    let user = &self.users[row_idx - 1];
                    let email = user
                        .email
                        .as_deref()
                        .map(|e| format!(" <{e}>"))
                        .unwrap_or_default();
                    let label = truncate_width(
                        &format!(
                            "{}● {}{}",
                            if is_sel { "▸ " } else { "  " },
                            user.display_name,
                            email
                        ),
                        chunks[1].width as usize,
                    );
                    Line::from(Span::styled(
                        label,
                        if is_sel {
                            theme.selection()
                        } else {
                            Style::default().fg(theme.fg)
                        },
                    ))
                };
                lines.push(line);
                hitmap.add_line(
                    chunks[1].x,
                    chunks[1].y + vis_idx as u16,
                    chunks[1].width,
                    HitTarget::OverlayItem(row_idx),
                );
            }
            if self.users.is_empty() && lines.len() < max_rows {
                lines.push(Line::from(Span::styled(
                    "  No matching users found",
                    theme.muted_style(),
                )));
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(
                    " ↑/↓ select · Enter confirm · Ctrl+V paste · Esc cancel",
                    chunks[2].width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))),
            chunks[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignee_selection_and_paste_search() {
        let mut ov = AssigneeOverlay::new("ENG-10", "ENG");
        ov.set_users(vec![UserSummary {
            id: "u1".into(),
            display_name: "Jane Doe".into(),
            email: Some("jane@acme.com".into()),
            avatar_url: None,
            active: true,
        }]);
        assert!(matches!(ov.selected_assignee(), Some(None)));

        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected_assignee().unwrap().unwrap().display_name, "Jane Doe");

        assert_eq!(
            ov.handle(&Action::Paste("alice\n".into())),
            OverlayOutcome::Dispatch(Action::RemoteSearch)
        );
        assert_eq!(ov.query.text, "alice");
        assert!(ov.loading);
    }
}
