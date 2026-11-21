//! Confluence page create (`n`) & edit (`e`) modal with `$EDITOR` (`Ctrl+E`), `Ctrl+V` paste, and draft helpers.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};

use crate::{
    models::PageDetail,
    ui::{
        app::{AppState, ExternalEditorTarget, UiState},
        doc_view::{truncate_width, wrap_plain_text},
        hitmap::HitMap,
        keymap::Action,
        overlays::{
            centered_rect, modal_block, read_clipboard_text, sanitize_single_line, OverlayOutcome,
        },
        text_edit::TextBuffer,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFormField {
    Title,
    Body,
}

/// Serializable draft payload for persisting unsent Confluence page forms in `DraftStore`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageFormDraft {
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct PageFormOverlay {
    /// `None` when creating a new page; `Some(page_id)` when editing an existing page.
    pub edit_page_id: Option<String>,
    pub prev_version: i32,
    pub space_key: String,
    pub parent_id: Option<String>,
    pub title: TextBuffer,
    pub body: TextBuffer,
    pub initial_title: String,
    pub initial_body: String,
    pub initial_body_raw: Option<String>,
    pub initial_body_representation: Option<String>,
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
            initial_title: String::new(),
            initial_body: String::new(),
            initial_body_raw: None,
            initial_body_representation: None,
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
            initial_title: detail.summary.title.clone(),
            initial_body: detail.body_markdown.clone(),
            initial_body_raw: (detail.body_representation == "storage"
                || !detail.body_raw.trim().is_empty())
            .then(|| detail.body_raw.clone()),
            initial_body_representation: (!detail.body_representation.trim().is_empty())
                .then(|| detail.body_representation.clone()),
            field: PageFormField::Body,
            error: None,
        }
    }

    /// Returns a stable `DraftStore` key for this create/edit Confluence page form.
    pub fn draft_key(&self) -> String {
        if let Some(id) = &self.edit_page_id {
            format!("page:edit:{id}")
        } else if let Some(parent) = &self.parent_id {
            format!("page:new:{}:{parent}", self.space_key)
        } else {
            format!("page:new:{}", self.space_key)
        }
    }

    pub fn to_draft(&self) -> PageFormDraft {
        PageFormDraft {
            title: self.title.text.clone(),
            body: self.body.text.clone(),
            base_version: self.edit_page_id.as_ref().map(|_| self.prev_version),
        }
    }

    pub fn to_draft_json(&self) -> String {
        serde_json::to_string(&self.to_draft()).unwrap_or_else(|_| self.body.text.clone())
    }

    /// Restores a draft from JSON (produced by `to_draft_json`) or plain Markdown text.
    pub fn apply_draft_raw(&mut self, raw: &str) -> bool {
        if raw.trim().is_empty() {
            return false;
        }
        if let Ok(draft) = serde_json::from_str::<PageFormDraft>(raw) {
            if !draft.title.trim().is_empty() {
                self.title.set_text(draft.title);
            }
            self.body.set_text(draft.body);
            if self.edit_page_id.is_some() {
                if let Some(draft_ver) = draft.base_version.filter(|v| *v > 0) {
                    let current_ver = self.prev_version;
                    if draft_ver != current_ver {
                        // Keep the draft's original base_version so submitting checks version
                        // conflict instead of silently promoting a stale draft over a newer page.
                        self.prev_version = draft_ver;
                        self.error = Some(format!(
                            "Draft based on v{draft_ver}, but page is now v{current_ver}; submitting will detect conflict"
                        ));
                    }
                }
            }
            return true;
        }
        self.body.set_text(raw);
        true
    }

    pub fn has_unsent_content(&self) -> bool {
        !self.title.text.trim().is_empty() || !self.body.text.trim().is_empty()
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the currently focused field.
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        self.paste_text(&text);
        true
    }

    fn paste_text(&mut self, text: &str) {
        if self.field == PageFormField::Body {
            self.body.insert_str(text);
        } else {
            let cleaned = sanitize_single_line(text);
            self.title.insert_str(&cleaned);
        }
        self.error = None;
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
                    self.error = Some("Please enter a page title".to_string());
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
                self.paste_text(s);
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
            Some(id) => format!(" Edit Confluence Page (#{id}, v{}) ", self.prev_version),
            None => format!(
                " New Confluence Page ({}{}) ",
                self.space_key,
                self.parent_id
                    .as_deref()
                    .map(|p| format!(" / child of #{p}"))
                    .unwrap_or_default()
            ),
        };
        let block = modal_block(modal_title, theme, true);
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
            Span::styled(
                " Title: ",
                if title_active {
                    theme.accent_style()
                } else {
                    theme.muted_style()
                },
            ),
            Span::styled(
                &self.title.text,
                if title_active {
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
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
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.card_bg).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Body (Markdown · Ctrl+E $EDITOR) ",
                theme.title(body_active),
            )))
            .border_style(theme.border_style(body_active));
        let body_inner = body_block.inner(chunks[1]);
        let wrapped = wrap_plain_text(&self.body.text, body_inner.width.max(4) as usize);
        let mut body_lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        if body_active {
            if let Some(last) = body_lines.last_mut() {
                last.spans.push(Span::styled("▏", theme.accent_style()));
            }
        }
        frame.render_widget(Paragraph::new(body_lines).block(body_block), chunks[1]);

        let footer = if let Some(err) = &self.error {
            Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme.error),
            ))
        } else {
            Line::from(Span::styled(
                truncate_width(
                    " Tab title/body · Ctrl+V paste · Ctrl+E $EDITOR · Enter confirm · Esc cancel",
                    chunks[2].width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))
        };
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_form_draft_roundtrip_and_validation() {
        let mut form = PageFormOverlay::new_page("ENG", Some("55".into()));
        assert_eq!(form.draft_key(), "page:new:ENG:55");

        assert_eq!(form.handle(&Action::Submit), OverlayOutcome::Handled);
        assert!(form.error.is_some());

        form.handle(&Action::Paste("Release Checklist\n".into()));
        assert_eq!(form.title.text, "Release Checklist");

        form.handle(&Action::NextField);
        form.handle(&Action::Paste("## Steps\n- Verify build".into()));
        assert_eq!(form.body.text, "## Steps\n- Verify build");

        let json = form.to_draft_json();
        let mut restored = PageFormOverlay::new_page("ENG", Some("55".into()));
        assert!(restored.apply_draft_raw(&json));
        assert_eq!(restored.title.text, "Release Checklist");
        assert_eq!(restored.body.text, "## Steps\n- Verify build");
        assert_eq!(
            restored.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit)
        );
    }

    #[test]
    fn page_form_draft_preserves_base_version_and_warns_on_stale_version() {
        use crate::models::PageSummary;
        let v1_detail = PageDetail {
            summary: PageSummary {
                id: "3001".into(),
                space_key: "ENG".into(),
                title: "Spec v1".into(),
                version_number: 1,
                ..Default::default()
            },
            body_raw: r#"{"type":"doc","version":1,"content":[]}"#.into(),
            body_representation: "atlas_doc_format".into(),
            body_markdown: "v1 body".into(),
            ..Default::default()
        };
        let mut form_v1 = PageFormOverlay::edit_page(&v1_detail);
        form_v1.body.set_text("Local v1 draft edit");
        let draft_json = form_v1.to_draft_json();

        let mut v2_detail = v1_detail;
        v2_detail.summary.version_number = 2;
        v2_detail.body_markdown = "Remote v2 edit".into();

        let mut form_v2 = PageFormOverlay::edit_page(&v2_detail);
        assert_eq!(form_v2.prev_version, 2);
        assert!(form_v2.apply_draft_raw(&draft_json));
        // Must NOT silently promote v1 draft to prev_version=2 (which would submit v3).
        assert_eq!(form_v2.prev_version, 1);
        assert_eq!(form_v2.body.text, "Local v1 draft edit");
        assert!(form_v2.error.as_deref().unwrap_or("").contains("v1"));
    }
}
