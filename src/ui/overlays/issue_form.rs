//! Jira issue create (`n`) & edit (`e`) modal with `$EDITOR` (`Ctrl+E`), `Ctrl+V` paste, and draft helpers.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};

use crate::{
    models::IssueDetail,
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
pub enum IssueFormField {
    Project,
    IssueType,
    Summary,
    Priority,
    Labels,
    Description,
}

impl IssueFormField {
    const ALL: [Self; 6] = [
        Self::Project,
        Self::IssueType,
        Self::Summary,
        Self::Priority,
        Self::Labels,
        Self::Description,
    ];

    fn next(self, editing: bool) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        let mut next = (idx + 1) % Self::ALL.len();
        if editing && (Self::ALL[next] == Self::Project || Self::ALL[next] == Self::IssueType) {
            next = 2; // Summary
        }
        Self::ALL[next]
    }

    fn prev(self, editing: bool) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        let mut prev = (idx + Self::ALL.len() - 1) % Self::ALL.len();
        if editing && (Self::ALL[prev] == Self::Project || Self::ALL[prev] == Self::IssueType) {
            prev = Self::ALL.len() - 1;
        }
        Self::ALL[prev]
    }
}

/// Serializable draft payload for persisting unsent Jira issue forms in `DraftStore`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueFormDraft {
    pub project: String,
    pub issue_type: String,
    pub summary: String,
    pub priority: String,
    pub labels: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct IssueFormOverlay {
    /// `None` when creating a new issue; `Some(key)` when editing an existing issue.
    pub edit_key: Option<String>,
    pub project: TextBuffer,
    pub issue_type: TextBuffer,
    pub summary: TextBuffer,
    pub priority: TextBuffer,
    pub labels: TextBuffer,
    pub description: TextBuffer,
    pub field: IssueFormField,
    pub error: Option<String>,
}

impl IssueFormOverlay {
    pub fn new_issue(default_project: &str) -> Self {
        Self {
            edit_key: None,
            project: TextBuffer::from_text(default_project),
            issue_type: TextBuffer::from_text("Task"),
            summary: TextBuffer::new(),
            priority: TextBuffer::from_text("Medium"),
            labels: TextBuffer::new(),
            description: TextBuffer::new(),
            field: if default_project.trim().is_empty() {
                IssueFormField::Project
            } else {
                IssueFormField::Summary
            },
            error: None,
        }
    }

    pub fn edit_issue(detail: &IssueDetail) -> Self {
        Self {
            edit_key: Some(detail.summary.key.clone()),
            project: TextBuffer::from_text(detail.summary.project_key.clone()),
            issue_type: TextBuffer::from_text(detail.summary.issue_type.name.clone()),
            summary: TextBuffer::from_text(detail.summary.summary.clone()),
            priority: TextBuffer::from_text(
                detail
                    .summary
                    .priority
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
            ),
            labels: TextBuffer::from_text(detail.summary.labels.join(", ")),
            description: TextBuffer::from_text(detail.description_markdown.clone()),
            field: IssueFormField::Summary,
            error: None,
        }
    }

    /// Returns a stable `DraftStore` key for this create/edit issue form.
    pub fn draft_key(&self) -> String {
        if let Some(key) = &self.edit_key {
            format!("issue:edit:{key}")
        } else {
            let proj = self.project.text.trim().to_ascii_uppercase();
            if proj.is_empty() {
                "issue:new".to_string()
            } else {
                format!("issue:new:{proj}")
            }
        }
    }

    pub fn to_draft(&self) -> IssueFormDraft {
        IssueFormDraft {
            project: self.project.text.clone(),
            issue_type: self.issue_type.text.clone(),
            summary: self.summary.text.clone(),
            priority: self.priority.text.clone(),
            labels: self.labels.text.clone(),
            description: self.description.text.clone(),
        }
    }

    pub fn to_draft_json(&self) -> String {
        serde_json::to_string(&self.to_draft()).unwrap_or_else(|_| self.description.text.clone())
    }

    /// Restores a draft from JSON (produced by `to_draft_json`) or plain Markdown text.
    pub fn apply_draft_raw(&mut self, raw: &str) -> bool {
        if raw.trim().is_empty() {
            return false;
        }
        if let Ok(draft) = serde_json::from_str::<IssueFormDraft>(raw) {
            if self.edit_key.is_none() {
                if !draft.project.trim().is_empty() {
                    self.project.set_text(draft.project);
                }
                if !draft.issue_type.trim().is_empty() {
                    self.issue_type.set_text(draft.issue_type);
                }
            }
            self.summary.set_text(draft.summary);
            if !draft.priority.trim().is_empty() {
                self.priority.set_text(draft.priority);
            }
            self.labels.set_text(draft.labels);
            self.description.set_text(draft.description);
            return true;
        }
        self.description.set_text(raw);
        true
    }

    pub fn has_unsent_content(&self) -> bool {
        !self.summary.text.trim().is_empty()
            || !self.description.text.trim().is_empty()
            || !self.labels.text.trim().is_empty()
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
        if self.field == IssueFormField::Description {
            self.description.insert_str(text);
        } else {
            let cleaned = sanitize_single_line(text);
            self.active_buffer_mut().insert_str(&cleaned);
        }
        self.error = None;
    }

    fn active_buffer_mut(&mut self) -> &mut TextBuffer {
        match self.field {
            IssueFormField::Project => &mut self.project,
            IssueFormField::IssueType => &mut self.issue_type,
            IssueFormField::Summary => &mut self.summary,
            IssueFormField::Priority => &mut self.priority,
            IssueFormField::Labels => &mut self.labels,
            IssueFormField::Description => &mut self.description,
        }
    }

    pub fn cycle_issue_type(&mut self) {
        const TYPES: [&str; 4] = ["Task", "Bug", "Story", "Epic"];
        let cur = self.issue_type.text.trim();
        let idx = TYPES
            .iter()
            .position(|t| t.eq_ignore_ascii_case(cur))
            .map(|i| (i + 1) % TYPES.len())
            .unwrap_or(0);
        self.issue_type.set_text(TYPES[idx]);
    }

    pub fn parsed_labels(&self) -> Vec<String> {
        self.labels
            .text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let is_edit = self.edit_key.is_some();
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::NextField => {
                self.field = self.field.next(is_edit);
                OverlayOutcome::Handled
            }
            Action::PrevField => {
                self.field = self.field.prev(is_edit);
                OverlayOutcome::Handled
            }
            Action::ExternalEditor => OverlayOutcome::Dispatch(Action::ExternalEditor),
            Action::InsertNewline => {
                if self.field == IssueFormField::Description {
                    self.description.insert_char('\n');
                }
                OverlayOutcome::Handled
            }
            Action::Submit => {
                if self.edit_key.is_none() && self.project.text.trim().is_empty() {
                    self.error = Some("Please enter a project key (e.g. PROJ)".to_string());
                    self.field = IssueFormField::Project;
                    return OverlayOutcome::Handled;
                }
                if self.summary.text.trim().is_empty() {
                    self.error = Some("Please enter an issue summary".to_string());
                    self.field = IssueFormField::Summary;
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
                if self.field == IssueFormField::IssueType && !is_edit {
                    self.cycle_issue_type();
                } else {
                    self.active_buffer_mut().move_left();
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                if self.field == IssueFormField::IssueType && !is_edit {
                    self.cycle_issue_type();
                } else {
                    self.active_buffer_mut().move_right();
                }
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
                if self.field == IssueFormField::Description {
                    self.description.move_up();
                } else {
                    self.field = self.field.prev(is_edit);
                }
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                if self.field == IssueFormField::Description {
                    self.description.move_down();
                } else {
                    self.field = self.field.next(is_edit);
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

    pub fn editor_target(&self) -> (ExternalEditorTarget, String) {
        (
            ExternalEditorTarget::IssueFormDescription,
            self.description.text.clone(),
        )
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
            80.min(area.width.saturating_sub(4)).max(34),
            24.min(area.height.saturating_sub(2)).max(14),
        );
        frame.render_widget(Clear, rect);

        let title = match &self.edit_key {
            Some(k) => format!(" Edit Issue — {k} "),
            None => " New Jira Issue ".to_string(),
        };
        let block = modal_block(title, theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 8 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(inner);

        let field_line = |f: IssueFormField, label: &str, val: &str, locked: bool| -> Line<'static> {
            let active = self.field == f;
            let lbl_style = if active {
                theme.accent_style()
            } else {
                theme.muted_style()
            };
            let cursor = if active && !locked { "▏" } else { "" };
            Line::from(vec![
                Span::styled(format!(" {label:<12}: "), lbl_style),
                Span::styled(
                    val.to_string(),
                    if locked {
                        theme.muted_style()
                    } else if active {
                        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg)
                    },
                ),
                Span::styled(cursor, theme.accent_style()),
            ])
        };

        let is_edit = self.edit_key.is_some();
        let top_lines = vec![
            field_line(IssueFormField::Project, "Project", &self.project.text, is_edit),
            field_line(
                IssueFormField::IssueType,
                "Type (←/→)",
                &self.issue_type.text,
                is_edit,
            ),
            field_line(IssueFormField::Summary, "Summary", &self.summary.text, false),
            field_line(IssueFormField::Priority, "Priority", &self.priority.text, false),
            field_line(IssueFormField::Labels, "Labels (,)", &self.labels.text, false),
        ];
        frame.render_widget(Paragraph::new(top_lines), chunks[0]);

        let desc_active = self.field == IssueFormField::Description;
        let desc_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.card_bg).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Description (Markdown · Ctrl+E $EDITOR) ",
                theme.title(desc_active),
            )))
            .border_style(theme.border_style(desc_active));
        let desc_inner = desc_block.inner(chunks[1]);
        let wrapped = wrap_plain_text(&self.description.text, desc_inner.width.max(4) as usize);
        let mut desc_lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        if desc_active {
            if let Some(last) = desc_lines.last_mut() {
                last.spans.push(Span::styled("▏", theme.accent_style()));
            }
        }
        frame.render_widget(Paragraph::new(desc_lines).block(desc_block), chunks[1]);

        let footer = if let Some(err) = &self.error {
            Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme.error),
            ))
        } else {
            Line::from(Span::styled(
                truncate_width(
                    " Tab next · Ctrl+V paste · Ctrl+E $EDITOR · Enter confirm · Esc cancel",
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
    fn issue_form_draft_roundtrip_and_validation() {
        let mut form = IssueFormOverlay::new_issue("eng");
        assert_eq!(form.draft_key(), "issue:new:ENG");

        // Missing summary fails validation
        assert_eq!(form.handle(&Action::Submit), OverlayOutcome::Handled);
        assert!(form.error.is_some());

        form.handle(&Action::Paste("Ship Settings modal\n".into()));
        assert_eq!(form.summary.text, "Ship Settings modal");
        assert!(form.error.is_none());

        form.field = IssueFormField::Description;
        form.handle(&Action::Paste("Line 1\nLine 2".into()));
        assert_eq!(form.description.text, "Line 1\nLine 2");

        let json = form.to_draft_json();
        let mut restored = IssueFormOverlay::new_issue("ENG");
        assert!(restored.apply_draft_raw(&json));
        assert_eq!(restored.summary.text, "Ship Settings modal");
        assert_eq!(restored.description.text, "Line 1\nLine 2");
        assert_eq!(
            restored.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit)
        );
    }
}
