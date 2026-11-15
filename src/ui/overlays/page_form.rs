//! Confluence page create (`n`) & edit (`e`) modal with `$EDITOR` (`Ctrl+E`) support.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::PageDetail,
    ui::{
        app::{AppState, ExternalEditorTarget, UiState},
        doc_view::{truncate_width, wrap_plain_text},
        hitmap::HitMap,
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
        text_edit::TextBuffer,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFormField {
    Title,
    Body,
}

#[derive(Debug, Clone)]
pub struct PageFormOverlay {
    /// `None` when creating a new page; `Some(page_id)` when editing existing page.
    pub edit_page_id: Option<String>,
    pub prev_version: i32,
    pub space_key: String,
    pub parent_id: Option<String>,
    pub title: TextBuffer,
    pub body: TextBuffer,
    pub field: PageFormField,
    pub error: Option<String>,
}

impl PageFormOverlay {
    pub fn new_page(space_key: impl Into<String>, parent_id: Option<String>) -> Self {
        Self {
            edit_page_id: None,
            prev_version: 0,
            space_key: space_key.into(),
            parent_id,
            title: TextBuffer::new(),
            body: TextBuffer::new(),
            field: PageFormField::Title,
            error: None,
        }
    }

    pub fn edit_page(detail: &PageDetail) -> Self {
        Self {
            edit_page_id: Some(detail.summary.id.clone()),
            prev_version: detail.summary.version_number,
            space_key: detail.summary.space_key.clone(),
            parent_id: detail.summary.parent_id.clone(),
            title: TextBuffer::from_text(detail.summary.title.clone()),
            body: TextBuffer::from_text(detail.body_markdown.clone()),
            field: PageFormField::Body,
            error: None,
        }
    }

    fn active_buffer_mut(&mut self) -> &mut TextBuffer {
        match self.field {
            PageFormField::Title => &mut self.title,
            PageFormField::Body => &mut self.body,
        }
    }

    pub fn editor_target(&self) -> (ExternalEditorTarget, String) {
        (ExternalEditorTarget::PageFormBody, self.body.text.clone())
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::NextField | Action::PrevField => {
                self.field = match self.field {
                    PageFormField::Title => PageFormField::Body,
                    PageFormField::Body => PageFormField::Title,
                };
                OverlayOutcome::Handled
            }
            Action::ExternalEditor => OverlayOutcome::Dispatch(Action::ExternalEditor),
            Action::InsertNewline => {
                if self.field == PageFormField::Body {
                    self.body.insert_char('\n');
                }
                OverlayOutcome::Handled
            }
            Action::Submit => {
                if self.title.text.trim().is_empty() {
                    self.error = Some("페이지 제목을 입력하세요".to_string());
                    self.field = PageFormField::Title;
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::Dispatch(Action::Submit)
            }
            Action::Input(ch) => {
                self.active_buffer_mut().insert_char(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.active_buffer_mut().backspace();
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.active_buffer_mut().delete();
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.active_buffer_mut().move_left();
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.active_buffer_mut().move_right();
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.active_buffer_mut().move_home();
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.active_buffer_mut().move_end();
                OverlayOutcome::Handled
            }
            Action::CursorUp => {
                if self.field == PageFormField::Body {
                    self.body.move_up();
                } else {
                    self.field = PageFormField::Title;
                }
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                if self.field == PageFormField::Body {
                    self.body.move_down();
                } else {
                    self.field = PageFormField::Body;
                }
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                self.active_buffer_mut().insert_str(s);
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
            82.min(area.width.saturating_sub(4)).max(34),
            24.min(area.height.saturating_sub(2)).max(12),
        );
        frame.render_widget(Clear, rect);

        let modal_title = match &self.edit_page_id {
            Some(id) => format!(" Confluence 페이지 수정 (#{id}, v{}) ", self.prev_version),
            None => format!(
                " 새 Confluence 페이지 ({}{}) ",
                self.space_key,
                self.parent_id
                    .as_deref()
                    .map(|p| format!(" / 하위 #{p}"))
                    .unwrap_or_default()
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(modal_title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 6 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(inner);

        let title_active = self.field == PageFormField::Title;
        let title_line = Line::from(vec![
            Span::styled(" 제목: ", if title_active { theme.accent_style() } else { theme.muted_style() }),
            Span::styled(
                &self.title.text,
                if title_active {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            Span::styled(if title_active { "▏" } else { "" }, theme.accent_style()),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                title_line,
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let body_active = self.field == PageFormField::Body;
        let body_block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 본문 Markdown (Ctrl+E: 외부 $EDITOR) ",
                theme.title(body_active),
            )))
            .border_style(theme.border_style(body_active));
        let body_inner = body_block.inner(chunks[1]);
        let wrapped = wrap_plain_text(&self.body.text, body_inner.width.max(4) as usize);
        let body_lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        frame.render_widget(Paragraph::new(body_lines).block(body_block), chunks[1]);

        let footer = if let Some(err) = &self.error {
            Line::from(Span::styled(
                format!(" 오류: {err}"),
                Style::default().fg(theme.error),
            ))
        } else {
            Line::from(Span::styled(
                truncate_width(
                    " Tab: 제목↔본문   Ctrl+E: $EDITOR   Ctrl+S/Enter: 게시   Esc: 취소",
                    chunks[2].width as usize,
                ),
                theme.muted_style(),
            ))
        };
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }
}
