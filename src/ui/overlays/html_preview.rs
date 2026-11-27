//! Interactive native HTML viewer modal (`H` / `[H: HTML]` / HTML attachments) backed by
//! `bitefoundry_html::HtmlViewSession` (0 MB idle on static HTML, on-demand 8 MiB QuickJS-NG
//! only when `<script>` or `on*` handlers are present, dropped immediately on close) and
//! lazy localhost Axum browser serving (`o` via `bitefoundry_html::NativeHtmlServer`).

use std::{collections::HashMap, fmt, sync::Arc};

use bitefoundry_html::{ContentKind, HtmlEffect, HtmlTheme, HtmlViewSession, SourceKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use scraper::{ElementRef, Html, Node};
use unicode_width::UnicodeWidthStr;

use crate::{
    adf,
    img_preview::{
        self, FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::{IssueDetail, PageDetail},
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::HitMap,
        keymap::Action,
        overlays::{
            centered_rect, image_preview::PendingPixelGraphic, read_clipboard_text,
            register_modal_chrome, sanitize_single_line, OverlayOutcome,
        },
        theme::Theme,
    },
};

/// Deferred side-effect intent emitted by [`HtmlPreviewOverlay`] for coordinator handling in `action.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlPreviewCommand {
    /// Serve the current HTML document via `AppState::html_server` (`NativeHtmlServer`) and open in browser.
    ServeInBrowser { title: String, html: String },
    /// Open an external link clicked/activated inside the HTML document.
    OpenUrl(String),
    /// Surface a status/alert message as a TUI toast.
    Toast(String),
}

/// Interactive HTML preview modal backed by `bitefoundry_html::HtmlViewSession`.
pub struct HtmlPreviewOverlay {
    pub title: String,
    pub source_url: String,
    pub session: Option<HtmlViewSession>,
    pub loading: bool,
    pub error: Option<String>,
    pub last_served_url: Option<String>,
    pub pending_commands: Vec<HtmlPreviewCommand>,
    /// Last rendered HTML viewport area (used to map screen clicks `(row, col)` to `click_at(line, col)`).
    pub last_content_rect: Rect,
    /// Clamped scroll offset from the last frame (used for `click_at` vertical line math).
    pub last_scroll_y: u16,
    last_mode_pill_rect: Rect,
    last_browser_pill_rect: Rect,
    /// Decoded/encoded inline graphic cache keyed by `(node_id, width_cols, protocol)`.
    /// Also tracks source `Arc<[u8]>` identity so dynamic JS canvas/SVG updates invalidate cleanly.
    graphic_cache: HashMap<GraphicCacheKey, GraphicCacheEntry>,
}

type GraphicCacheKey = (usize, u16, TerminalImageProtocol);
type GraphicCacheEntry = (Arc<[u8]>, RenderedTerminalImage);

impl Drop for HtmlPreviewOverlay {
    fn drop(&mut self) {
        self.graphic_cache.clear();
    }
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.title)
            .field("source_url", &self.source_url)
            .field("has_session", &self.session.is_some())
            .field("raw_mode", &self.is_raw_mode())
            .field("js_active", &self.is_js_active())
            .field("loading", &self.loading)
            .field("error", &self.error)
            .field("last_served_url", &self.last_served_url)
            .finish()
    }
}

impl HtmlPreviewOverlay {
    /// Opens an interactive HTML preview immediately for the given HTML markup.
    pub fn new(title: impl Into<String>, html: impl Into<String>) -> Self {
        let title_str = title.into();
        let html_str = html.into();
        let title_opt = if title_str.trim().is_empty() {
            None
        } else {
            Some(title_str.clone())
        };
        let mut session = HtmlViewSession::new(title_opt, html_str);
        let init_effects = session.take_pending_effects();
        let resolved_title = session
            .title()
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                if title_str.trim().is_empty() {
                    "HTML Document".to_string()
                } else {
                    title_str
                }
            });

        let mut overlay = Self {
            title: resolved_title,
            source_url: String::new(),
            session: Some(session),
            loading: false,
            error: None,
            last_served_url: None,
            pending_commands: Vec::new(),
            last_content_rect: Rect::default(),
            last_scroll_y: 0,
            last_mode_pill_rect: Rect::default(),
            last_browser_pill_rect: Rect::default(),
            graphic_cache: HashMap::new(),
        };
        overlay.apply_effects(init_effects);
        overlay
    }

    /// Opens an interactive Markdown preview immediately backed by `HtmlViewSession::new_markdown`.
    pub fn new_markdown(title: impl Into<String>, raw_md: impl Into<String>) -> Self {
        let title_str = title.into();
        let md_str = raw_md.into();
        let title_opt = if title_str.trim().is_empty() {
            None
        } else {
            Some(title_str.clone())
        };
        let session = HtmlViewSession::new_markdown(title_opt, md_str);
        let resolved_title = session
            .title()
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                if title_str.trim().is_empty() {
                    "Markdown Document".to_string()
                } else {
                    title_str
                }
            });

        Self {
            title: resolved_title,
            source_url: String::new(),
            session: Some(session),
            loading: false,
            error: None,
            last_served_url: None,
            pending_commands: Vec::new(),
            last_content_rect: Rect::default(),
            last_scroll_y: 0,
            last_mode_pill_rect: Rect::default(),
            last_browser_pill_rect: Rect::default(),
            graphic_cache: HashMap::new(),
        }
    }

    /// Creates a placeholder overlay while an HTML or Markdown attachment is being downloaded asynchronously.
    pub fn loading(title: impl Into<String>, source_url: impl Into<String>) -> Self {
        let title_str = title.into();
        Self {
            title: if title_str.trim().is_empty() {
                "HTML Attachment".to_string()
            } else {
                title_str
            },
            source_url: source_url.into(),
            session: None,
            loading: true,
            error: None,
            last_served_url: None,
            pending_commands: Vec::new(),
            last_content_rect: Rect::default(),
            last_scroll_y: 0,
            last_mode_pill_rect: Rect::default(),
            last_browser_pill_rect: Rect::default(),
            graphic_cache: HashMap::new(),
        }
    }

    /// Populates the overlay after an async HTML attachment fetch completes.
    pub fn set_loaded(&mut self, result: Result<String, String>) {
        self.set_loaded_with_kind(ContentKind::Html, result);
    }

    /// Populates the overlay after an async HTML or Markdown attachment fetch completes.
    pub fn set_loaded_with_kind(&mut self, kind: ContentKind, result: Result<String, String>) {
        self.loading = false;
        self.graphic_cache.clear();
        match result {
            Ok(body) => {
                self.error = None;
                let title_opt = if self.title.trim().is_empty() {
                    None
                } else {
                    Some(self.title.clone())
                };
                let mut session = HtmlViewSession::from_content(title_opt, kind, body);
                let init_effects = session.take_pending_effects();
                if self.title.trim().is_empty() {
                    if let Some(t) = session.title() {
                        self.title = t.to_string();
                    }
                }
                self.session = Some(session);
                self.apply_effects(init_effects);
            }
            Err(err) => {
                self.session = None;
                self.error = Some(err);
            }
        }
    }

    pub fn is_markdown(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.source_kind() == SourceKind::Markdown)
    }

    pub fn is_editing_input(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.editing_input.is_some())
    }

    /// Exits active `<input>`/`<textarea>` editing so clicking `[✕ Esc]` closes the modal immediately.
    pub fn force_close_if_editing(&mut self) {
        if self.is_editing_input() {
            let effects = self
                .session
                .as_mut()
                .map(HtmlViewSession::finish_editing)
                .unwrap_or_default();
            self.apply_effects(effects);
        }
    }

    pub fn is_raw_mode(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.raw_mode)
    }

    pub fn is_js_active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.js_active)
    }

    pub fn session(&self) -> Option<&HtmlViewSession> {
        self.session.as_ref()
    }

    pub fn session_mut(&mut self) -> Option<&mut HtmlViewSession> {
        self.session.as_mut()
    }

    pub fn take_pending_commands(&mut self) -> Vec<HtmlPreviewCommand> {
        std::mem::take(&mut self.pending_commands)
    }

    /// Pastes clipboard text into the currently focused `<input>` / `<textarea>` if editing.
    pub fn paste_from_clipboard(&mut self) -> bool {
        if !self.is_editing_input() {
            return false;
        }
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        if let Some(sess) = self.session.as_mut() {
            for ch in cleaned.chars() {
                sess.input_char(ch);
            }
            return true;
        }
        false
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        // Text input editing mode: forward characters directly into the focused HTML input.
        if self.is_editing_input() {
            match action {
                Action::Input(ch) => {
                    if let Some(sess) = self.session.as_mut() {
                        sess.input_char(*ch);
                    }
                    return OverlayOutcome::Handled;
                }
                Action::Backspace => {
                    if let Some(sess) = self.session.as_mut() {
                        sess.backspace();
                    }
                    return OverlayOutcome::Handled;
                }
                Action::Paste(s) => {
                    let cleaned = sanitize_single_line(s);
                    if let Some(sess) = self.session.as_mut() {
                        for ch in cleaned.chars() {
                            sess.input_char(ch);
                        }
                    }
                    return OverlayOutcome::Handled;
                }
                Action::Submit | Action::Activate | Action::CloseOverlay => {
                    let effects = self
                        .session
                        .as_mut()
                        .map(HtmlViewSession::finish_editing)
                        .unwrap_or_default();
                    self.apply_effects(effects);
                    return OverlayOutcome::Handled;
                }
                Action::NextField => {
                    let effects = self
                        .session
                        .as_mut()
                        .map(HtmlViewSession::finish_editing)
                        .unwrap_or_default();
                    self.apply_effects(effects);
                    if let Some(sess) = self.session.as_mut() {
                        sess.focus_next_control();
                    }
                    self.scroll_focused_control_into_view();
                    return OverlayOutcome::Handled;
                }
                Action::PrevField => {
                    let effects = self
                        .session
                        .as_mut()
                        .map(HtmlViewSession::finish_editing)
                        .unwrap_or_default();
                    self.apply_effects(effects);
                    if let Some(sess) = self.session.as_mut() {
                        sess.focus_prev_control();
                    }
                    self.scroll_focused_control_into_view();
                    return OverlayOutcome::Handled;
                }
                _ => return OverlayOutcome::Handled,
            }
        }

        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Quit => OverlayOutcome::Close,
            Action::NextField | Action::FocusNextPane => {
                if let Some(sess) = self.session.as_mut() {
                    sess.focus_next_control();
                }
                self.scroll_focused_control_into_view();
                OverlayOutcome::Handled
            }
            Action::PrevField | Action::FocusPrevPane => {
                if let Some(sess) = self.session.as_mut() {
                    sess.focus_prev_control();
                }
                self.scroll_focused_control_into_view();
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Submit | Action::Input(' ') => {
                let effects = self
                    .session
                    .as_mut()
                    .map(HtmlViewSession::activate_focused)
                    .unwrap_or_default();
                self.apply_effects(effects);
                OverlayOutcome::Handled
            }
            Action::Input('m') | Action::Input('M') | Action::Input('r') | Action::Input('R') => {
                self.toggle_raw_mode();
                OverlayOutcome::Handled
            }
            Action::Input('o') | Action::Input('O') | Action::OpenInBrowser => {
                self.queue_serve_in_browser();
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::Input('k') => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_sub(2);
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_add(2);
                }
                OverlayOutcome::Handled
            }
            Action::ScrollUp => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_sub(3);
                }
                OverlayOutcome::Handled
            }
            Action::ScrollDown => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_add(3);
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                let step = self.last_content_rect.height.saturating_sub(2).max(10);
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_sub(step);
                }
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                let step = self.last_content_rect.height.saturating_sub(2).max(10);
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = sess.scroll_y.saturating_add(step);
                }
                OverlayOutcome::Handled
            }
            Action::GoTop | Action::CursorHome | Action::Input('g') => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = 0;
                }
                OverlayOutcome::Handled
            }
            Action::GoBottom | Action::CursorEnd | Action::Input('G') => {
                if let Some(sess) = self.session.as_mut() {
                    sess.scroll_y = u16::MAX;
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    /// Maps a screen-space mouse click `(row, col)` inside the overlay to header pills or
    /// `HtmlViewSession::click_at(line_index, col)`.
    pub fn handle_mouse_click(&mut self, row: u16, col: u16) -> bool {
        let pos = ratatui::layout::Position::new(col, row);
        if self.last_mode_pill_rect.width > 0 && self.last_mode_pill_rect.contains(pos) {
            self.toggle_raw_mode();
            return true;
        }
        if self.last_browser_pill_rect.width > 0 && self.last_browser_pill_rect.contains(pos) {
            self.queue_serve_in_browser();
            return true;
        }
        if self.last_content_rect.width == 0 || !self.last_content_rect.contains(pos) {
            return false;
        }
        let rel_row = row.saturating_sub(self.last_content_rect.y) as usize;
        let line_index = rel_row.saturating_add(self.last_scroll_y as usize);
        let rel_col = col.saturating_sub(self.last_content_rect.x);
        let effects = self
            .session
            .as_mut()
            .map(|sess| sess.click_at(line_index, rel_col))
            .unwrap_or_default();
        let interacted = !effects.is_empty();
        self.apply_effects(effects);
        interacted
    }

    fn toggle_raw_mode(&mut self) {
        if let Some(sess) = self.session.as_mut() {
            sess.toggle_raw_mode();
            sess.scroll_y = 0;
            self.last_scroll_y = 0;
        }
    }

    fn queue_serve_in_browser(&mut self) {
        if let Some(sess) = self.session.as_ref() {
            self.pending_commands.push(HtmlPreviewCommand::ServeInBrowser {
                title: self.title.clone(),
                html: sess.serve_html(),
            });
        } else if !self.source_url.trim().is_empty() {
            self.pending_commands
                .push(HtmlPreviewCommand::OpenUrl(self.source_url.clone()));
        }
    }

    fn apply_effects(&mut self, effects: Vec<HtmlEffect>) {
        for eff in effects {
            match eff {
                HtmlEffect::None | HtmlEffect::DomChanged => {}
                HtmlEffect::OpenUrl(url) => {
                    self.pending_commands.push(HtmlPreviewCommand::OpenUrl(url));
                }
                HtmlEffect::Toast(msg) => {
                    if let Some(sess) = self.session.as_mut() {
                        sess.last_status = Some(msg.clone());
                    }
                    self.pending_commands.push(HtmlPreviewCommand::Toast(msg));
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let target = if action.trim().is_empty() {
                        "form".to_string()
                    } else {
                        action
                    };
                    let msg = format!("Submitted to {target} ({} field(s))", fields.len());
                    if let Some(sess) = self.session.as_mut() {
                        sess.last_status = Some(msg.clone());
                    }
                    self.pending_commands.push(HtmlPreviewCommand::Toast(msg));
                }
            }
        }
    }

    fn scroll_focused_control_into_view(&mut self) {
        let view_h = self.last_content_rect.height;
        if view_h == 0 {
            return;
        }
        let Some(sess) = self.session.as_mut() else {
            return;
        };
        let Some(focus_idx) = sess.focused_control else {
            return;
        };
        let width = self.last_content_rect.width.max(12);
        let theme = HtmlTheme::dark();
        let line_opt = {
            let rendered = sess.render(width, &theme, false);
            rendered.controls.get(focus_idx).map(|c| c.line as u16)
        };
        if let Some(line) = line_opt {
            if line < sess.scroll_y {
                sess.scroll_y = line;
            } else if line >= sess.scroll_y.saturating_add(view_h) {
                sess.scroll_y = line.saturating_sub(view_h.saturating_sub(1));
            }
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);

        self.last_content_rect = Rect::default();
        self.last_mode_pill_rect = Rect::default();
        self.last_browser_pill_rect = Rect::default();

        let modal_style = if theme.no_color {
            Style::default()
        } else {
            Style::default().bg(theme.card_bg).fg(theme.fg)
        };
        let close_w = "[✕ Esc]".width() as u16;
        let show_close_pill = rect.width >= 16;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let is_md = self.is_markdown();
        let mode_label = if self.is_raw_mode() {
            if is_md { "Raw MD" } else { "Raw HTML" }
        } else {
            "Rendered"
        };
        let js_badge = if self.is_js_active() { " · JS" } else { "" };
        let kind_prefix = if is_md {
            "Markdown Preview"
        } else {
            "HTML Preview"
        };
        let title = format!(" {kind_prefix} — {} [{mode_label}{js_badge}] ", self.title);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(modal_style)
            .title(Line::from(Span::styled(
                truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, &theme, hitmap, true);

        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let (header_area, content_area, footer_area) = if inner.height >= 4 {
            (
                Some(Rect::new(inner.x, inner.y, inner.width, 1)),
                Rect::new(
                    inner.x,
                    inner.y.saturating_add(1),
                    inner.width,
                    inner.height.saturating_sub(2),
                ),
                Rect::new(inner.x, inner.y.saturating_add(inner.height - 1), inner.width, 1),
            )
        } else {
            (
                None,
                Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1)),
                Rect::new(inner.x, inner.y.saturating_add(inner.height - 1), inner.width, 1),
            )
        };

        // Pre-render session at available width so we know control count, scrollbar, and status.
        let html_theme = theme_to_html_theme(&theme);
        let mut control_count = 0usize;
        let mut status_text: Option<String> = None;

        if let Some(sess) = self.session.as_mut() {
            // Reserve scrollbar column upfront so `HtmlViewSession`'s 1-slot render cache hits
            // every frame instead of thrashing between `width` and `width - 1`.
            let avail_w = if content_area.width > 2 {
                content_area.width.saturating_sub(1)
            } else {
                content_area.width
            };

            let prev_scroll_y = sess.scroll_y;
            let (total_lines, ctrl_len, graphics, visible_lines, sy) = {
                let rendered = sess.render(avail_w.max(12), &html_theme, ui.show_hrefs);
                let total_lines = rendered.lines.len();
                let max_scroll_y = total_lines.saturating_sub(content_area.height as usize) as u16;
                let sy = prev_scroll_y.min(max_scroll_y);
                let view_start = sy as usize;
                let view_end = view_start
                    .saturating_add(content_area.height as usize)
                    .min(total_lines);
                let visible_lines = rendered.lines[view_start..view_end].to_vec();
                let graphics = if rendered.graphics.is_empty() {
                    Vec::new()
                } else {
                    rendered.graphics.clone()
                };
                (total_lines, rendered.controls.len(), graphics, visible_lines, sy)
            };
            sess.scroll_y = sy;
            let show_scrollbar =
                total_lines > content_area.height as usize && content_area.width > 2;
            let max_scroll_y = total_lines.saturating_sub(content_area.height as usize) as u16;
            control_count = ctrl_len;
            status_text = sess.last_status.clone();

            self.last_scroll_y = sy;
            self.last_content_rect =
                Rect::new(content_area.x, content_area.y, avail_w, content_area.height);

            // Render intersecting inline graphics (SVG / Canvas 2D / data:image/*) into their
            // reserved vertical slots so scroll offsets and click_at(line, col) stay exact.
            let view_start = sy as usize;
            let view_end = view_start.saturating_add(content_area.height as usize);
            let mut visible_lines = visible_lines;
            let caps = ui.graphics_caps;
            let proto = caps.protocol;
            let bg_rgb = img_preview::color_to_rgb(
                theme.card_bg,
                img_preview::DEFAULT_PREVIEW_BG_RGB,
            );
            let mut pixel_skip_rects: Vec<Rect> = Vec::new();

            if avail_w > 0 && content_area.height > 0 && !graphics.is_empty() {
                for g in &graphics {
                    let g_start = g.line_start;
                    let g_end = g.line_start.saturating_add(g.line_count);
                    if g.line_count == 0 || g_end <= view_start || g_start >= view_end {
                        continue;
                    }

                    let cache_key = (g.node_id.0, avail_w, proto);
                    let cache_hit = self
                        .graphic_cache
                        .get(&cache_key)
                        .is_some_and(|(src, _)| Arc::ptr_eq(src, &g.image_bytes));
                    if !cache_hit {
                        let slot_rows = g.line_count.min(u16::MAX as usize) as u16;
                        let fit = FitBox::fit(avail_w, slot_rows);
                        let render_caps = TerminalGraphicsCaps {
                            protocol: proto,
                            cell_px: caps.cell_px,
                            cell_px_exact: caps.cell_px_exact,
                        };
                        if let Ok(rendered_img) = img_preview::decode_and_render_for_protocol(
                            &g.image_bytes,
                            fit,
                            bg_rgb,
                            render_caps,
                        ) {
                            self.graphic_cache.insert(
                                cache_key,
                                (Arc::clone(&g.image_bytes), rendered_img),
                            );
                        }
                    }

                    let Some((_, rendered_img)) = self.graphic_cache.get(&cache_key) else {
                        continue;
                    };

                    let vis_start = g_start.max(view_start);
                    let vis_end = g_end.min(view_end);
                    if rendered_img.protocol.is_pixel() && rendered_img.pixel_payload.is_some() {
                        for line_idx in vis_start..vis_end {
                            if let Some(slot) = visible_lines.get_mut(line_idx - view_start) {
                                *slot = Line::default();
                            }
                        }
                        let crop_scroll_y = vis_start.saturating_sub(g_start) as u16;
                        let vis_rows = vis_end.saturating_sub(vis_start) as u16;
                        if vis_rows > 0 && crop_scroll_y < rendered_img.rows {
                            if let Some((payload, draw_cols, draw_rows)) = rendered_img
                                .viewport_pixel_payload(0, crop_scroll_y, avail_w, vis_rows)
                            {
                                let screen_y = self
                                    .last_content_rect
                                    .y
                                    .saturating_add(vis_start.saturating_sub(view_start) as u16);
                                let draw_w = draw_cols.min(avail_w);
                                let draw_h = draw_rows.min(vis_rows);
                                if draw_w > 0 && draw_h > 0 {
                                    let target_rect = Rect::new(
                                        self.last_content_rect.x,
                                        screen_y,
                                        draw_w,
                                        draw_h,
                                    );
                                    let pending_key = format!(
                                        "html:{}:{}x{}:{}#crop:0,{},{},{}",
                                        g.node_id.0,
                                        avail_w,
                                        g.line_count,
                                        proto.as_str(),
                                        crop_scroll_y,
                                        draw_w,
                                        draw_h
                                    );
                                    ui.pending_pixel_graphics.push(PendingPixelGraphic {
                                        cache_key: pending_key,
                                        rect: target_rect,
                                        protocol: rendered_img.protocol,
                                        payload: Some(payload),
                                    });
                                    pixel_skip_rects.push(target_rect);
                                }
                            }
                        }
                    } else {
                        for line_idx in vis_start..vis_end {
                            let row_offset = line_idx - g_start;
                            if let Some(slot) = visible_lines.get_mut(line_idx - view_start) {
                                *slot = rendered_img
                                    .lines
                                    .get(row_offset)
                                    .cloned()
                                    .unwrap_or_default();
                            }
                        }
                    }
                }
            }

            if avail_w > 0 && content_area.height > 0 {
                frame.render_widget(
                    Paragraph::new(visible_lines).style(modal_style),
                    self.last_content_rect,
                );
                if !pixel_skip_rects.is_empty() {
                    let buf = frame.buffer_mut();
                    for target_rect in pixel_skip_rects {
                        for y in target_rect.top()..target_rect.bottom() {
                            for x in target_rect.left()..target_rect.right() {
                                if let Some(cell) = buf.cell_mut((x, y)) {
                                    cell.set_skip(true);
                                }
                            }
                        }
                    }
                }
            }

            if show_scrollbar {
                let sb_area = Rect::new(
                    content_area.right().saturating_sub(1),
                    content_area.y,
                    1,
                    content_area.height,
                );
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        } else if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" Failed to load HTML: {err}"),
                    Style::default().fg(theme.error),
                ))
                .style(modal_style),
                content_area,
            );
        } else if self.loading {
            frame.render_widget(
                Paragraph::new(Span::styled(" Loading HTML document…", theme.muted_style()))
                    .style(modal_style),
                content_area,
            );
        }

        if let Some(hdr) = header_area {
            let pill_style = if theme.no_color {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
                    .bg(theme.chip_bg)
                    .fg(theme.chip_fg)
                    .add_modifier(Modifier::BOLD)
            };
            let mode_pill = if self.is_raw_mode() {
                "[m: Raw]"
            } else {
                "[m: Rendered]"
            };
            let browser_pill = "[o: Browser]";
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut cx = hdr.x;

            spans.push(Span::styled(" ", modal_style));
            cx = cx.saturating_add(1);

            let mode_w = mode_pill.width() as u16;
            if cx.saturating_add(mode_w) <= hdr.right() {
                self.last_mode_pill_rect = Rect::new(cx, hdr.y, mode_w, 1);
            }
            spans.push(Span::styled(mode_pill, pill_style));
            cx = cx.saturating_add(mode_w);

            spans.push(Span::styled(" ", modal_style));
            cx = cx.saturating_add(1);

            let browser_w = browser_pill.width() as u16;
            if cx.saturating_add(browser_w) <= hdr.right() {
                self.last_browser_pill_rect = Rect::new(cx, hdr.y, browser_w, 1);
            }
            spans.push(Span::styled(browser_pill, pill_style));
            cx = cx.saturating_add(browser_w);

            let meta = if self.is_js_active() {
                format!("  JS active · {control_count} control(s)")
            } else if !self.is_raw_mode() {
                format!("  Static (0 MB JS) · {control_count} control(s)")
            } else {
                "  Source view".to_string()
            };
            let rem = hdr.right().saturating_sub(cx) as usize;
            if rem > 0 {
                let mut tail = meta;
                if let Some(st) = status_text.as_deref().filter(|s| !s.trim().is_empty()) {
                    tail.push_str("  ·  ");
                    tail.push_str(st);
                }
                spans.push(Span::styled(
                    truncate_width(&tail, rem),
                    theme.muted_style(),
                ));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)).style(modal_style), hdr);
        }

        let footer = if self.is_editing_input() {
            " Typing input · Enter/Esc: Finish editing · Backspace: Delete · Tab: Next control "
        } else {
            " Tab/S-Tab: Focus · Enter/Space/Click: Activate · m: Raw/Rendered · o: Browser · j/k/Wheel: Scroll · Esc/q: Close "
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(footer, footer_area.width as usize),
                theme.muted_style(),
            ))
            .style(modal_style),
            footer_area,
        );
    }
}

/// Converts Atlassian's active [`Theme`] into a matching `bitefoundry_html::HtmlTheme`.
pub fn theme_to_html_theme(theme: &Theme) -> HtmlTheme {
    if theme.no_color {
        return HtmlTheme::no_color();
    }
    HtmlTheme {
        text: theme.fg_primary,
        muted: theme.fg_muted,
        heading: theme.confluence_teal,
        accent: theme.accent,
        link: theme.accent_soft,
        code_fg: theme.code_fg,
        code_bg: theme.code_bg,
        quote_border: theme.border,
        border: theme.border,
        control_fg: theme.chip_fg,
        control_bg: theme.chip_bg,
        focus_fg: if theme.ansi16_only {
            Color::Reset
        } else {
            theme.jira_pill_fg
        },
        focus_bg: if theme.ansi16_only {
            Color::Reset
        } else {
            theme.jira_cobalt
        },
        raw_tag: theme.accent,
        raw_attr: theme.warning,
    }
}

/// Strictly checks whether an attachment's MIME type or filename identifies it as HTML.
pub fn is_html_attachment(mime_type: &str, filename: &str) -> bool {
    let mime = (!mime_type.trim().is_empty()).then_some(mime_type);
    let name = (!filename.trim().is_empty()).then_some(filename);
    bitefoundry_html::sniff_content_kind(mime, name, None) == ContentKind::Html
}

/// Strictly checks whether an attachment's MIME type or filename identifies it as Markdown.
pub fn is_markdown_attachment(mime_type: &str, filename: &str) -> bool {
    let mime = (!mime_type.trim().is_empty()).then_some(mime_type);
    let name = (!filename.trim().is_empty()).then_some(filename);
    bitefoundry_html::sniff_content_kind(mime, name, None) == ContentKind::Markdown
}

/// Returns `true` when an attachment is HTML or Markdown and previewable via [`HtmlPreviewOverlay`].
pub fn is_previewable_markup_attachment(mime_type: &str, filename: &str) -> bool {
    is_html_attachment(mime_type, filename) || is_markdown_attachment(mime_type, filename)
}

/// Pre-processes Confluence Storage XHTML (`ac:structured-macro`, `<ac:parameter>`, CDATA,
/// `<ac:image>`, `<ac:task-list>`, `<ac:link>`) into clean standard HTML so `bitefoundry-html`
/// renders macros without leaking `<ac:parameter>` or raw diagram CDATA source.
pub fn normalize_confluence_storage_xhtml(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if !lower.contains("<ac:") && !lower.contains("<ri:") && !lower.contains("<![cdata[") {
        return raw.to_string();
    }

    let normalized = crate::adf::preprocess_storage_xhtml(raw);

    let fragment = Html::parse_fragment(&normalized);
    let mut out = String::with_capacity(raw.len());
    serialize_normalized_children(fragment.root_element(), &mut out);
    out
}

fn is_void_html_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn collect_storage_raw_text(el: ElementRef<'_>) -> String {
    let mut s = String::new();
    for node in el.descendants() {
        match node.value() {
            Node::Text(t) => s.push_str(t),
            Node::Comment(c) => {
                let trimmed = c.trim();
                if let Some(inner) = trimmed
                    .strip_prefix("[CDATA[")
                    .and_then(|rest| rest.strip_suffix("]]"))
                {
                    s.push_str(inner);
                }
            }
            Node::Element(e) if e.name().eq_ignore_ascii_case("br") => s.push('\n'),
            _ => {}
        }
    }
    s
}

fn serialize_normalized_children(parent: ElementRef<'_>, out: &mut String) {
    for child in parent.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&escape_html(t)),
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(child) {
                    serialize_normalized_element(el, out);
                }
            }
            _ => {}
        }
    }
}

fn serialize_normalized_element(el: ElementRef<'_>, out: &mut String) {
    let tag = el.value().name().to_ascii_lowercase();
    match tag.as_str() {
        "ac:parameter" | "ac:plain-text-body" => {}
        "ac:rich-text-body" | "ac:layout" | "ac:layout-section" | "ac:layout-cell" => {
            serialize_normalized_children(el, out);
        }
        "ac:structured-macro" => {
            let macro_name = el
                .value()
                .attr("ac:name")
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut params = HashMap::<String, String>::new();
            let mut plain_body: Option<String> = None;
            let mut rich_body_el: Option<ElementRef<'_>> = None;

            for child in el.children().filter_map(ElementRef::wrap) {
                let cname = child.value().name().to_ascii_lowercase();
                if cname == "ac:parameter" {
                    if let Some(pname) = child.value().attr("ac:name") {
                        params.insert(
                            pname.to_ascii_lowercase(),
                            collect_storage_raw_text(child).trim().to_string(),
                        );
                    }
                } else if cname == "ac:plain-text-body" {
                    plain_body = Some(collect_storage_raw_text(child));
                } else if cname == "ac:rich-text-body" {
                    rich_body_el = Some(child);
                }
            }

            match macro_name.as_str() {
                "code" | "noformat" => {
                    let lang = params
                        .get("language")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty());
                    let code = plain_body
                        .or_else(|| rich_body_el.map(collect_storage_raw_text))
                        .unwrap_or_default();
                    if let Some(l) = lang {
                        out.push_str(&format!(
                            "<pre><code class=\"language-{}\">{}</code></pre>",
                            escape_html_attr(l),
                            escape_html(&code)
                        ));
                    } else {
                        out.push_str(&format!("<pre><code>{}</code></pre>", escape_html(&code)));
                    }
                }
                "info" | "note" | "warning" | "tip" | "panel" => {
                    out.push_str("<blockquote>");
                    if let Some(title) = params
                        .get("title")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        out.push_str(&format!("<p><strong>{}</strong></p>", escape_html(title)));
                    }
                    if let Some(rich) = rich_body_el {
                        serialize_normalized_children(rich, out);
                    } else if let Some(plain) = plain_body.filter(|s| !s.trim().is_empty()) {
                        out.push_str(&format!("<p>{}</p>", escape_html(plain.trim())));
                    }
                    out.push_str("</blockquote>");
                }
                "expand" => {
                    let title = params
                        .get("title")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Details");
                    out.push_str(&format!(
                        "<details><summary>{}</summary>",
                        escape_html(title)
                    ));
                    if let Some(rich) = rich_body_el {
                        serialize_normalized_children(rich, out);
                    }
                    out.push_str("</details>");
                }
                "html" => {
                    if let Some(html_body) = plain_body {
                        out.push_str(&html_body);
                    } else if let Some(rich) = rich_body_el {
                        serialize_normalized_children(rich, out);
                    }
                }
                "status" => {
                    if let Some(title) = params
                        .get("title")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        out.push_str(&format!("<span>[{}]</span>", escape_html(title)));
                    }
                }
                "jira" => {
                    if let Some(key) = params
                        .get("key")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        out.push_str(&format!("<span>{}</span>", escape_html(key)));
                    }
                }
                "mermaid" | "drawio" | "gliffy" | "plantuml" | "chart" => {
                    out.push_str(&format!(
                        "<blockquote><p>[◈ Diagram ({}) · Press o for Browser]</p></blockquote>",
                        escape_html(&macro_name)
                    ));
                }
                _ => {
                    if let Some(rich) = rich_body_el {
                        serialize_normalized_children(rich, out);
                    }
                }
            }
        }
        "ac:image" => {
            let alt = el
                .value()
                .attr("ac:alt")
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let mut src: Option<&str> = None;
            for child in el.descendants().filter_map(ElementRef::wrap) {
                let cname = child.value().name().to_ascii_lowercase();
                if cname == "ri:url" {
                    src = child.value().attr("ri:value");
                } else if cname == "ri:attachment" && src.is_none() {
                    src = child.value().attr("ri:filename");
                }
            }
            let src_val = src.unwrap_or("");
            let alt_val = alt.or(src).unwrap_or("image");
            out.push_str(&format!(
                "<img src=\"{}\" alt=\"{}\">",
                escape_html_attr(src_val),
                escape_html_attr(alt_val)
            ));
        }
        "ac:link" => {
            let mut label: Option<String> = None;
            let mut fallback: Option<String> = None;
            for child in el.descendants().filter_map(ElementRef::wrap) {
                let cname = child.value().name().to_ascii_lowercase();
                if cname == "ac:plain-text-link-body" || cname == "ac:link-body" {
                    let t = collect_storage_raw_text(child).trim().to_string();
                    if !t.is_empty() {
                        label = Some(t);
                    }
                } else if cname == "ri:page" {
                    if let Some(t) = child.value().attr("ri:content-title") {
                        fallback = Some(t.to_string());
                    }
                } else if cname == "ri:user" {
                    fallback = child
                        .value()
                        .attr("ri:username")
                        .or_else(|| child.value().attr("ri:account-id"))
                        .map(|u| format!("@{u}"));
                }
            }
            if let Some(text) = label.or(fallback) {
                out.push_str(&escape_html(&text));
            }
        }
        "ac:task-list" => {
            out.push_str("<ul>");
            for child in el.children().filter_map(ElementRef::wrap) {
                if child.value().name().eq_ignore_ascii_case("ac:task") {
                    let mut checked = false;
                    let mut body_el: Option<ElementRef<'_>> = None;
                    for sub in child.children().filter_map(ElementRef::wrap) {
                        let sname = sub.value().name().to_ascii_lowercase();
                        if sname == "ac:task-status" {
                            let status = collect_storage_raw_text(sub).trim().to_ascii_lowercase();
                            checked = matches!(status.as_str(), "complete" | "completed" | "done");
                        } else if sname == "ac:task-body" {
                            body_el = Some(sub);
                        }
                    }
                    out.push_str("<li><input type=\"checkbox\"");
                    if checked {
                        out.push_str(" checked");
                    }
                    out.push_str("> ");
                    if let Some(b) = body_el {
                        serialize_normalized_children(b, out);
                    }
                    out.push_str("</li>");
                }
            }
            out.push_str("</ul>");
        }
        _ if tag.starts_with("ac:") || tag.starts_with("ri:") => {
            serialize_normalized_children(el, out);
        }
        _ => {
            let orig_name = el.value().name();
            out.push('<');
            out.push_str(orig_name);
            for (k, v) in el.value().attrs() {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&escape_html_attr(v));
                out.push('"');
            }
            out.push('>');
            if !is_void_html_tag(&tag) {
                if tag == "script" || tag == "style" {
                    for child in el.children() {
                        if let Node::Text(t) = child.value() {
                            out.push_str(t);
                        }
                    }
                } else {
                    serialize_normalized_children(el, out);
                }
                out.push_str("</");
                out.push_str(orig_name);
                out.push('>');
            }
        }
    }
}

/// Builds interactive HTML markup for a Confluence [`PageDetail`] (preserving Confluence storage/rendered
/// HTML after normalizing Confluence storage macros, otherwise converting Markdown / `DocBlock` AST to HTML).
pub fn page_detail_to_html(page: &PageDetail) -> String {
    let raw = page.body_raw.trim();
    let rep = page.body_representation.trim().to_ascii_lowercase();
    let raw_is_html = !raw.is_empty()
        && (rep == "storage"
            || rep == "html"
            || rep.contains("view")
            || bitefoundry_html::looks_like_html_bytes(raw.as_bytes()));

    if raw_is_html {
        let normalized = normalize_confluence_storage_xhtml(raw);
        let head_lower: String = normalized
            .chars()
            .take(256)
            .collect::<String>()
            .to_ascii_lowercase();
        if head_lower.contains("<!doctype") || head_lower.contains("<html") {
            return normalized;
        }
        return format!(
            "<article><h1>{}</h1>\n{normalized}\n</article>",
            escape_html(&page.summary.title)
        );
    }

    let md = if !page.body_markdown.trim().is_empty() {
        page.body_markdown.clone()
    } else if !page.blocks.is_empty() {
        adf::blocks_to_markdown(&page.blocks)
    } else {
        String::new()
    };

    let body_html = if md.trim().is_empty() {
        "<p>(empty page body)</p>".to_string()
    } else {
        adf::markdown_to_storage_xhtml(&md)
    };

    let mut out = format!(
        "<article><h1>{}</h1>\n<p><strong>Space:</strong> {} · <strong>Version:</strong> v{}</p>\n{body_html}",
        escape_html(&page.summary.title),
        escape_html(&page.summary.space_key),
        page.summary.version_number,
    );
    if !page.summary.web_url.trim().is_empty() {
        out.push_str(&format!(
            "\n<p><a href=\"{}\">Open in Confluence</a></p>",
            escape_html_attr(&page.summary.web_url)
        ));
    }
    out.push_str("\n</article>");
    out
}

/// Builds interactive HTML markup for a Jira [`IssueDetail`] (preserving raw HTML description if present,
/// otherwise converting Markdown / `DocBlock` AST to HTML with collapsible comments).
pub fn issue_detail_to_html(issue: &IssueDetail) -> String {
    let s = &issue.summary;
    let raw_desc = issue.description_raw.trim();
    let desc_html = if !raw_desc.is_empty() && bitefoundry_html::looks_like_html_bytes(raw_desc.as_bytes())
    {
        raw_desc.to_string()
    } else {
        let md = if !issue.description_markdown.trim().is_empty() {
            issue.description_markdown.clone()
        } else if !issue.description_blocks.is_empty() {
            adf::blocks_to_markdown(&issue.description_blocks)
        } else {
            String::new()
        };
        if md.trim().is_empty() {
            "<p>(no description)</p>".to_string()
        } else {
            adf::markdown_to_storage_xhtml(&md)
        }
    };

    let assignee = s
        .assignee
        .as_ref()
        .map(|u| u.display_name.as_str())
        .unwrap_or("Unassigned");
    let priority = s
        .priority
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or("Medium");

    let mut out = format!(
        "<article><h1>{}: {}</h1>\n<table><tr><th>Status</th><td>{}</td><th>Priority</th><td>{}</td></tr><tr><th>Assignee</th><td>{}</td><th>Project</th><td>{}</td></tr></table>\n<h2>Description</h2>\n{desc_html}",
        escape_html(&s.key),
        escape_html(&s.summary),
        escape_html(&s.status.name),
        escape_html(priority),
        escape_html(assignee),
        escape_html(&s.project_key),
    );

    if !issue.comments.is_empty() {
        out.push_str(&format!(
            "\n<details open><summary>Comments ({})</summary>",
            issue.comments.len()
        ));
        for c in &issue.comments {
            let c_html = if !c.body_markdown.trim().is_empty() {
                adf::markdown_to_storage_xhtml(&c.body_markdown)
            } else {
                format!("<p>{}</p>", escape_html(&c.body_raw))
            };
            out.push_str(&format!(
                "\n<section><p><strong>{}</strong> ({})</p>{c_html}</section>",
                escape_html(&c.author.display_name),
                escape_html(&c.created_at),
            ));
        }
        out.push_str("\n</details>");
    }

    if !s.web_url.trim().is_empty() {
        out.push_str(&format!(
            "\n<p><a href=\"{}\">Open {} in Jira</a></p>",
            escape_html_attr(&s.web_url),
            escape_html(&s.key)
        ));
    }
    out.push_str("\n</article>");
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona},
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

    fn make_test_app_and_ui() -> (AppState, UiState, mpsc::UnboundedReceiver<crate::ui::app::UiMsg>) {
        let cfg = AppConfig {
            site_url: "https://acme.atlassian.net".into(),
            ..Default::default()
        };
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Confluence,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);
        (app, ui, ui_rx)
    }

    fn render_overlay_text(
        ov: &mut HtmlPreviewOverlay,
        app: &AppState,
        ui: &mut UiState,
        width: u16,
        height: u16,
    ) -> (String, HitMap) {
        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        ui.pending_pixel_graphics.clear();
        terminal
            .draw(|f| ov.draw(f, f.area(), app, ui, &mut hitmap))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        (text, hitmap)
    }

    #[tokio::test]
    async fn button_onclick_tab_activate_and_click_at_mutate_dom() {
        let (app, mut ui, _rx) = make_test_app_and_ui();
        let html = r#"
            <h1>Interactive Counter</h1>
            <p id="status">Count: 0</p>
            <button id="inc" onclick="window.__c = (window.__c || 0) + 1; document.getElementById('status').textContent = 'Count: ' + window.__c;">Increment</button>
        "#;
        let mut ov = HtmlPreviewOverlay::new("Counter Demo", html);
        assert!(ov.is_js_active(), "inline onclick must spin up QuickJS on demand");

        // Initial render populates last_content_rect and control hit coordinates
        let (screen0, hitmap) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(screen0.contains("Count: 0"), "expected initial DOM:\n{screen0}");
        assert!(screen0.contains("[ Increment ]"), "expected button:\n{screen0}");
        assert!(!hitmap.is_empty());

        // Tab to focus the button and press Space to activate -> Count: 1
        assert_eq!(ov.handle(&Action::NextField), OverlayOutcome::Handled);
        assert_eq!(ov.handle(&Action::Input(' ')), OverlayOutcome::Handled);
        let (screen1, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(screen1.contains("Count: 1"), "expected Count: 1 after Tab+Space:\n{screen1}");

        // Mouse click directly on the button's screen coordinates -> Count: 2
        let (btn_line, btn_col) = {
            let content_w = ov.last_content_rect.width;
            let sess = ov.session_mut().unwrap();
            let theme = theme_to_html_theme(&ui.theme);
            let r = sess.render(content_w, &theme, false);
            let ctrl = &r.controls[0];
            (ctrl.line as u16, ctrl.col_start + 1)
        };
        let screen_row = ov.last_content_rect.y + btn_line.saturating_sub(ov.last_scroll_y);
        let screen_col = ov.last_content_rect.x + btn_col;
        assert!(ov.handle_mouse_click(screen_row, screen_col));

        let (screen2, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(screen2.contains("Count: 2"), "expected Count: 2 after click_at:\n{screen2}");
    }

    #[tokio::test]
    async fn static_html_has_zero_js_and_toggles_raw_mode_and_text_input() {
        let (app, mut ui, _rx) = make_test_app_and_ui();
        let html = r#"
            <form action="/save">
              <input type="text" name="note" value="hi" />
              <details><summary>More Info</summary><p>Hidden Secret</p></details>
            </form>
        "#;
        let mut ov = HtmlPreviewOverlay::new("Static Form", html);
        assert!(!ov.is_js_active(), "static HTML must allocate 0 MB QuickJS");

        let (screen0, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(!screen0.contains("Hidden Secret"));
        assert!(screen0.contains("Static (0 MB JS)"));

        // Focus text input, activate, type, backspace, finish editing
        ov.handle(&Action::NextField);
        ov.handle(&Action::Activate);
        assert!(ov.is_editing_input());
        ov.handle(&Action::Input('!'));
        ov.handle(&Action::Backspace);
        ov.handle(&Action::Input('2'));
        ov.handle(&Action::Submit);
        assert!(!ov.is_editing_input());

        // Focus details summary and press Enter to expand
        ov.handle(&Action::NextField);
        ov.handle(&Action::Activate);
        let (screen1, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(screen1.contains("Hidden Secret"));

        // Press `m` to toggle Raw HTML mode
        ov.handle(&Action::Input('m'));
        assert!(ov.is_raw_mode());
        let (raw_screen, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 24);
        assert!(raw_screen.contains("<form action=\"/save\">"), "expected raw tags:\n{raw_screen}");
        assert!(raw_screen.contains("[m: Raw]"));

        // Press `m` again to return to Rendered mode
        ov.handle(&Action::Input('m'));
        assert!(!ov.is_raw_mode());

        // Press `q` to close
        assert_eq!(ov.handle(&Action::Input('q')), OverlayOutcome::Close);
    }

    #[tokio::test]
    async fn press_o_serves_html_natively_over_localhost_axum() {
        let (mut app, mut ui, mut ui_rx) = make_test_app_and_ui();
        let html = "<h1>Atlassian Local Serve</h1><p>Interactive body</p>";
        let mut ov = HtmlPreviewOverlay::new("Serve Test", html);

        assert_eq!(ov.handle(&Action::Input('o')), OverlayOutcome::Handled);
        let cmds = ov.take_pending_commands();
        assert_eq!(cmds.len(), 1);
        let (title, raw_html) = match &cmds[0] {
            HtmlPreviewCommand::ServeInBrowser { title, html } => (title.clone(), html.clone()),
            other => panic!("expected ServeInBrowser, got {other:?}"),
        };

        app.request_serve_html_in_browser(&mut ui, title, raw_html);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("HtmlServed timed out")
            .expect("ui_rx closed");
        app.handle_ui_msg(&mut ui, msg);

        let url = app
            .last_served_html_url
            .clone()
            .expect("expected served localhost URL");
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.contains("/view/"));

        // Fetch the served document over TCP to verify HTTP 200 + HTML payload
        let without_scheme = url.strip_prefix("http://").unwrap();
        let (host_port, path) = without_scheme.split_once('/').unwrap();
        let mut stream = TcpStream::connect(host_port).await.unwrap();
        let req = format!("GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "unexpected response: {resp}");
        assert!(resp.contains("<h1>Atlassian Local Serve</h1>"));
        app.html_server.shutdown();
    }

    #[test]
    fn page_detail_to_html_normalizes_confluence_storage_xhtml_with_zero_leaks() {
        let mut page = PageDetail::default();
        page.summary.title = "Architecture Spec".into();
        page.body_representation = "storage".into();
        page.body_raw = r#"
            <style>@media (max-width: 600px) { .secret-css { color: red; } }</style>
            <script>const SECRET_JS = 42;</script>
            <ac:parameter ac:name="orphan">SECRET_ORPHAN_PARAM</ac:parameter>
            <p>Main body paragraph.</p>
            <ac:structured-macro ac:name="info">
                <ac:parameter ac:name="title">Heads Up</ac:parameter>
                <ac:rich-text-body><p>Important notice</p></ac:rich-text-body>
            </ac:structured-macro>
            <ac:structured-macro ac:name="code">
                <ac:parameter ac:name="language">rust</ac:parameter>
                <ac:plain-text-body><![CDATA[fn main() { println!("ok"); }]]></ac:plain-text-body>
            </ac:structured-macro>
            <ac:structured-macro ac:name="expand">
                <ac:parameter ac:name="title">More Details</ac:parameter>
                <ac:rich-text-body><p>Expanded copy</p></ac:rich-text-body>
            </ac:structured-macro>
            <ac:structured-macro ac:name="html">
                <ac:parameter ac:name="sanitize">false</ac:parameter>
                <ac:plain-text-body><![CDATA[<div class="embedded"><style>.inner-css-leak{display:none}</style><p>Unwrapped Embedded Content</p></div>]]></ac:plain-text-body>
            </ac:structured-macro>
            <ac:structured-macro ac:name="mermaid">
                <ac:parameter ac:name="theme">dark</ac:parameter>
                <ac:plain-text-body><![CDATA[graph TD; SECRET_MERMAID_CDATA --> END;]]></ac:plain-text-body>
            </ac:structured-macro>
        "#
        .into();

        let html = page_detail_to_html(&page);
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.contains("<details><summary>More Details</summary>"));
        assert!(html.contains("Unwrapped Embedded Content"));
        assert!(!html.contains("<ac:parameter"));
        assert!(!html.contains("SECRET_ORPHAN_PARAM"));
        assert!(!html.contains("SECRET_MERMAID_CDATA"));

        let mut sess = HtmlViewSession::new(Some(page.summary.title.clone()), html.clone());
        let theme = HtmlTheme::dark();
        let rendered = sess.render(90, &theme, false);
        let visible_text: String = rendered
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            visible_text.contains("Main body paragraph."),
            "missing main body in rendered output:\nHTML:\n{html}\nVISIBLE:\n{visible_text}"
        );
        assert!(visible_text.contains("Heads Up"));
        assert!(visible_text.contains("Important notice"));
        assert!(visible_text.contains("Unwrapped Embedded Content"));
        assert!(visible_text.contains("[◈ Diagram (mermaid) · Press o for Browser]"));

        for forbidden in [
            "secret-css",
            "SECRET_JS",
            "SECRET_ORPHAN_PARAM",
            "inner-css-leak",
            "SECRET_MERMAID_CDATA",
            "<ac:parameter",
            "ac:structured-macro",
        ] {
            assert!(
                !visible_text.contains(forbidden),
                "leaked `{forbidden}` into rendered HTML preview:\n{visible_text}"
            );
        }
    }

    #[tokio::test]
    async fn html_preview_inline_svg_renders_halfblocks_and_preserves_button_click_coordinates() {
        let (app, mut ui, _rx) = make_test_app_and_ui();
        ui.graphics_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: true,
        };

        let html = r##"
            <h1>Vector Chart</h1>
            <p id="status">Clicked: 0</p>
            <svg width="160" height="80"><rect width="160" height="80" fill="#e11d48"/><circle cx="80" cy="40" r="24" fill="#38bdf8"/></svg>
            <button id="go" onclick="window.__hits = (window.__hits || 0) + 1; document.getElementById('status').textContent = 'Clicked: ' + window.__hits;">Run Action</button>
        "##;
        let mut ov = HtmlPreviewOverlay::new("SVG + Button", html);

        let (screen0, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 32);
        assert!(screen0.contains("Vector Chart"), "expected heading:\n{screen0}");
        assert!(screen0.contains("Clicked: 0"), "expected initial status:\n{screen0}");
        assert!(
            screen0.contains('▀'),
            "expected Halfblock `▀` glyphs for inline <svg> in reserved slot:\n{screen0}"
        );
        assert!(screen0.contains("[ Run Action ]"), "expected button below SVG:\n{screen0}");

        // Verify the graphic slot exists and the button sits strictly below the SVG slot
        let (graphic_end, btn_line, btn_col) = {
            let content_w = ov.last_content_rect.width;
            let sess = ov.session_mut().unwrap();
            let theme = theme_to_html_theme(&ui.theme);
            let r = sess.render(content_w, &theme, false);
            assert_eq!(r.graphics.len(), 1, "expected 1 inline SVG graphic");
            let g = &r.graphics[0];
            assert!(g.line_count >= 3, "expected reserved slot lines >= 3");
            let ctrl = &r.controls[0];
            assert!(
                ctrl.line >= g.line_start + g.line_count,
                "button line {} must be below graphic slot {}..{}",
                ctrl.line,
                g.line_start,
                g.line_start + g.line_count
            );
            (g.line_start + g.line_count, ctrl.line as u16, ctrl.col_start + 1)
        };
        assert!(graphic_end > 0);
        assert_eq!(ov.graphic_cache.len(), 1, "rendered SVG should be cached");

        // Click the button below the inline SVG via screen coordinates -> click_at hits button
        let screen_row = ov.last_content_rect.y + btn_line.saturating_sub(ov.last_scroll_y);
        let screen_col = ov.last_content_rect.x + btn_col;
        assert!(
            ov.handle_mouse_click(screen_row, screen_col),
            "click_at at ({btn_line}, {btn_col}) must hit <button> below inline SVG"
        );

        let (screen1, _) = render_overlay_text(&mut ov, &app, &mut ui, 90, 32);
        assert!(
            screen1.contains("Clicked: 1"),
            "expected button onclick to fire after clicking below SVG:\n{screen1}"
        );
    }

    #[tokio::test]
    async fn html_preview_inline_svg_pixel_protocol_queues_pending_graphic_and_skips_cells() {
        let (app, mut ui, _rx) = make_test_app_and_ui();
        ui.graphics_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };

        let html = r##"
            <h1>Pixel SVG</h1>
            <svg width="120" height="60"><rect width="120" height="60" fill="#16a34a"/></svg>
            <p>Caption below</p>
        "##;
        let mut ov = HtmlPreviewOverlay::new("Pixel SVG", html);

        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        ui.pending_pixel_graphics.clear();
        let mut all_cells_skipped = false;
        terminal
            .draw(|f| {
                ov.draw(f, f.area(), &app, &mut ui, &mut hitmap);
                if let Some(pg) = ui.pending_pixel_graphics.first() {
                    let buf = f.buffer_mut();
                    all_cells_skipped = true;
                    for y in pg.rect.top()..pg.rect.bottom() {
                        for x in pg.rect.left()..pg.rect.right() {
                            if !buf[(x, y)].skip {
                                all_cells_skipped = false;
                            }
                        }
                    }
                }
            })
            .unwrap();

        assert_eq!(
            ui.pending_pixel_graphics.len(),
            1,
            "Sixel inline SVG should queue 1 pending pixel graphic"
        );
        let pg = &ui.pending_pixel_graphics[0];
        assert!(pg.cache_key.starts_with("html:"));
        assert_eq!(pg.protocol, TerminalImageProtocol::Sixel);
        assert!(pg.payload.as_deref().is_some_and(|p| p.starts_with("\x1bP0;1;q\"1;1;")));
        assert!(pg.rect.width > 0 && pg.rect.height > 0);
        assert!(
            all_cells_skipped,
            "every pixel graphic cell in {:?} must be marked set_skip(true)",
            pg.rect
        );
    }
}
