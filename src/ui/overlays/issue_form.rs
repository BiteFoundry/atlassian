//! Jira issue create (`n`) & edit (`e`) modal with `$EDITOR` (`Ctrl+E`), `Ctrl+V` paste, and draft helpers.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use serde::{Deserialize, Serialize};

use crate::{
    models::{IssueDetail, IssueUpdate},
    ui::{
        app::{AppState, ExternalEditorTarget, UiState},
        doc_view::truncate_width,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_block, modal_pill_muted, modal_pill_primary,
            modal_pill_secondary, read_clipboard_text, register_modal_chrome, render_button_bar,
            sanitize_single_line, wrap_text_with_cursor_line, OverlayOutcome,
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
    /// Site the create/update request goes to (the site of the row or issue
    /// the form was opened from).
    pub site_id: Option<String>,
    pub project: TextBuffer,
    pub issue_type: TextBuffer,
    pub summary: TextBuffer,
    pub priority: TextBuffer,
    pub labels: TextBuffer,
    pub description: TextBuffer,
    pub body_scroll: u16,
    body_max_scroll: Option<u16>,
    follow_cursor: bool,
    pub initial_summary: String,
    pub initial_priority: String,
    pub initial_labels: Vec<String>,
    pub initial_description: String,
    pub field: IssueFormField,
    pub error: Option<String>,
}

impl IssueFormOverlay {
    pub fn new_issue(default_project: &str) -> Self {
        Self {
            edit_key: None,
            site_id: None,
            project: TextBuffer::from_text(default_project),
            issue_type: TextBuffer::from_text("Task"),
            summary: TextBuffer::new(),
            priority: TextBuffer::from_text("Medium"),
            labels: TextBuffer::new(),
            description: TextBuffer::new(),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
            initial_summary: String::new(),
            initial_priority: String::new(),
            initial_labels: Vec::new(),
            initial_description: String::new(),
            field: if default_project.trim().is_empty() {
                IssueFormField::Project
            } else {
                IssueFormField::Summary
            },
            error: None,
        }
    }

    pub fn edit_issue(detail: &IssueDetail) -> Self {
        let initial_priority = detail
            .summary
            .priority
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let initial_labels = detail.summary.labels.clone();
        Self {
            edit_key: Some(detail.summary.key.clone()),
            site_id: None,
            project: TextBuffer::from_text(detail.summary.project_key.clone()),
            issue_type: TextBuffer::from_text(detail.summary.issue_type.name.clone()),
            summary: TextBuffer::from_text(detail.summary.summary.clone()),
            priority: TextBuffer::from_text(initial_priority.clone()),
            labels: TextBuffer::from_text(initial_labels.join(", ")),
            description: TextBuffer::from_text(detail.description_markdown.clone()),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
            initial_summary: detail.summary.summary.clone(),
            initial_priority,
            initial_labels,
            initial_description: detail.description_markdown.clone(),
            field: IssueFormField::Summary,
            error: None,
        }
    }

    /// Focuses field `0..=5` on click; clicking `Type` (1) while creating also cycles issue type.
    pub fn click_field(&mut self, field_idx: usize) {
        let is_edit = self.edit_key.is_some();
        match field_idx {
            0 if !is_edit => self.field = IssueFormField::Project,
            1 if !is_edit => {
                self.field = IssueFormField::IssueType;
                self.cycle_issue_type();
            }
            2 => self.field = IssueFormField::Summary,
            3 => self.field = IssueFormField::Priority,
            4 => self.field = IssueFormField::Labels,
            5 => {
                self.field = IssueFormField::Description;
                self.follow_cursor = true;
            }
            _ => {}
        }
    }

    fn scroll_body(&mut self, delta: i32) {
        self.follow_cursor = false;
        let cap = self
            .body_max_scroll
            .unwrap_or_else(|| self.description.text.lines().count().saturating_sub(1) as u16);
        if delta < 0 {
            self.body_scroll = self.body_scroll.saturating_sub(delta.unsigned_abs() as u16);
        } else if delta > 0 {
            self.body_scroll = self.body_scroll.saturating_add(delta as u16).min(cap);
        }
    }

    /// Builds a minimal Jira `IssueUpdate` containing only fields actually changed since the edit
    /// form opened, preventing unmodified description/priority/labels from overwriting remote edits.
    pub fn build_edit_patch(&self) -> IssueUpdate {
        let cur_summary = self.summary.text.trim();
        let cur_priority = self.priority.text.trim();
        let cur_labels = self.parsed_labels();

        IssueUpdate {
            summary: (cur_summary != self.initial_summary.trim())
                .then(|| cur_summary.to_string()),
            description_markdown: (self.description.text != self.initial_description)
                .then(|| self.description.text.clone()),
            priority: (!cur_priority.is_empty() && cur_priority != self.initial_priority.trim())
                .then(|| cur_priority.to_string()),
            assignee_id: None,
            labels: (cur_labels != self.initial_labels).then_some(cur_labels),
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
            self.follow_cursor = true;
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
                if self.field == IssueFormField::Description {
                    self.description.insert_char('\n');
                    self.follow_cursor = true;
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
                let is_desc = self.field == IssueFormField::Description;
                self.active_buffer_mut().insert_char(*ch);
                if is_desc {
                    self.follow_cursor = true;
                }
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                let is_desc = self.field == IssueFormField::Description;
                self.active_buffer_mut().backspace();
                if is_desc {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::Delete => {
                let is_desc = self.field == IssueFormField::Description;
                self.active_buffer_mut().delete();
                if is_desc {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                if self.field == IssueFormField::IssueType && !is_edit {
                    self.cycle_issue_type();
                } else {
                    let is_desc = self.field == IssueFormField::Description;
                    self.active_buffer_mut().move_left();
                    if is_desc {
                        self.follow_cursor = true;
                    }
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                if self.field == IssueFormField::IssueType && !is_edit {
                    self.cycle_issue_type();
                } else {
                    let is_desc = self.field == IssueFormField::Description;
                    self.active_buffer_mut().move_right();
                    if is_desc {
                        self.follow_cursor = true;
                    }
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                let is_desc = self.field == IssueFormField::Description;
                self.active_buffer_mut().move_home();
                if is_desc {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                let is_desc = self.field == IssueFormField::Description;
                self.active_buffer_mut().move_end();
                if is_desc {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorUp => {
                if self.field == IssueFormField::Description {
                    self.description.move_up();
                    self.follow_cursor = true;
                } else {
                    self.field = self.field.prev(is_edit);
                }
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                if self.field == IssueFormField::Description {
                    self.description.move_down();
                    self.follow_cursor = true;
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
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 8 {
            return;
        }

        let has_err = self.error.is_some() && inner.height >= 10;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if has_err {
                vec![
                    Constraint::Length(5),
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]
            } else {
                vec![
                    Constraint::Length(5),
                    Constraint::Min(3),
                    Constraint::Length(1),
                ]
            })
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
        let type_display = if is_edit {
            self.issue_type.text.clone()
        } else {
            format!("◀ {} ▶", self.issue_type.text)
        };
        let top_lines = vec![
            field_line(IssueFormField::Project, "Project", &self.project.text, is_edit),
            field_line(IssueFormField::IssueType, "Type (←/→)", &type_display, is_edit),
            field_line(IssueFormField::Summary, "Summary", &self.summary.text, false),
            field_line(IssueFormField::Priority, "Priority", &self.priority.text, false),
            field_line(IssueFormField::Labels, "Labels (,)", &self.labels.text, false),
        ];
        frame.render_widget(Paragraph::new(top_lines), chunks[0]);
        for idx in 0..5usize {
            hitmap.add_line(
                chunks[0].x,
                chunks[0].y + idx as u16,
                chunks[0].width,
                HitTarget::OverlayField(idx),
            );
        }

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
        frame.render_widget(desc_block, chunks[1]);
        hitmap.add(chunks[1], HitTarget::OverlayScrollArea);
        hitmap.add(chunks[1], HitTarget::OverlayField(5));

        if desc_inner.width > 0 && desc_inner.height > 0 {
            let view_h = desc_inner.height as usize;
            let initial_w = desc_inner.width.max(4) as usize;
            let (wrapped_check, _) =
                wrap_text_with_cursor_line(&self.description.text, self.description.cursor, initial_w);
            let show_scrollbar = wrapped_check.len() > view_h && desc_inner.width > 2;
            let text_w = if show_scrollbar {
                desc_inner.width.saturating_sub(1).max(4) as usize
            } else {
                initial_w
            };
            let (wrapped, cursor_line) =
                wrap_text_with_cursor_line(&self.description.text, self.description.cursor, text_w);
            let max_scroll = wrapped.len().saturating_sub(view_h) as u16;
            self.body_max_scroll = Some(max_scroll);

            if desc_active && self.follow_cursor {
                if cursor_line < self.body_scroll {
                    self.body_scroll = cursor_line;
                } else if view_h > 0 && cursor_line >= self.body_scroll.saturating_add(view_h as u16) {
                    self.body_scroll = cursor_line.saturating_sub(view_h as u16 - 1);
                }
                self.follow_cursor = false;
            }
            self.body_scroll = self.body_scroll.min(max_scroll);

            let mut desc_lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
            if desc_active {
                if let Some(last) = desc_lines.last_mut() {
                    last.spans.push(Span::styled("▏", theme.accent_style()));
                }
            }
            let text_area = if show_scrollbar {
                Rect::new(
                    desc_inner.x,
                    desc_inner.y,
                    desc_inner.width.saturating_sub(1),
                    desc_inner.height,
                )
            } else {
                desc_inner
            };
            frame.render_widget(
                Paragraph::new(desc_lines).scroll((self.body_scroll, 0)),
                text_area,
            );
            if show_scrollbar {
                let sb_area = Rect::new(
                    desc_inner.right().saturating_sub(1),
                    desc_inner.y,
                    1,
                    desc_inner.height,
                );
                let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                    .position(self.body_scroll as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        }

        let btn_area = if has_err {
            if let Some(err) = &self.error {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate_width(&format!(" Error: {err}"), chunks[2].width as usize),
                        Style::default().fg(theme.error),
                    ))),
                    chunks[2],
                );
            }
            chunks[3]
        } else {
            chunks[2]
        };

        render_button_bar(
            frame,
            btn_area,
            &[
                ("[Ctrl+S] Submit", ButtonId::OverlaySubmit, modal_pill_primary(theme)),
                ("[Ctrl+E] $EDITOR", ButtonId::OverlayExternalEditor, modal_pill_secondary(theme)),
                ("[Esc] Cancel", ButtonId::OverlayCancel, modal_pill_muted(theme)),
            ],
            Some("· Tab next · Ctrl+V paste"),
            theme,
            hitmap,
        );
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

    #[test]
    fn issue_form_edit_patch_sends_only_modified_fields() {
        use crate::models::{IssuePriority, IssueStatus, IssueSummary, IssueType, StatusCategory};
        let detail = IssueDetail {
            summary: IssueSummary {
                id: "100".into(),
                key: "ENG-7".into(),
                project_key: "ENG".into(),
                summary: "Original title".into(),
                status: IssueStatus {
                    id: "1".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
                priority: Some(IssuePriority {
                    id: "2".into(),
                    name: "High".into(),
                }),
                issue_type: IssueType {
                    id: "10001".into(),
                    name: "Story".into(),
                    subtask: false,
                    epic: false,
                },
                labels: vec!["backend".into(), "sync".into()],
                ..Default::default()
            },
            description_markdown: "Rich body markdown".into(),
            ..Default::default()
        };

        let mut form = IssueFormOverlay::edit_issue(&detail);
        let empty_patch = form.build_edit_patch();
        assert_eq!(empty_patch, IssueUpdate::default());

        form.summary.set_text("Updated title only");
        let patch = form.build_edit_patch();
        assert_eq!(patch.summary.as_deref(), Some("Updated title only"));
        assert!(patch.description_markdown.is_none());
        assert!(patch.priority.is_none());
        assert!(patch.labels.is_none());
        assert!(patch.assignee_id.is_none());
    }
}
