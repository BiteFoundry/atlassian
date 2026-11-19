//! Markdown comment composer modal (`c`) for Jira issues and Confluence pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, ExternalEditorTarget, UiState},
    doc_view::{truncate_width, wrap_plain_text},
    hitmap::HitMap,
    keymap::Action,
    overlays::{centered_rect, modal_block, read_clipboard_text, OverlayOutcome},
    text_edit::TextBuffer,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    Issue(String),
    Page { page_id: String, title: String },
}

impl CommentTarget {
    /// Returns the persistent `DraftStore` key for this comment target.
    pub fn draft_key(&self) -> String {
        match self {
            Self::Issue(key) => format!("comment:issue:{key}"),
            Self::Page { page_id, .. } => format!("comment:page:{page_id}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommentOverlay {
    pub target: CommentTarget,
    pub body: TextBuffer,
    pub error: Option<String>,
}

impl CommentOverlay {
    pub fn new(target: CommentTarget) -> Self {
        Self {
            target,
            body: TextBuffer::new(),
            error: None,
        }
    }

    pub fn with_draft(target: CommentTarget, draft: impl Into<String>) -> Self {
        Self {
            target,
            body: TextBuffer::from_text(draft),
            error: None,
        }
    }

    pub fn draft_key(&self) -> String {
        self.target.draft_key()
    }

    pub fn draft_text(&self) -> &str {
        &self.body.text
    }

    pub fn has_unsent_content(&self) -> bool {
        !self.body.text.trim().is_empty()
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the comment body.
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        self.body.insert_str(&text);
        self.error = None;
        true
    }

    pub fn editor_target(&self) -> (ExternalEditorTarget, String) {
        (ExternalEditorTarget::CommentBody, self.body.text.clone())
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::ExternalEditor => OverlayOutcome::Dispatch(Action::ExternalEditor),
            Action::InsertNewline => {
                self.body.insert_char('\n');
                OverlayOutcome::Handled
            }
            Action::Submit => {
                if self.body.text.trim().is_empty() {
                    self.error = Some("Please enter a comment".to_string());
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::Dispatch(Action::Submit)
                }
            }
            Action::Input(ch) => {
                self.body.insert_char(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.body.backspace();
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.body.delete();
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.body.move_left();
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.body.move_right();
                OverlayOutcome::Handled
            }
            Action::CursorUp => {
                self.body.move_up();
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                self.body.move_down();
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.body.move_home();
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.body.move_end();
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                self.body.insert_str(s);
                self.error = None;
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
        _hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            74.min(area.width.saturating_sub(4)).max(30),
            16.min(area.height.saturating_sub(2)).max(9),
        );
        frame.render_widget(Clear, rect);

        let title = match &self.target {
            CommentTarget::Issue(k) => format!(" Add Comment — {k} "),
            CommentTarget::Page { title, .. } => format!(" Add Page Comment — {title} "),
        };
        let block = modal_block(title, theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let wrapped = wrap_plain_text(&self.body.text, chunks[0].width.max(4) as usize);
        let mut lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled("▏", theme.accent_style()));
        }
        frame.render_widget(Paragraph::new(lines), chunks[0]);

        let footer = if let Some(err) = &self.error {
            Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme.error),
            ))
        } else {
            Line::from(Span::styled(
                truncate_width(
                    " Shift+Enter newline · Ctrl+V paste · Ctrl+E $EDITOR · Enter confirm · Esc cancel",
                    chunks[1].width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))
        };
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_draft_key_and_validation() {
        let mut issue_comment = CommentOverlay::new(CommentTarget::Issue("PROJ-9".into()));
        assert_eq!(issue_comment.draft_key(), "comment:issue:PROJ-9");
        assert!(!issue_comment.has_unsent_content());

        assert_eq!(issue_comment.handle(&Action::Submit), OverlayOutcome::Handled);
        assert!(issue_comment.error.is_some());

        issue_comment.handle(&Action::Paste("Looks good to me!".into()));
        assert!(issue_comment.error.is_none());
        assert!(issue_comment.has_unsent_content());
        assert_eq!(
            issue_comment.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit)
        );

        let page_comment = CommentOverlay::with_draft(
            CommentTarget::Page {
                page_id: "123".into(),
                title: "Design".into(),
            },
            "Saved note",
        );
        assert_eq!(page_comment.draft_key(), "comment:page:123");
        assert_eq!(page_comment.draft_text(), "Saved note");
    }
}
