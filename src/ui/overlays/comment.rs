//! Markdown comment composer modal (`c`) for Jira issues and Confluence pages.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, ExternalEditorTarget, UiState},
    doc_view::{truncate_width, wrap_plain_text},
    hitmap::HitMap,
    keymap::Action,
    overlays::{centered_rect, OverlayOutcome},
    text_edit::TextBuffer,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    Issue(String),
    Page { page_id: String, title: String },
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
                    self.error = Some("댓글 내용을 입력하세요".to_string());
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
            72.min(area.width.saturating_sub(4)).max(30),
            16.min(area.height.saturating_sub(2)).max(9),
        );
        frame.render_widget(Clear, rect);

        let title = match &self.target {
            CommentTarget::Issue(k) => format!(" 댓글 작성 — {k} "),
            CommentTarget::Page { title, .. } => format!(" 페이지 댓글 작성 — {title} "),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
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
                format!(" {err}"),
                Style::default().fg(theme.error),
            ))
        } else {
            Line::from(Span::styled(
                truncate_width(
                    " Shift+Enter: 줄바꿈   Ctrl+E: $EDITOR   Ctrl+S/Enter: 등록   Esc: 취소",
                    chunks[1].width as usize,
                ),
                theme.muted_style(),
            ))
        };
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }
}
