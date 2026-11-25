//! Full-screen multi-protocol inline terminal image attachment viewer (`v`).
//! Supports Kitty Graphics, iTerm2 OSC 1337, DEC Sixel, and 24-bit Truecolor Half-Block (`▀`)
//! with prev/next cycling (`[`/`]`, `←`/`→`), zoom/fit (`+`/`-`/`0`/`f`), protocol cycle (`p`),
//! download (`d`), browser open (`o`), and close (`Esc`/`q`/`v`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{
        self, FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::AttachmentItem,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_pill_secondary, register_modal_chrome, render_button_bar,
            OverlayOutcome,
        },
    },
};

/// Pixel placement request emitted by [`ImagePreviewOverlay`] for synchronized post-frame rendering in `tui.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
}

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub owner_label: String,
    /// Site of the issue or page that owns `items`; attachment ids are only unique per site.
    pub site_id: Option<String>,
    pub items: Vec<AttachmentItem>,
    pub index: usize,
    /// Zoom percentage (`25..=300`, default `100`).
    pub zoom_pct: u16,
    /// Vertical viewport scroll offset (in terminal cell rows) for tall/zoomed images.
    pub scroll_y: u16,
    /// Horizontal viewport scroll offset (in terminal cell columns) for zoomed images.
    pub scroll_x: u16,
    /// Whether fit-to-modal mode (`100%`) is active; `f` toggles between Fit (`100%`) and Zoom (`150%`).
    pub fit_to_window: bool,
    /// Active graphics protocol for this preview overlay (`p` cycles Kitty -> iTerm2 -> Sixel -> Halfblock).
    pub protocol: TerminalImageProtocol,
    /// Whether the user manually cycled the protocol in this overlay session.
    pub protocol_overridden: bool,
    /// Probed terminal cell pixel dimensions `(cell_w, cell_h)` used for aspect-preserving pixel scaling.
    pub cell_px: (u16, u16),
    /// Whether `cell_px` came from an exact terminal/env measurement rather than a fallback estimate.
    pub cell_px_exact: bool,
    /// Currently painted pixel graphic `(cache_key, rect, protocol)` synchronized by `tui.rs`.
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_attachments(
        owner_label: impl Into<String>,
        attachments: &[AttachmentItem],
        preferred_idx: Option<usize>,
    ) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (raw_idx, att) in attachments.iter().enumerate() {
            if img_preview::is_image_attachment(&att.mime_type, &att.filename) {
                if preferred_idx == Some(raw_idx) {
                    start = items.len();
                }
                items.push(att.clone());
            }
        }
        if items.is_empty() {
            return None;
        }
        let caps = img_preview::detect_graphics_caps_auto();
        Some(Self {
            owner_label: owner_label.into(),
            site_id: None,
            items,
            index: start,
            zoom_pct: 100,
            scroll_y: 0,
            scroll_x: 0,
            fit_to_window: true,
            protocol: caps.protocol,
            protocol_overridden: false,
            cell_px: caps.cell_px,
            cell_px_exact: caps.cell_px_exact,
            active_pixel_graphic: None,
            error: None,
        })
    }

    pub fn with_graphics_caps(mut self, caps: TerminalGraphicsCaps) -> Self {
        self.apply_graphics_caps(caps);
        self
    }

    /// Applies startup-probed graphics capabilities if the user has not manually cycled protocol yet.
    pub fn apply_graphics_caps(&mut self, caps: TerminalGraphicsCaps) {
        self.cell_px = caps.cell_px;
        self.cell_px_exact = caps.cell_px_exact;
        if !self.protocol_overridden {
            self.protocol = caps.protocol;
        }
    }

    /// Returns the effective [`TerminalGraphicsCaps`] for rendering the current preview frame.
    pub fn graphics_caps(&self) -> TerminalGraphicsCaps {
        TerminalGraphicsCaps {
            protocol: self.protocol,
            cell_px: self.cell_px,
            cell_px_exact: self.cell_px_exact,
        }
    }

    pub fn current(&self) -> Option<&AttachmentItem> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    /// Splits the modal interior into `(optional_header_bar, image_canvas_area, footer_bar)`.
    pub fn content_areas(area: Rect) -> (Option<Rect>, Rect, Rect) {
        let rect = Self::modal_rect(area);
        let inner = Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        if inner.width == 0 || inner.height < 2 {
            return (None, Rect::default(), Rect::default());
        }
        if inner.height >= 4 {
            let header = Rect::new(inner.x, inner.y, inner.width, 1);
            let img_area = Rect::new(
                inner.x,
                inner.y.saturating_add(1),
                inner.width,
                inner.height.saturating_sub(2),
            );
            let footer = Rect::new(
                inner.x,
                inner.y.saturating_add(inner.height - 1),
                inner.width,
                1,
            );
            (Some(header), img_area, footer)
        } else {
            let img_area = Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            );
            let footer = Rect::new(
                inner.x,
                inner.y.saturating_add(inner.height - 1),
                inner.width,
                1,
            );
            (None, img_area, footer)
        }
    }

    /// Computes centered pixel/halfblock placement rect inside the modal canvas for a rendered image.
    pub fn placement_rect(area: Rect, rendered_cols: u16, rendered_rows: u16) -> Rect {
        let (_, img_area, _) = Self::content_areas(area);
        if img_area.width == 0 || img_area.height == 0 {
            return Rect::default();
        }
        let target_w = rendered_cols.min(img_area.width);
        let target_h = rendered_rows.min(img_area.height);
        let x_pad = img_area.width.saturating_sub(target_w) / 2;
        let y_pad = img_area.height.saturating_sub(target_h) / 2;
        Rect::new(
            img_area.x.saturating_add(x_pad),
            img_area.y.saturating_add(y_pad),
            target_w,
            target_h,
        )
    }

    /// Returns the modal's actual inner image canvas dimensions `(view_cols, view_rows)`.
    pub fn viewport_dimensions(term_width: u16, term_height: u16) -> (u16, u16) {
        let area = Rect::new(0, 0, term_width.max(24), term_height.max(12));
        let (_, img_area, _) = Self::content_areas(area);
        (img_area.width.max(1), img_area.height.max(1))
    }

    /// Builds the explicit [`FitBox`] sizing contract for the current terminal size and zoom level.
    pub fn fit_box(&self, term_width: u16, term_height: u16) -> FitBox {
        let (view_cols, view_rows) = Self::viewport_dimensions(term_width, term_height);
        FitBox::scrollable(view_cols, view_rows, self.zoom_pct)
    }

    /// Builds the viewport-, zoom-, cell-pixel-, and protocol-scoped cache key for `att`.
    pub fn preview_image_cache_key(
        &self,
        att: &AttachmentItem,
        term_width: u16,
        term_height: u16,
    ) -> String {
        let fit = self.fit_box(term_width, term_height);
        let cache_id = self.cache_id(att);
        img_preview::preview_cache_key(
            &cache_id,
            fit.view_cols,
            fit.view_rows,
            fit.zoom_pct,
            self.cell_px,
            self.protocol,
        )
    }

    pub fn next_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + 1) % self.items.len();
            self.scroll_y = 0;
            self.scroll_x = 0;
            self.error = None;
        }
    }

    pub fn prev_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + self.items.len() - 1) % self.items.len();
            self.scroll_y = 0;
            self.scroll_x = 0;
            self.error = None;
        }
    }

    pub fn zoom_in(&mut self) {
        self.zoom_pct = (self.zoom_pct + 25).min(300);
        self.fit_to_window = self.zoom_pct == 100;
    }

    pub fn zoom_out(&mut self) {
        self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.fit_to_window = self.zoom_pct == 100;
    }

    pub fn reset_zoom(&mut self) {
        self.zoom_pct = 100;
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.fit_to_window = true;
    }

    /// Toggles between Fit-to-Window (`100%`) and Zoomed (`150%`).
    pub fn toggle_zoom_fit(&mut self) {
        if self.fit_to_window && self.zoom_pct == 100 {
            self.zoom_pct = 150;
            self.fit_to_window = false;
        } else {
            self.reset_zoom();
        }
    }

    /// Cycles the active terminal graphics protocol (`Kitty -> iTerm2 -> Sixel -> Halfblock -> Kitty`).
    pub fn cycle_protocol(&mut self) -> TerminalImageProtocol {
        self.protocol = self.protocol.next_manual();
        self.protocol_overridden = true;
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.error = None;
        self.protocol
    }

    /// Cache identity of `att`: the attachment id, qualified by site when the owner's site is known.
    pub fn cache_id(&self, att: &AttachmentItem) -> String {
        match self.site_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(site) => format!("{}/{}", site.to_ascii_lowercase(), att.id),
            None => att.id.clone(),
        }
    }

    /// Resolves the protocol-aware cache key and cached rendered image for the current attachment.
    pub fn lookup_cached<'a>(
        &self,
        area: Rect,
        app: &'a AppState,
    ) -> Option<(String, &'a RenderedTerminalImage)> {
        let cur = self.current()?;
        let proto_key = self.preview_image_cache_key(cur, area.width, area.height);
        if self.protocol.is_pixel() {
            return app
                .pixel_image_cache
                .get(&proto_key)
                .map(|image| (proto_key, image));
        }
        app.image_cache
            .get(&proto_key)
            .map(|rendered| (proto_key, rendered))
    }

    /// Computes viewport geometry `(avail_w, avail_h, show_scrollbar, max_scroll_x, max_scroll_y)` for a rendered image.
    fn viewport_layout(
        &self,
        img_area: Rect,
        rendered: &RenderedTerminalImage,
    ) -> (u16, u16, bool, u16, u16) {
        let is_pixel = rendered.protocol.is_pixel() && rendered.pixel_payload.is_some();
        let total_rows = if is_pixel {
            rendered.rows
        } else {
            rendered.lines.len() as u16
        };
        let total_cols = rendered.cols;
        let avail_h = if is_pixel
            && !self.cell_px_exact
            && rendered.protocol == TerminalImageProtocol::Sixel
        {
            img_area.height.saturating_sub(1).max(1)
        } else {
            img_area.height
        };
        let show_scrollbar = total_rows > avail_h && img_area.width > 2;
        let avail_w = if show_scrollbar {
            img_area.width.saturating_sub(1)
        } else {
            img_area.width
        };
        let max_scroll_y = total_rows.saturating_sub(avail_h);
        let max_scroll_x = total_cols.saturating_sub(avail_w);
        (avail_w, avail_h, show_scrollbar, max_scroll_x, max_scroll_y)
    }

    /// Computes the synchronized pixel placement descriptor if the active protocol is pixel-based and cached.
    pub fn pending_pixel_graphic(
        &self,
        area: Rect,
        app: &AppState,
    ) -> Option<PendingPixelGraphic> {
        if !self.protocol.is_pixel() {
            return None;
        }
        let (cache_key, rendered) = self.lookup_cached(area, app)?;
        if !rendered.protocol.is_pixel() || rendered.pixel_payload.is_none() {
            return None;
        }
        let (_, img_area, _) = Self::content_areas(area);
        if img_area.width == 0 || img_area.height == 0 {
            return None;
        }
        let (avail_w, avail_h, _, max_scroll_x, max_scroll_y) =
            self.viewport_layout(img_area, rendered);
        let sx = self.scroll_x.min(max_scroll_x);
        let sy = self.scroll_y.min(max_scroll_y);
        let draw_w = rendered.cols.saturating_sub(sx).min(avail_w);
        let draw_h = rendered.rows.saturating_sub(sy).min(avail_h);
        if draw_w == 0 || draw_h == 0 {
            return None;
        }
        let x_pad = avail_w.saturating_sub(draw_w) / 2;
        let y_pad = avail_h.saturating_sub(draw_h) / 2;
        let rect = Rect::new(
            img_area.x.saturating_add(x_pad),
            img_area.y.saturating_add(y_pad),
            draw_w,
            draw_h,
        );
        let needs_crop =
            sx > 0 || sy > 0 || rendered.cols > avail_w || rendered.rows > avail_h;
        let effective_key = if needs_crop {
            format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}")
        } else {
            cache_key
        };
        Some(PendingPixelGraphic {
            cache_key: effective_key,
            rect,
            protocol: rendered.protocol,
        })
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v')
            | Action::Quit => OverlayOutcome::Close,
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::ScrollUp => {
                self.scroll_y = self.scroll_y.saturating_sub(3);
                OverlayOutcome::Handled
            }
            Action::ScrollDown => {
                self.scroll_y = self.scroll_y.saturating_add(3);
                OverlayOutcome::Handled
            }
            Action::MouseScrollHoriz { delta, .. } => {
                let step = (delta.unsigned_abs() as u16).saturating_mul(4).max(4);
                if *delta < 0 {
                    self.scroll_x = self.scroll_x.saturating_sub(step);
                } else if *delta > 0 {
                    self.scroll_x = self.scroll_x.saturating_add(step);
                }
                OverlayOutcome::Handled
            }
            Action::GoTop | Action::CursorHome | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::GoBottom | Action::CursorEnd | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::Input('h') | Action::CursorLeft => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') | Action::CursorRight => {
                self.scroll_x = self.scroll_x.saturating_add(4);
                OverlayOutcome::Handled
            }
            Action::Input('[') | Action::MoveLeft => {
                self.prev_image();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input(']') | Action::MoveRight => {
                self.next_image();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_in();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_out();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('0') => {
                self.reset_zoom();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('f') | Action::Input('F') => {
                self.toggle_zoom_fit();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('p') | Action::Input('P') | Action::CycleImageProtocol => {
                self.cycle_protocol();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('d') | Action::Input('D') | Action::DownloadAttachment => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            Action::Input('o') | Action::Input('O') | Action::OpenInBrowser => {
                OverlayOutcome::Dispatch(Action::OpenInBrowser)
            }
            Action::LoadPreviewImage => OverlayOutcome::Dispatch(Action::LoadPreviewImage),
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let _ = self.draw_with_pending(frame, area, app, ui, hitmap);
    }

    /// Draws the image preview modal and returns any pending synchronized pixel graphic placement.
    pub fn draw_with_pending(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) -> Option<PendingPixelGraphic> {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);

        let modal_style = Style::default().bg(theme.card_bg).fg(theme.fg);
        let close_w = "[✕ Esc]".width() as u16;
        let show_close_pill = rect.width >= 16;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let Some(cur) = self.current().cloned() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(modal_style)
                .title(Line::from(Span::styled(" Image Preview ", theme.title(true))))
                .border_style(theme.border_style(true));
            frame.render_widget(
                Paragraph::new("No image attachments to display").block(block),
                rect,
            );
            register_modal_chrome(rect, frame, theme, hitmap, true);
            return None;
        };

        let cached_entry = self.lookup_cached(area, app);
        let dim_info = match cached_entry {
            Some((_, img)) => format!(" · {}x{} {}", img.width_px, img.height_px, img.format_label),
            None => String::new(),
        };
        let proto_label = self.protocol.label();
        let zoom_label = if self.fit_to_window && self.zoom_pct == 100 {
            "Fit".to_string()
        } else {
            format!("{}%", self.zoom_pct)
        };
        let title = format!(
            " {} — {} ({}/{}){} · {} [{}] ",
            self.owner_label,
            cur.filename,
            self.index + 1,
            self.items.len(),
            dim_info,
            zoom_label,
            proto_label
        );

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
        register_modal_chrome(rect, frame, theme, hitmap, true);

        if inner.width == 0 || inner.height < 2 {
            return None;
        }

        let (header_area, img_area, footer_area) = Self::content_areas(area);

        if let Some(hdr) = header_area {
            let mut spans = Vec::new();
            let mut cx = hdr.x;
            let push_span = |spans: &mut Vec<Span<'static>>, cx: &mut u16, text: String, style: Style| {
                let w = text.width() as u16;
                spans.push(Span::styled(text, style));
                *cx = cx.saturating_add(w);
            };

            push_span(
                &mut spans,
                &mut cx,
                format!(" {} ", cur.filename),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
            if let Some((_, img)) = cached_entry {
                push_span(
                    &mut spans,
                    &mut cx,
                    format!("{}x{} {} ", img.width_px, img.height_px, img.format_label),
                    theme.muted_style(),
                );
            } else if !cur.mime_type.trim().is_empty() {
                push_span(
                    &mut spans,
                    &mut cx,
                    format!("{} ", cur.mime_type.trim()),
                    theme.muted_style(),
                );
            }
            if cur.size > 0 {
                push_span(
                    &mut spans,
                    &mut cx,
                    format!("· {} ", format_file_size(cur.size)),
                    theme.muted_style(),
                );
            }
            push_span(
                &mut spans,
                &mut cx,
                format!("· {} ", zoom_label),
                theme.muted_style(),
            );
            let proto_text = format!(" [{proto_label}] ");
            let proto_w = proto_text.width() as u16;
            let proto_x = cx;
            if proto_x < hdr.right() {
                let hit_w = proto_w.min(hdr.right().saturating_sub(proto_x));
                hitmap.add_line(
                    proto_x,
                    hdr.y,
                    hit_w,
                    HitTarget::Button(ButtonId::OverlayImageProtocol),
                );
            }
            push_span(
                &mut spans,
                &mut cx,
                proto_text,
                Style::default()
                    .bg(theme.chip_bg)
                    .fg(theme.chip_fg)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(modal_style),
                hdr,
            );
        }

        let mut pending = None;

        if let Some((cache_key, rendered)) = cached_entry {
            let (avail_w, avail_h, show_scrollbar, max_scroll_x, max_scroll_y) =
                self.viewport_layout(img_area, rendered);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sx = self.scroll_x;
            let sy = self.scroll_y;

            let is_pixel = rendered.protocol.is_pixel() && rendered.pixel_payload.is_some();
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let draw_w = rendered.cols.saturating_sub(sx).min(avail_w);
            let draw_h = total_rows.saturating_sub(sy).min(avail_h);
            let x_pad = avail_w.saturating_sub(draw_w) / 2;
            let y_pad = avail_h.saturating_sub(draw_h) / 2;
            let target_rect = Rect::new(
                img_area.x.saturating_add(x_pad),
                img_area.y.saturating_add(y_pad),
                draw_w,
                draw_h,
            );

            if is_pixel {
                if target_rect.width > 0 && target_rect.height > 0 {
                    let needs_crop =
                        sx > 0 || sy > 0 || rendered.cols > avail_w || rendered.rows > avail_h;
                    let effective_key = if needs_crop {
                        format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}")
                    } else {
                        cache_key
                    };
                    pending = Some(PendingPixelGraphic {
                        cache_key: effective_key.clone(),
                        rect: target_rect,
                        protocol: rendered.protocol,
                    });
                    if self.active_pixel_graphic.as_ref()
                        == Some(&(effective_key, target_rect, rendered.protocol))
                    {
                        let buf = frame.buffer_mut();
                        for y in target_rect.top()..target_rect.bottom() {
                            for x in target_rect.left()..target_rect.right() {
                                if let Some(cell) = buf.cell_mut((x, y)) {
                                    cell.set_skip(true);
                                }
                            }
                        }
                    }
                }
            } else if target_rect.width > 0 && target_rect.height > 0 {
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    target_rect,
                );
            }

            if show_scrollbar {
                let sb_area = Rect::new(
                    img_area.right().saturating_sub(1),
                    img_area.y,
                    1,
                    img_area.height,
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
                    format!(" Failed to load/decode image: {err}"),
                    Style::default().fg(theme.error),
                ))
                .style(modal_style),
                img_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(" Loading image…", theme.muted_style()))
                    .style(modal_style),
                img_area,
            );
        }

        let pill = modal_pill_secondary(theme);
        render_button_bar(
            frame,
            footer_area,
            &[
                ("[←]", ButtonId::OverlayImagePrev, pill),
                ("[→]", ButtonId::OverlayImageNext, pill),
                ("[-]", ButtonId::OverlayImageZoomOut, pill),
                ("[+]", ButtonId::OverlayImageZoomIn, pill),
                ("[Fit]", ButtonId::OverlayImageFit, pill),
                ("[d] Download", ButtonId::DownloadAttachment, pill),
                ("[o] Browser", ButtonId::OpenBrowser, pill),
            ],
            Some("· Wheel scroll · p protocol · Esc close"),
            theme,
            hitmap,
        );

        pending
    }
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes as f64 >= KB {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        atlassian::AtlassianClient,
        auth::AuthManager,
        config::{AppConfig, Persona},
        img_preview::DecodedImageCache,
        storage::Storage,
        sync,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::mpsc;

    fn sample_attachments() -> Vec<AttachmentItem> {
        vec![
            AttachmentItem {
                id: "1".into(),
                filename: "spec.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 100,
                author_name: None,
                created_at: String::new(),
                content_url: String::new(),
                thumbnail_url: None,
            },
            AttachmentItem {
                id: "2".into(),
                filename: "shot1.png".into(),
                mime_type: "image/png".into(),
                size: 2048,
                author_name: None,
                created_at: String::new(),
                content_url: "https://acme.atlassian.net/att/2".into(),
                thumbnail_url: None,
            },
            AttachmentItem {
                id: "3".into(),
                filename: "shot2.jpg".into(),
                mime_type: "image/jpeg".into(),
                size: 15_360,
                author_name: None,
                created_at: String::new(),
                content_url: "https://acme.atlassian.net/att/3".into(),
                thumbnail_url: None,
            },
        ]
    }

    fn make_test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[tokio::test]
    async fn halfblock_loads_use_the_rendered_image_budget() {
        use crate::ui::app::UiMsg;

        let (mut app, mut ui) = make_test_app_and_ui();
        let image = |size| RenderedTerminalImage {
            width_px: 384,
            height_px: 192,
            cols: 1,
            rows: 1,
            format_label: "PNG".into(),
            protocol: TerminalImageProtocol::Halfblock,
            lines: vec![Line::raw("▀".repeat(size))],
            pixel_payload: None,
            scroll_backing: None,
        };
        let size = image(128).approx_heap_bytes();
        app.image_cache = DecodedImageCache::with_limits(size * 2, 64);
        let mut overlay =
            ImagePreviewOverlay::from_attachments("ATL-1", &sample_attachments(), None).unwrap();
        overlay.protocol = TerminalImageProtocol::Halfblock;
        let area = Rect::new(0, 0, 140, 40);
        let fit = overlay.fit_box(area.width, area.height);
        let key = |id| {
            img_preview::preview_cache_key(
                id,
                fit.view_cols,
                fit.view_rows,
                fit.zoom_pct,
                overlay.cell_px,
                TerminalImageProtocol::Halfblock,
            )
        };
        for id in ["1", "2", "3"] {
            app.handle_ui_msg(
                &mut ui,
                UiMsg::ImageLoaded {
                    cache_key: key(id),
                    rendered: Ok(image(128)),
                },
            );
        }
        assert_eq!(app.image_cache.total_bytes(), size * 2);
        assert!(!app.image_cache.contains_key(&key("1")));
        assert!(app.image_cache.contains_key(&key("3")));

        app.handle_ui_msg(
            &mut ui,
            UiMsg::ImageLoaded {
                cache_key: key("2"),
                rendered: Ok(image(4096)),
            },
        );
        assert_eq!(app.image_cache.len(), 1);
        assert!(app.image_cache.total_bytes() > app.image_cache.max_bytes());
        assert_eq!(overlay.lookup_cached(area, &app).unwrap().0, key("2"));
    }

    #[test]
    fn filters_images_and_handles_zoom_fit_protocol_and_cycle() {
        let atts = sample_attachments();
        let mut overlay = ImagePreviewOverlay::from_attachments("ATL-1", &atts, Some(2))
            .unwrap()
            .with_graphics_caps(TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Kitty,
                cell_px: (8, 16),
                cell_px_exact: true,
            });
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(overlay.current().unwrap().id, "3");
        assert!(overlay.fit_to_window);
        assert_eq!(overlay.protocol, TerminalImageProtocol::Kitty);

        assert_eq!(
            overlay.handle(&Action::Input('[')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.current().unwrap().id, "2");

        // Zoom in / out / fit toggle / reset
        overlay.handle(&Action::Input('+'));
        assert_eq!(overlay.zoom_pct, 125);
        assert!(!overlay.fit_to_window);
        let fit_125 = overlay.fit_box(120, 40);

        overlay.handle(&Action::Input('0'));
        assert_eq!(overlay.zoom_pct, 100);
        assert!(overlay.fit_to_window);
        let fit_100 = overlay.fit_box(120, 40);
        assert_eq!(fit_125.view_cols, fit_100.view_cols);
        assert_eq!(fit_125.view_rows, fit_100.view_rows);
        assert!(fit_125.zoom_pct > fit_100.zoom_pct);
        assert!(fit_100.scrollable);

        // Verify zoom scales up to 300%
        for _ in 0..10 {
            overlay.handle(&Action::Input('+'));
        }
        assert_eq!(overlay.zoom_pct, 300);
        overlay.handle(&Action::Input('0'));
        assert_eq!(overlay.zoom_pct, 100);

        overlay.handle(&Action::Input('f'));
        assert_eq!(overlay.zoom_pct, 150);
        assert!(!overlay.fit_to_window);
        overlay.handle(&Action::Input('f'));
        assert_eq!(overlay.zoom_pct, 100);
        assert!(overlay.fit_to_window);

        overlay.handle(&Action::Input('-'));
        assert_eq!(overlay.zoom_pct, 75);

        // Protocol cycling (`p`)
        assert_eq!(
            overlay.handle(&Action::Input('p')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.protocol, TerminalImageProtocol::ITerm2);
        assert!(overlay.protocol_overridden);

        overlay.handle(&Action::Input('p'));
        assert_eq!(overlay.protocol, TerminalImageProtocol::Sixel);
        overlay.handle(&Action::Input('p'));
        assert_eq!(overlay.protocol, TerminalImageProtocol::Halfblock);
        overlay.handle(&Action::Input('p'));
        assert_eq!(overlay.protocol, TerminalImageProtocol::Kitty);

        assert_eq!(
            overlay.handle(&Action::Input('d')),
            OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
        );
        assert_eq!(
            overlay.handle(&Action::Input('o')),
            OverlayOutcome::Dispatch(Action::OpenInBrowser)
        );
        assert_eq!(overlay.handle(&Action::Input('v')), OverlayOutcome::Close);
    }

    #[tokio::test]
    async fn draw_renders_protocol_badges_metadata_and_skips_active_pixel_cells() {
        let (mut app, ui) = make_test_app_and_ui();
        let atts = sample_attachments();
        let mut overlay = ImagePreviewOverlay::from_attachments("ATL-42", &atts, Some(1)).unwrap();
        let area = Rect::new(0, 0, 100, 30);

        for (proto, badge) in [
            (TerminalImageProtocol::Sixel, "[Sixel]"),
            (TerminalImageProtocol::Kitty, "[Kitty]"),
            (TerminalImageProtocol::ITerm2, "[iTerm2]"),
            (TerminalImageProtocol::Halfblock, "[Halfblock]"),
        ] {
            overlay.protocol = proto;
            overlay.active_pixel_graphic = None;
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::default();
            terminal
                .draw(|frame| {
                    overlay.draw(frame, area, &app, &ui, &mut hitmap);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let top_row: String = (0..area.width)
                .map(|x| buf[(x, 1)].symbol())
                .collect();
            let header_row: String = (0..area.width)
                .map(|x| buf[(x, 2)].symbol())
                .collect();
            assert!(
                top_row.contains(badge),
                "expected {badge} in modal title row: {top_row}"
            );
            assert!(
                header_row.contains("shot1.png") && header_row.contains(badge),
                "expected filename and {badge} in metadata header row: {header_row}"
            );
        }

        // Insert a cached Sixel image and verify pending_pixel_graphic + cell skip behavior.
        let proto = TerminalImageProtocol::Sixel;
        overlay.protocol = proto;
        let cur = overlay.current().unwrap();
        let cache_key = overlay.preview_image_cache_key(cur, area.width, area.height);
        app.pixel_image_cache.insert(
            cache_key.clone(),
            RenderedTerminalImage {
                width_px: 320,
                height_px: 200,
                cols: 40,
                rows: 12,
                format_label: "PNG".to_string(),
                protocol: proto,
                lines: Vec::new(),
                pixel_payload: Some("\x1bP0;1;q\"1;1;10;10#0;2;100;0;0#0!10~\x1b\\".to_string()),
                scroll_backing: None,
            },
        );

        let pending = overlay
            .pending_pixel_graphic(area, &app)
            .expect("pending pixel graphic must be computed for cached Sixel image");
        assert_eq!(pending.cache_key, cache_key);
        assert_eq!(pending.protocol, proto);
        assert_eq!(pending.rect.width, 40);
        assert_eq!(pending.rect.height, 12);

        overlay.active_pixel_graphic =
            Some((pending.cache_key.clone(), pending.rect, pending.protocol));

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        let mut skipped_inside = 0usize;
        let mut unskipped_border = 0usize;
        terminal
            .draw(|frame| {
                let drawn_pending = overlay.draw_with_pending(frame, area, &app, &ui, &mut hitmap);
                assert_eq!(drawn_pending, Some(pending.clone()));
                let buf = frame.buffer_mut();
                for y in pending.rect.top()..pending.rect.bottom() {
                    for x in pending.rect.left()..pending.rect.right() {
                        if buf[(x, y)].skip {
                            skipped_inside += 1;
                        }
                    }
                }
                let modal = ImagePreviewOverlay::modal_rect(area);
                if !buf[(modal.x, modal.y)].skip {
                    unskipped_border += 1;
                }
            })
            .unwrap();

        assert_eq!(
            skipped_inside,
            (pending.rect.width as usize) * (pending.rect.height as usize)
        );
        assert_eq!(unskipped_border, 1);

        let _ = app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn tall_image_scrolls_clamps_renders_scrollbar_and_close_pill() {
        use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
        use std::io::Cursor;

        let img: RgbaImage = ImageBuffer::from_fn(400, 2000, |x, y| {
            Rgba([
                ((x * 255) / 400) as u8,
                ((y * 255) / 2000) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        let png = buf.into_inner();

        let area = Rect::new(0, 0, 100, 30);
        let atts = sample_attachments();

        for proto in [TerminalImageProtocol::Halfblock, TerminalImageProtocol::Sixel] {
            let (mut app, ui) = make_test_app_and_ui();
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let mut overlay = ImagePreviewOverlay::from_attachments("ATL-99", &atts, Some(1))
                .unwrap()
                .with_graphics_caps(caps);
            let fit = overlay.fit_box(area.width, area.height);
            let rendered =
                img_preview::decode_and_render_for_protocol(&png, fit, (25, 25, 25), caps)
                    .unwrap();
            let cur = overlay.current().unwrap();
            let base_key = overlay.preview_image_cache_key(cur, area.width, area.height);
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let (_, img_area, _) = ImagePreviewOverlay::content_areas(area);
            let expected_max_scroll = total_rows.saturating_sub(img_area.height);
            assert!(expected_max_scroll > 0, "expected scrollable tall image for {proto:?}");

            if proto.is_pixel() {
                app.pixel_image_cache.insert(base_key.clone(), rendered);
            } else {
                app.image_cache.insert(base_key.clone(), rendered);
            }

            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::new();
            let mut pg0 = None;
            terminal
                .draw(|frame| {
                    pg0 = overlay.draw_with_pending(frame, area, &app, &ui, &mut hitmap);
                })
                .unwrap();

            // Modal registers OverlayBody and clickable [✕ Esc] close pill
            assert!(hitmap.region_of(HitTarget::OverlayBody).is_some());
            assert!(hitmap.region_of(HitTarget::OverlayClose).is_some());

            // Scrollbar column rendered on right-most column of img_area
            let sb_x = img_area.right() - 1;
            let has_sb = (img_area.top()..img_area.bottom()).any(|y| {
                !terminal.backend().buffer()[(sb_x, y)]
                    .symbol()
                    .trim()
                    .is_empty()
            });
            assert!(has_sb, "expected scrollbar for tall {proto:?} image");

            if proto == TerminalImageProtocol::Sixel {
                let p = pg0.expect("expected Sixel pending graphic");
                assert!(
                    p.cache_key.starts_with(&format!("{base_key}#crop:0,0,")),
                    "unexpected crop key: {}",
                    p.cache_key
                );
            }

            // Scroll down via j / wheel / PageDown / G and clamp in-place
            assert_eq!(overlay.handle(&Action::Input('j')), OverlayOutcome::Handled);
            assert_eq!(overlay.handle(&Action::ScrollDown), OverlayOutcome::Handled);
            assert_eq!(overlay.handle(&Action::GoBottom), OverlayOutcome::Handled);
            assert_eq!(overlay.scroll_y, u16::MAX);

            let mut pg_bottom = None;
            terminal
                .draw(|frame| {
                    hitmap.clear();
                    pg_bottom = overlay.draw_with_pending(frame, area, &app, &ui, &mut hitmap);
                })
                .unwrap();
            assert_eq!(overlay.scroll_y, expected_max_scroll);

            if proto == TerminalImageProtocol::Sixel {
                let p = pg_bottom.expect("expected Sixel pending graphic at bottom");
                assert!(p
                    .cache_key
                    .starts_with(&format!("{base_key}#crop:0,{expected_max_scroll},")));
            }

            // Wheel up responds immediately from clamped bottom
            overlay.handle(&Action::ScrollUp);
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));

            let _ = app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
        }
    }
}
