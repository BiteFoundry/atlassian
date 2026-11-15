//! Attachment file path input overlay (`u`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::HitMap,
    keymap::Action,
    overlays::{centered_rect, OverlayOutcome},
    text_edit::TextBuffer,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadTarget {
    Issue(String),
    Page(String),
}

#[derive(Debug, Clone)]
pub struct PathInputOverlay {
    pub target: UploadTarget,
    pub path: TextBuffer,
    pub error: Option<String>,
}

impl PathInputOverlay {
    pub fn new(target: UploadTarget) -> Self {
        Self {
            target,
            path: TextBuffer::new(),
            error: None,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Input(ch) => {
                self.path.insert_char(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.path.backspace();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.path.delete();
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.path.move_left();
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.path.move_right();
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.path.move_home();
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.path.move_end();
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                self.path.insert_str(s);
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => {
                let trimmed = self.path.text.trim();
                if trimmed.is_empty() {
                    self.error = Some("파일 경로를 입력하세요".to_string());
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::Dispatch(Action::Submit)
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
        _hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 64.min(area.width.saturating_sub(4)).max(28), 8);
        frame.render_widget(Clear, rect);

        let target_label = match &self.target {
            UploadTarget::Issue(k) => format!("이슈 {k}"),
            UploadTarget::Page(id) => format!("페이지 #{id}"),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                format!(" 첨부파일 업로드 ({target_label}) "),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let mut lines = vec![
            Line::from(vec![
                Span::styled(" 경로: ", theme.accent_style()),
                Span::raw(&self.path.text),
                Span::styled("▏", theme.accent_style()),
            ]),
            Line::from(""),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" {err}"),
                Style::default().fg(theme.error),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                truncate_width(" Enter: 업로드   Esc: 취소", inner.width as usize),
                theme.muted_style(),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
