//! Jira issue assignee live search picker modal (`a`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::UserSummary,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
        text_edit::TextBuffer,
    },
};

#[derive(Debug, Clone)]
pub struct AssigneeOverlay {
    pub issue_key: String,
    pub project_or_issue_key: String,
    pub query: TextBuffer,
    pub users: Vec<UserSummary>,
    /// Index `0` is always "미할당 (Unassigned)"; `1..` map to `users[idx - 1]`.
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

    /// Returns `Some(None)` for Unassigned, `Some(Some(user))` for user, or `None` if invalid.
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
                self.query.insert_str(s);
                self.loading = true;
                OverlayOutcome::Dispatch(Action::RemoteSearch)
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
            60.min(area.width.saturating_sub(4)).max(26),
            18.min(area.height.saturating_sub(2)).max(9),
        );
        frame.render_widget(Clear, rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                format!(" 담당자 지정 — {} ", self.issue_key),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
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
            Span::styled(" 검색: ", theme.accent_style()),
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

        // Row 0: Unassigned
        let sel0 = self.selected == 0;
        lines.push(Line::from(Span::styled(
            format!("{}○ 미할당 (Unassigned)", if sel0 { "▸ " } else { "  " }),
            if sel0 {
                theme.selection()
            } else {
                theme.muted_style()
            },
        )));
        hitmap.add_line(chunks[1].x, chunks[1].y, chunks[1].width, HitTarget::OverlayItem(0));

        if self.loading {
            lines.push(Line::from(Span::styled(
                "  사용자 검색 중…",
                theme.muted_style(),
            )));
        } else if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  오류: {err}"),
                Style::default().fg(theme.error),
            )));
        } else {
            for (u_idx, user) in self.users.iter().enumerate() {
                let row_idx = u_idx + 1;
                if row_idx >= max_rows {
                    break;
                }
                let is_sel = self.selected == row_idx;
                let email = user
                    .email
                    .as_deref()
                    .map(|e| format!(" <{e}>"))
                    .unwrap_or_default();
                let label = truncate_width(
                    &format!("{}● {}{}", if is_sel { "▸ " } else { "  " }, user.display_name, email),
                    chunks[1].width as usize,
                );
                lines.push(Line::from(Span::styled(
                    label,
                    if is_sel {
                        theme.selection()
                    } else {
                        Style::default()
                    },
                )));
                hitmap.add_line(
                    chunks[1].x,
                    chunks[1].y + row_idx as u16,
                    chunks[1].width,
                    HitTarget::OverlayItem(row_idx),
                );
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(" ↑/↓: 선택   Enter: 할당   Esc: 닫기", chunks[2].width as usize),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))),
            chunks[2],
        );
    }
}
