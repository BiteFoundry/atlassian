//! Markdown comment composer modal (`c`) for Jira issues and Confluence pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::ui::{
    app::{AppState, ExternalEditorTarget, UiState},
    doc_view::truncate_width,
    hitmap::{ButtonId, HitMap, HitTarget},
    keymap::Action,
    overlays::{
        centered_rect, modal_block, modal_pill_muted, modal_pill_primary, modal_pill_secondary,
        read_clipboard_text, register_modal_chrome, render_button_bar, wrap_text_with_cursor_line,
        OverlayOutcome,
    },
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
    /// Site of the issue or page the overlay was opened for.
    pub site_id: Option<String>,
    pub body: TextBuffer,
    pub body_scroll: u16,
    body_max_scroll: Option<u16>,
    follow_cursor: bool,
    pub error: Option<String>,
}

impl CommentOverlay {
    pub fn new(target: CommentTarget) -> Self {
        Self {
            target,
            site_id: None,
            body: TextBuffer::new(),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
            error: None,
        }
    }

    pub fn with_draft(target: CommentTarget, draft: impl Into<String>) -> Self {
        Self {
            target,
            site_id: None,
            body: TextBuffer::from_text(draft),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
            error: None,
        }
    }

    pub fn click_field(&mut self, _field_idx: usize) {
        self.follow_cursor = true;
    }

    fn scroll_body(&mut self, delta: i32) {
        self.follow_cursor = false;
        let cap = self
            .body_max_scroll
            .unwrap_or_else(|| self.body.text.lines().count().saturating_sub(1) as u16);
        if delta < 0 {
            self.body_scroll = self.body_scroll.saturating_sub(delta.unsigned_abs() as u16);
        } else if delta > 0 {
            self.body_scroll = self.body_scroll.saturating_add(delta as u16).min(cap);
        }
    }

    pub fn draft_key(&self) -> String {
        self.target.draft_key()
    }

    pub fn with_site(mut self, site_id: Option<String>) -> Self {
        self.site_id = site_id;
        self
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
        self.follow_cursor = true;
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
            Action::ScrollUp => {
                self.scroll_body(-3);
                OverlayOutcome::Handled
            }
            Action::ScrollDown => {
                self.scroll_body(3);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.scroll_body(*delta);
                OverlayOutcome::Handled
            }
            Action::InsertNewline => {
                self.body.insert_char('\n');
                self.follow_cursor = true;
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
                self.follow_cursor = true;
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.body.backspace();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.body.delete();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.body.move_left();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.body.move_right();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorUp => {
                self.body.move_up();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                self.body.move_down();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.body.move_home();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.body.move_end();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                self.body.insert_str(s);
                self.follow_cursor = true;
                self.error = None;
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
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
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let has_err = self.error.is_some() && inner.height >= 4;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if has_err {
                vec![Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]
            } else {
                vec![Constraint::Min(1), Constraint::Length(1)]
            })
            .split(inner);

        let body_area = chunks[0];
        hitmap.add(body_area, HitTarget::OverlayScrollArea);
        hitmap.add(body_area, HitTarget::OverlayField(0));

        let view_h = body_area.height as usize;
        let initial_w = body_area.width.max(4) as usize;
        let (wrapped_check, _) = wrap_text_with_cursor_line(&self.body.text, self.body.cursor, initial_w);
        let show_scrollbar = wrapped_check.len() > view_h && body_area.width > 2;
        let text_w = if show_scrollbar {
            body_area.width.saturating_sub(1).max(4) as usize
        } else {
            initial_w
        };
        let (wrapped, cursor_line) = wrap_text_with_cursor_line(&self.body.text, self.body.cursor, text_w);
        let total_lines = wrapped.len();
        let max_scroll = total_lines.saturating_sub(view_h) as u16;
        self.body_max_scroll = Some(max_scroll);

        if self.follow_cursor {
            if cursor_line < self.body_scroll {
                self.body_scroll = cursor_line;
            } else if view_h > 0 && cursor_line >= self.body_scroll.saturating_add(view_h as u16) {
                self.body_scroll = cursor_line.saturating_sub(view_h as u16 - 1);
            }
            self.follow_cursor = false;
        }
        self.body_scroll = self.body_scroll.min(max_scroll);

        let mut lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled("▏", theme.accent_style()));
        }
        let text_area = if show_scrollbar {
            Rect::new(body_area.x, body_area.y, body_area.width.saturating_sub(1), body_area.height)
        } else {
            body_area
        };
        frame.render_widget(
            Paragraph::new(lines).scroll((self.body_scroll, 0)),
            text_area,
        );
        if show_scrollbar {
            let sb_area = Rect::new(
                body_area.right().saturating_sub(1),
                body_area.y,
                1,
                body_area.height,
            );
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                .position(self.body_scroll as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        let btn_area = if has_err {
            if let Some(err) = &self.error {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate_width(&format!(" Error: {err}"), chunks[1].width as usize),
                        Style::default().fg(theme.error),
                    ))),
                    chunks[1],
                );
            }
            chunks[2]
        } else {
            chunks[1]
        };

        render_button_bar(
            frame,
            btn_area,
            &[
                ("[Ctrl+S] Submit", ButtonId::OverlaySubmit, modal_pill_primary(theme)),
                ("[Ctrl+E] $EDITOR", ButtonId::OverlayExternalEditor, modal_pill_secondary(theme)),
                ("[Esc] Cancel", ButtonId::OverlayCancel, modal_pill_muted(theme)),
            ],
            Some("· Shift+Enter newline · Ctrl+V paste"),
            theme,
            hitmap,
        );
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
