//! Full-screen multi-protocol inline terminal image attachment viewer (`v`).
//! Supports Kitty Graphics, iTerm2 OSC 1337, DEC Sixel, and 24-bit Truecolor Half-Block (`▀`)
//! with prev/next cycling (`[`/`]`, `←`/`→`), zoom/fit (`+`/`-`/`0`/`f`), protocol cycle (`p`),
//! download (`d`), browser open (`o`), and close (`Esc`/`q`/`v`).

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview::{
        self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::AttachmentItem,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::HitMap,
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
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
    pub items: Vec<AttachmentItem>,
    pub index: usize,
    /// Zoom percentage (`25..=200`, default `100`).
    pub zoom_pct: u16,
    /// Whether fit-to-modal mode (`100%`) is active; `f` toggles between Fit (`100%`) and Zoom (`150%`).
    pub fit_to_window: bool,
    /// Active graphics protocol for this preview overlay (`p` cycles Kitty -> iTerm2 -> Sixel -> Halfblock).
    pub protocol: TerminalImageProtocol,
    /// Whether the user manually cycled the protocol in this overlay session.
    pub protocol_overridden: bool,
    /// Probed terminal cell pixel dimensions `(cell_w, cell_h)` used for aspect-preserving pixel scaling.
    pub cell_px: (u16, u16),
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
            items,
            index: start,
            zoom_pct: 100,
            fit_to_window: true,
            protocol: caps.protocol,
            protocol_overridden: false,
            cell_px: caps.cell_px,
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
        if !self.protocol_overridden {
            self.protocol = caps.protocol;
        }
    }

    /// Returns the effective [`TerminalGraphicsCaps`] for rendering the current preview frame.
    pub fn graphics_caps(&self) -> TerminalGraphicsCaps {
        TerminalGraphicsCaps {
            protocol: self.protocol,
            cell_px: self.cell_px,
            cell_px_exact: true,
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

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let area = Rect::new(0, 0, term_width.max(24), term_height.max(12));
        let (_, img_area, _) = Self::content_areas(area);
        let max_cols = img_area.width.max(12);
        let max_rows = img_area.height.max(6);
        let base_cols = max_cols.saturating_sub(2).max(12).min(max_cols);
        let base_rows = max_rows.saturating_sub(1).max(6).min(max_rows);
        let scaled_cols =
            ((base_cols as u32 * self.zoom_pct as u32) / 100).clamp(8, max_cols as u32) as u16;
        let scaled_rows =
            ((base_rows as u32 * self.zoom_pct as u32) / 100).clamp(4, max_rows as u32) as u16;
        (scaled_cols, scaled_rows)
    }

    pub fn next_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + 1) % self.items.len();
            self.error = None;
        }
    }

    pub fn prev_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + self.items.len() - 1) % self.items.len();
            self.error = None;
        }
    }

    pub fn zoom_in(&mut self) {
        self.zoom_pct = (self.zoom_pct + 25).min(200);
        self.fit_to_window = self.zoom_pct == 100;
    }

    pub fn zoom_out(&mut self) {
        self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
        self.fit_to_window = self.zoom_pct == 100;
    }

    pub fn reset_zoom(&mut self) {
        self.zoom_pct = 100;
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
        self.error = None;
        self.protocol
    }

    /// Resolves the protocol-aware cache key and cached rendered image for the current attachment.
    pub fn lookup_cached<'a>(
        &self,
        area: Rect,
        image_cache: &'a HashMap<String, RenderedTerminalImage>,
    ) -> Option<(String, &'a RenderedTerminalImage)> {
        let cur = self.current()?;
        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let proto_key = img_preview::preview_cache_key(&cur.id, cols, rows, self.protocol);
        if let Some(rendered) = image_cache.get(&proto_key) {
            return Some((proto_key, rendered));
        }
        if self.protocol == TerminalImageProtocol::Halfblock {
            let legacy_key = AppState::image_cache_key(&cur.id, cols, rows);
            if let Some(rendered) = image_cache.get(&legacy_key) {
                return Some((legacy_key, rendered));
            }
        }
        None
    }

    /// Computes the synchronized pixel placement descriptor if the active protocol is pixel-based and cached.
    pub fn pending_pixel_graphic(
        &self,
        area: Rect,
        image_cache: &HashMap<String, RenderedTerminalImage>,
    ) -> Option<PendingPixelGraphic> {
        if !self.protocol.is_pixel() {
            return None;
        }
        let (cache_key, rendered) = self.lookup_cached(area, image_cache)?;
        if !rendered.protocol.is_pixel() || rendered.pixel_payload.is_none() {
            return None;
        }
        let rect = Self::placement_rect(area, rendered.cols, rendered.rows);
        if rect.width == 0 || rect.height == 0 {
            return None;
        }
        Some(PendingPixelGraphic {
            cache_key,
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
            Action::Input('p') | Action::Input('P') => {
                self.cycle_protocol();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('d') | Action::Input('D') | Action::DownloadAttachment => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            Action::Input('o') | Action::Input('O') | Action::OpenInBrowser => {
                OverlayOutcome::Dispatch(Action::OpenInBrowser)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
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
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        _hitmap: &mut HitMap,
    ) -> Option<PendingPixelGraphic> {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);

        let modal_style = Style::default().bg(theme.card_bg).fg(theme.fg);

        let Some(cur) = self.current() else {
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
            return None;
        };

        let cached_entry = self.lookup_cached(area, &app.image_cache);
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
                truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return None;
        }

        let (header_area, img_area, footer_area) = Self::content_areas(area);

        if let Some(hdr) = header_area {
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", cur.filename),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((_, img)) = cached_entry {
                spans.push(Span::styled(
                    format!("{}x{} {} ", img.width_px, img.height_px, img.format_label),
                    theme.muted_style(),
                ));
            } else if !cur.mime_type.trim().is_empty() {
                spans.push(Span::styled(
                    format!("{} ", cur.mime_type.trim()),
                    theme.muted_style(),
                ));
            }
            if cur.size > 0 {
                spans.push(Span::styled(
                    format!("· {} ", format_file_size(cur.size)),
                    theme.muted_style(),
                ));
            }
            spans.push(Span::styled(
                format!("· {} ", zoom_label),
                theme.muted_style(),
            ));
            spans.push(Span::styled(
                format!(" [{proto_label}] "),
                Style::default()
                    .bg(theme.chip_bg)
                    .fg(theme.chip_fg)
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(modal_style),
                hdr,
            );
        }

        let mut pending = None;

        if let Some((cache_key, rendered)) = cached_entry {
            let target_rect = Self::placement_rect(area, rendered.cols, rendered.rows);
            if rendered.protocol.is_pixel() && rendered.pixel_payload.is_some() {
                if target_rect.width > 0 && target_rect.height > 0 {
                    pending = Some(PendingPixelGraphic {
                        cache_key: cache_key.clone(),
                        rect: target_rect,
                        protocol: rendered.protocol,
                    });
                    if self.active_pixel_graphic.as_ref()
                        == Some(&(cache_key, target_rect, rendered.protocol))
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
            } else {
                frame.render_widget(Paragraph::new(rendered.lines.clone()), target_rect);
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

        let footer =
            " [/]: Prev/Next   +/- (0/f): Zoom/Fit   p: Protocol   d: Download   o: Browser   Esc/q/v: Close ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(footer, footer_area.width as usize),
                theme.muted_style(),
            ))
            .style(modal_style),
            footer_area,
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
        let (cols_125, rows_125) = overlay.target_dimensions(120, 40);

        overlay.handle(&Action::Input('0'));
        assert_eq!(overlay.zoom_pct, 100);
        assert!(overlay.fit_to_window);
        let (cols_100, rows_100) = overlay.target_dimensions(120, 40);
        assert!(cols_125 > cols_100);
        assert!(rows_125 > rows_100);

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
        let (cols, rows) = overlay.target_dimensions(area.width, area.height);

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
        let cache_key = img_preview::preview_cache_key(&cur.id, cols, rows, proto);
        app.image_cache.insert(
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
            },
        );

        let pending = overlay
            .pending_pixel_graphic(area, &app.image_cache)
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
}
