//! Full-screen 24-bit Truecolor Half-Block image attachment viewer (`v`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview,
    models::AttachmentItem,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::HitMap,
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub owner_label: String,
    pub items: Vec<AttachmentItem>,
    pub index: usize,
    pub zoom_pct: u16,
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
        Some(Self {
            owner_label: owner_label.into(),
            items,
            index: start,
            zoom_pct: 100,
            error: None,
        })
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

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let base_cols = rect.width.saturating_sub(4).max(12);
        let base_rows = rect.height.saturating_sub(4).max(6);
        let scaled_cols =
            ((base_cols as u32 * self.zoom_pct as u32) / 100).clamp(8, base_cols as u32) as u16;
        let scaled_rows =
            ((base_rows as u32 * self.zoom_pct as u32) / 100).clamp(4, base_rows as u32) as u16;
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
    }

    pub fn zoom_out(&mut self) {
        self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
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
            Action::Input('d') | Action::DownloadAttachment => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            Action::Input('o') | Action::OpenInBrowser => {
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
        _hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);

        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let Some(cur) = self.current() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(Line::from(Span::styled(" 이미지 미리보기 ", theme.title(true))))
                .border_style(theme.border_style(true));
            frame.render_widget(Paragraph::new("표시할 이미지가 없습니다").block(block), rect);
            return;
        };

        let cache_key = AppState::image_cache_key(&cur.id, cols, rows);
        let cached = app.image_cache.get(&cache_key);
        let dim_info = match cached {
            Some(img) => format!(" · {}x{}", img.width_px, img.height_px),
            None => String::new(),
        };
        let title = format!(
            " 🖼 {} — {} ({}/{}){} · {}% ",
            self.owner_label,
            cur.filename,
            self.index + 1,
            self.items.len(),
            dim_info,
            self.zoom_pct
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let img_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);

        if let Some(rendered) = cached {
            let x_pad = img_area.width.saturating_sub(rendered.cols) / 2;
            let y_pad = img_area.height.saturating_sub(rendered.rows) / 2;
            let target_rect = Rect::new(
                img_area.x.saturating_add(x_pad),
                img_area.y.saturating_add(y_pad),
                rendered.cols.min(img_area.width),
                rendered.rows.min(img_area.height),
            );
            frame.render_widget(Paragraph::new(rendered.lines.clone()), target_rect);
        } else if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" 이미지 로드/디코드 실패: {err}"),
                    Style::default().fg(theme.error),
                )),
                img_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " 이미지를 불러오고 있습니다…",
                    theme.muted_style(),
                )),
                img_area,
            );
        }

        let footer = " [/]: 이전·다음   +/-: 확대·축소   d: 다운로드   o: 외부열기   Esc/q/v: 닫기 ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(footer, footer_area.width as usize),
                theme.muted_style(),
            )),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_images_and_handles_zoom_and_cycle() {
        let atts = vec![
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
                size: 200,
                author_name: None,
                created_at: String::new(),
                content_url: String::new(),
                thumbnail_url: None,
            },
            AttachmentItem {
                id: "3".into(),
                filename: "shot2.jpg".into(),
                mime_type: "image/jpeg".into(),
                size: 300,
                author_name: None,
                created_at: String::new(),
                content_url: String::new(),
                thumbnail_url: None,
            },
        ];

        let mut overlay = ImagePreviewOverlay::from_attachments("ATL-1", &atts, Some(2)).unwrap();
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(overlay.current().unwrap().id, "3");

        assert_eq!(
            overlay.handle(&Action::Input('[')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.current().unwrap().id, "2");

        overlay.handle(&Action::Input('+'));
        assert_eq!(overlay.zoom_pct, 125);
        overlay.handle(&Action::Input('-'));
        overlay.handle(&Action::Input('-'));
        assert_eq!(overlay.zoom_pct, 75);
        assert_eq!(overlay.handle(&Action::Input('v')), OverlayOutcome::Close);
    }
}
