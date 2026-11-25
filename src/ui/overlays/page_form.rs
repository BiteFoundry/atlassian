//! Confluence page create (`n`) & edit (`e`) modal with `$EDITOR` (`Ctrl+E`), `Ctrl+V` paste, and draft helpers.

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
    models::PageDetail,
    ui::{
        app::{AppState, ExternalEditorTarget, UiState},
        doc_view::truncate_width,
        filter::is_personal_space_key,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_block, modal_pill_muted, modal_pill_primary,
            modal_pill_secondary, read_clipboard_text, register_modal_chrome, render_button_bar,
            sanitize_single_line, wrap_text_with_cursor_line, OverlayOutcome,
        },
        text_edit::TextBuffer,
        theme::Theme,
    },
};

/// Footer prompt after the first submit into someone else's personal space.
const FOREIGN_SPACE_CONFIRM: &str = "Press again to create in someone else's personal space";

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
    /// Site the create/update request goes to.
    pub site_id: Option<String>,
    pub prev_version: i32,
    pub space_key: String,
    /// Display name of the target space, shown in the personal space banner.
    pub space_name: Option<String>,
    /// New page in a personal space (`~` key or `personal` space type).
    pub personal_space: bool,
    /// Whether that personal space is the signed-in user's own; `None` when unknown.
    pub personal_owner_is_me: Option<bool>,
    /// Set by the first submit into someone else's personal space; the next submits.
    foreign_space_confirm_pending: bool,
    pub parent_id: Option<String>,
    pub title: TextBuffer,
    pub body: TextBuffer,
    pub body_scroll: u16,
    body_max_scroll: Option<u16>,
    follow_cursor: bool,
    pub initial_title: String,
    pub initial_body: String,
    pub initial_body_raw: Option<String>,
    pub initial_body_representation: Option<String>,
    pub field: PageFormField,
    pub error: Option<String>,
}

impl PageFormOverlay {
    pub fn new_page(space_key: impl Into<String>, parent_id: Option<String>) -> Self {
        let space_key = space_key.into();
        Self {
            edit_page_id: None,
            site_id: None,
            prev_version: 0,
            personal_space: is_personal_space_key(&space_key),
            space_key,
            space_name: None,
            personal_owner_is_me: None,
            foreign_space_confirm_pending: false,
            parent_id,
            title: TextBuffer::new(),
            body: TextBuffer::new(),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
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
            site_id: None,
            prev_version: detail.summary.version_number,
            space_key: detail.summary.space_key.clone(),
            space_name: None,
            personal_space: false,
            personal_owner_is_me: None,
            foreign_space_confirm_pending: false,
            parent_id: detail.summary.parent_id.clone(),
            title: TextBuffer::from_text(detail.summary.title.clone()),
            body: TextBuffer::from_text(detail.body_markdown.clone()),
            body_scroll: 0,
            body_max_scroll: None,
            follow_cursor: true,
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

    /// Focuses field `0` (Title) or `1` (Body) on mouse click.
    pub fn click_field(&mut self, field_idx: usize) {
        match field_idx {
            0 => self.field = PageFormField::Title,
            1 => {
                self.field = PageFormField::Body;
                self.follow_cursor = true;
            }
            _ => {}
        }
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

    /// Banner for a new page in a personal space: its text, and `true` when it is a
    /// warning (someone else's space, or an owner that cannot be told) rather than
    /// the user's own space.
    pub fn personal_space_banner(&self) -> Option<(String, bool)> {
        if self.edit_page_id.is_some() || !self.personal_space {
            return None;
        }
        let key = self.space_key.trim();
        let name = self
            .space_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        Some(if self.personal_owner_is_me == Some(true) {
            (
                format!("Personal space {key}{name} — your own personal space."),
                false,
            )
        } else {
            (
                format!(
                    "⚠ Personal space {key}{name} — pages here are visible in that person's space."
                ),
                true,
            )
        })
    }

    /// New page in a personal space known to belong to someone else.
    fn needs_foreign_space_confirm(&self) -> bool {
        self.edit_page_id.is_none()
            && self.personal_space
            && self.personal_owner_is_me == Some(false)
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
            self.follow_cursor = true;
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
                if self.field == PageFormField::Body {
                    self.body.insert_char('\n');
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::Submit => {
                if self.title.text.trim().is_empty() {
                    self.error = Some("Please enter a page title".to_string());
                    self.field = PageFormField::Title;
                    return OverlayOutcome::Handled;
                }
                if self.needs_foreign_space_confirm() && !self.foreign_space_confirm_pending {
                    self.foreign_space_confirm_pending = true;
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::Dispatch(Action::Submit)
            }
            Action::Input(ch) => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().insert_char(*ch);
                if is_body {
                    self.follow_cursor = true;
                }
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().backspace();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::Delete => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().delete();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().move_left();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().move_right();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().move_home();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                let is_body = self.field == PageFormField::Body;
                self.active_buffer_mut().move_end();
                if is_body {
                    self.follow_cursor = true;
                }
                OverlayOutcome::Handled
            }
            Action::CursorUp => {
                if self.field == PageFormField::Body {
                    self.body.move_up();
                    self.follow_cursor = true;
                } else {
                    self.field = PageFormField::Title;
                }
                OverlayOutcome::Handled
            }
            Action::CursorDown => {
                if self.field == PageFormField::Body {
                    self.body.move_down();
                    self.follow_cursor = true;
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
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        self.render(frame, area, &ui.theme, hitmap);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, hitmap: &mut HitMap) {
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
        register_modal_chrome(rect, frame, theme, hitmap, true);
        let banner = self.personal_space_banner();
        let banner_h = u16::from(banner.is_some());
        if inner.width == 0 || inner.height < 6 + banner_h {
            return;
        }

        let has_notice = (self.error.is_some() || self.foreign_space_confirm_pending)
            && inner.height >= 7 + banner_h;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if has_notice {
                vec![
                    Constraint::Length(banner_h),
                    Constraint::Length(2),
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]
            } else {
                vec![
                    Constraint::Length(banner_h),
                    Constraint::Length(2),
                    Constraint::Min(3),
                    Constraint::Length(1),
                ]
            })
            .split(inner);
        let chunks = &rows[1..];

        if let Some((text, warning)) = banner {
            let style = if warning {
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_width(&format!(" {text}"), rows[0].width as usize),
                    style,
                )),
                rows[0],
            );
        }

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
        hitmap.add_line(chunks[0].x, chunks[0].y, chunks[0].width, HitTarget::OverlayField(0));

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
        frame.render_widget(body_block, chunks[1]);
        hitmap.add(chunks[1], HitTarget::OverlayScrollArea);
        hitmap.add(chunks[1], HitTarget::OverlayField(1));

        if body_inner.width > 0 && body_inner.height > 0 {
            let view_h = body_inner.height as usize;
            let initial_w = body_inner.width.max(4) as usize;
            let (wrapped_check, _) =
                wrap_text_with_cursor_line(&self.body.text, self.body.cursor, initial_w);
            let show_scrollbar = wrapped_check.len() > view_h && body_inner.width > 2;
            let text_w = if show_scrollbar {
                body_inner.width.saturating_sub(1).max(4) as usize
            } else {
                initial_w
            };
            let (wrapped, cursor_line) =
                wrap_text_with_cursor_line(&self.body.text, self.body.cursor, text_w);
            let max_scroll = wrapped.len().saturating_sub(view_h) as u16;
            self.body_max_scroll = Some(max_scroll);

            if body_active && self.follow_cursor {
                if cursor_line < self.body_scroll {
                    self.body_scroll = cursor_line;
                } else if view_h > 0 && cursor_line >= self.body_scroll.saturating_add(view_h as u16) {
                    self.body_scroll = cursor_line.saturating_sub(view_h as u16 - 1);
                }
                self.follow_cursor = false;
            }
            self.body_scroll = self.body_scroll.min(max_scroll);

            let mut body_lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
            if body_active {
                if let Some(last) = body_lines.last_mut() {
                    last.spans.push(Span::styled("▏", theme.accent_style()));
                }
            }
            let text_area = if show_scrollbar {
                Rect::new(
                    body_inner.x,
                    body_inner.y,
                    body_inner.width.saturating_sub(1),
                    body_inner.height,
                )
            } else {
                body_inner
            };
            frame.render_widget(
                Paragraph::new(body_lines).scroll((self.body_scroll, 0)),
                text_area,
            );
            if show_scrollbar {
                let sb_area = Rect::new(
                    body_inner.right().saturating_sub(1),
                    body_inner.y,
                    1,
                    body_inner.height,
                );
                let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                    .position(self.body_scroll as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        }

        let btn_area = if has_notice {
            let notice_line = if let Some(err) = &self.error {
                Line::from(Span::styled(
                    truncate_width(&format!(" Error: {err}"), chunks[2].width as usize),
                    Style::default().fg(theme.error),
                ))
            } else {
                Line::from(Span::styled(
                    truncate_width(
                        &format!(" {FOREIGN_SPACE_CONFIRM}"),
                        chunks[2].width as usize,
                    ),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ))
            };
            frame.render_widget(Paragraph::new(notice_line), chunks[2]);
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
            Some("· Tab title/body · Ctrl+V paste"),
            theme,
            hitmap,
        );
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

    fn render_text(form: &PageFormOverlay) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let theme = Theme::midnight_dark();
        let mut form = form.clone();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|f| form.render(f, f.area(), &theme, &mut hitmap))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn personal_space_banner_shown_for_tilde_key() {
        let mut form = PageFormOverlay::new_page("~jdoe", None);
        form.space_name = Some("John Doe".into());
        assert!(form.personal_space);

        let (text, warning) = form.personal_space_banner().expect("banner for a ~ key");
        assert_eq!(
            text,
            "⚠ Personal space ~jdoe (John Doe) — pages here are visible in that person's space."
        );
        assert!(warning, "unknown owner is shown as a warning");
        let screen = render_text(&form);
        assert!(
            screen.contains("⚠ Personal space ~jdoe (John Doe)"),
            "banner under the modal title:\n{screen}"
        );

        // The user's own personal space: muted banner, no warning sign.
        form.personal_owner_is_me = Some(true);
        let (text, warning) = form.personal_space_banner().unwrap();
        assert!(!warning);
        assert_eq!(
            text,
            "Personal space ~jdoe (John Doe) — your own personal space."
        );
        let screen = render_text(&form);
        assert!(screen.contains("Personal space ~jdoe (John Doe) — your own personal space."));
        assert!(!screen.contains('⚠'));
    }

    #[test]
    fn foreign_personal_space_requires_second_confirm() {
        let mut form = PageFormOverlay::new_page("~asmith", None);
        form.personal_owner_is_me = Some(false);
        form.handle(&Action::Paste("Meeting notes".into()));

        assert_eq!(form.handle(&Action::Submit), OverlayOutcome::Handled);
        let screen = render_text(&form);
        assert!(
            screen.contains(FOREIGN_SPACE_CONFIRM),
            "confirm prompt:\n{screen}"
        );
        assert_eq!(
            form.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit),
            "second submit creates the page"
        );

        // Own or unknown owner: a single submit, as for any space.
        for owner in [Some(true), None] {
            let mut form = PageFormOverlay::new_page("~asmith", None);
            form.personal_owner_is_me = owner;
            form.handle(&Action::Paste("Meeting notes".into()));
            assert_eq!(
                form.handle(&Action::Submit),
                OverlayOutcome::Dispatch(Action::Submit),
                "owner {owner:?}"
            );
        }
    }

    #[test]
    fn no_banner_for_regular_space_or_edit() {
        let form = PageFormOverlay::new_page("ENG", Some("55".into()));
        assert!(form.personal_space_banner().is_none());
        assert!(!render_text(&form).contains("Personal space"));

        // Editing a page that lives in someone else's personal space.
        let detail = PageDetail {
            summary: crate::models::PageSummary {
                id: "77".into(),
                space_key: "~asmith".into(),
                title: "Their page".into(),
                version_number: 4,
                ..Default::default()
            },
            body_markdown: "body".into(),
            ..Default::default()
        };
        let mut edit = PageFormOverlay::edit_page(&detail);
        edit.personal_owner_is_me = Some(false);
        assert!(edit.personal_space_banner().is_none());
        assert!(!render_text(&edit).contains("Personal space"));
        assert_eq!(
            edit.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit),
            "edits need no extra confirmation"
        );
    }
}
