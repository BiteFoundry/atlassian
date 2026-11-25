//! Attachment browser modal for listing, previewing (`Enter`/`v`), downloading (`d`),
//! or uploading (`u`) attachments on the active Jira issue or Confluence page.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview,
    models::AttachmentItem,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, html_preview, modal_block, modal_pill_primary, modal_pill_secondary,
            register_modal_chrome, render_button_bar, OverlayOutcome,
        },
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    pub index: usize,
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub author_name: Option<String>,
    pub is_image: bool,
    pub is_html: bool,
    pub is_markdown: bool,
}

impl AttachmentRow {
    pub fn from_item(index: usize, item: &AttachmentItem) -> Self {
        let filename = if item.filename.trim().is_empty() {
            format!("attachment-{}", index + 1)
        } else {
            item.filename.clone()
        };
        let is_image = img_preview::is_image_attachment(&item.mime_type, &filename);
        let is_html = !is_image && html_preview::is_html_attachment(&item.mime_type, &filename);
        let is_markdown = !is_image
            && !is_html
            && html_preview::is_markdown_attachment(&item.mime_type, &filename);
        Self {
            index,
            id: item.id.clone(),
            filename,
            mime_type: item.mime_type.clone(),
            size: item.size,
            author_name: item.author_name.clone(),
            is_image,
            is_html,
            is_markdown,
        }
    }

    pub fn icon(&self) -> &'static str {
        if self.is_image {
            "[IMG]"
        } else if self.is_html {
            "[HTML]"
        } else if self.is_markdown {
            "[MD]"
        } else {
            "[FILE]"
        }
    }

    pub fn label(&self) -> String {
        let size_part = format_size(self.size);
        let size_suffix = if size_part.is_empty() {
            String::new()
        } else {
            format!(" ({size_part})")
        };
        let author_suffix = self
            .author_name
            .as_deref()
            .filter(|a| !a.trim().is_empty())
            .map(|a| format!(" · {a}"))
            .unwrap_or_default();
        format!("{} {}{}{}", self.icon(), self.filename, size_suffix, author_suffix)
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Action intent recorded by `AttachmentsOverlay` for coordinator wiring in `action.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentCommand {
    PreviewImage(usize),
    PreviewHtml(usize),
    Download(usize),
    Upload,
    Refresh,
    OpenUrl(String),
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentsOverlay {
    pub owner_label: String,
    pub items: Vec<AttachmentItem>,
    pub selected: usize,
    pub notice: Option<String>,
    pub pending_command: Option<AttachmentCommand>,
}

/// Alias aligned with `teams` / `outlook` overlay naming.
pub type AttachmentsState = AttachmentsOverlay;

impl AttachmentsOverlay {
    pub fn new(owner_label: impl Into<String>, items: Vec<AttachmentItem>) -> Self {
        Self {
            owner_label: owner_label.into(),
            items,
            selected: 0,
            notice: None,
            pending_command: None,
        }
    }

    pub fn from_app(app: &AppState, ui: &UiState) -> Self {
        let (label, items) = app.current_attachments(ui);
        Self::new(label, items)
    }

    pub fn rows(&self) -> Vec<AttachmentRow> {
        self.items
            .iter()
            .enumerate()
            .map(|(idx, item)| AttachmentRow::from_item(idx, item))
            .collect()
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
            self.notice = None;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.notice = None;
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            self.selected = 0;
        } else if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
        self.notice = None;
    }

    pub fn selected_index(&self) -> Option<usize> {
        if self.items.is_empty() {
            None
        } else {
            Some(self.selected.min(self.items.len() - 1))
        }
    }

    pub fn selected_attachment(&self) -> Option<&AttachmentItem> {
        self.selected_index().and_then(|idx| self.items.get(idx))
    }

    pub fn is_selected_image(&self) -> bool {
        self.selected_attachment()
            .is_some_and(|att| img_preview::is_image_attachment(&att.mime_type, &att.filename))
    }

    pub fn is_selected_html(&self) -> bool {
        self.selected_attachment()
            .is_some_and(|att| html_preview::is_html_attachment(&att.mime_type, &att.filename))
    }

    pub fn is_selected_markdown(&self) -> bool {
        self.selected_attachment()
            .is_some_and(|att| html_preview::is_markdown_attachment(&att.mime_type, &att.filename))
    }

    pub fn is_selected_markup(&self) -> bool {
        self.selected_attachment().is_some_and(|att| {
            html_preview::is_previewable_markup_attachment(&att.mime_type, &att.filename)
        })
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp | Action::CursorUp | Action::PrevField => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::NextField => {
                self.move_down();
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Submit => {
                let Some(idx) = self.selected_index() else {
                    self.notice = Some("No attachments yet — press u to upload a file".to_string());
                    return OverlayOutcome::Handled;
                };
                if self.is_selected_image() {
                    self.pending_command = Some(AttachmentCommand::PreviewImage(idx));
                    OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview)
                } else if self.is_selected_markup() {
                    self.pending_command = Some(AttachmentCommand::PreviewHtml(idx));
                    OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
                } else {
                    self.pending_command = Some(AttachmentCommand::Download(idx));
                    OverlayOutcome::Dispatch(Action::DownloadAttachment)
                }
            }
            Action::Input('v') | Action::OpenImagePreview => {
                let Some(idx) = self.selected_index() else {
                    return OverlayOutcome::Handled;
                };
                if self.is_selected_image() {
                    self.pending_command = Some(AttachmentCommand::PreviewImage(idx));
                    OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview)
                } else if self.is_selected_markup() {
                    self.pending_command = Some(AttachmentCommand::PreviewHtml(idx));
                    OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
                } else {
                    self.notice = Some(
                        "Selected file is not a previewable image, HTML, or Markdown file — press d to download"
                            .to_string(),
                    );
                    OverlayOutcome::Handled
                }
            }
            Action::Input('H') | Action::OpenHtmlPreview => {
                let Some(idx) = self.selected_index() else {
                    return OverlayOutcome::Handled;
                };
                if self.is_selected_markup() {
                    self.pending_command = Some(AttachmentCommand::PreviewHtml(idx));
                    OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
                } else {
                    self.notice =
                        Some("Selected attachment is not an HTML or Markdown file".to_string());
                    OverlayOutcome::Handled
                }
            }
            Action::Input('d') | Action::DownloadAttachment => {
                let Some(idx) = self.selected_index() else {
                    return OverlayOutcome::Handled;
                };
                self.pending_command = Some(AttachmentCommand::Download(idx));
                OverlayOutcome::Dispatch(Action::DownloadAttachment)
            }
            Action::Input('u') | Action::UploadAttachment => {
                self.pending_command = Some(AttachmentCommand::Upload);
                OverlayOutcome::CloseAndDispatch(Action::UploadAttachment)
            }
            Action::Input('r') | Action::SyncAll => {
                self.pending_command = Some(AttachmentCommand::Refresh);
                OverlayOutcome::Handled
            }
            Action::Input('o') | Action::OpenInBrowser => {
                if let Some(att) = self.selected_attachment() {
                    if !att.content_url.trim().is_empty() {
                        self.pending_command =
                            Some(AttachmentCommand::OpenUrl(att.content_url.clone()));
                        return OverlayOutcome::Dispatch(Action::OpenInBrowser);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
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
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(78).max(30),
            area.height.saturating_sub(4).min(20).max(9),
        );
        frame.render_widget(Clear, rect);

        let title = if self.owner_label.trim().is_empty() {
            " Attachments ".to_string()
        } else {
            format!(" Attachments — {} ", self.owner_label)
        };
        let block = modal_block(title, theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let rows = self.rows();
        let total = rows.len();
        let header = if total == 0 {
            " No attachments on this item — press u to upload".to_string()
        } else {
            let active = (self.selected + 1).min(total);
            format!(
                " {total} attachment{} · {active}/{total}",
                if total == 1 { "" } else { "s" }
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&header, inner.width as usize),
                theme.muted_style(),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let footer_rows = 2u16.min(inner.height.saturating_sub(1));
        let footer_y = inner.y + inner.height.saturating_sub(footer_rows);
        let list_y = inner.y.saturating_add(2);
        let visible = footer_y.saturating_sub(list_y) as usize;

        if visible > 0 && total > 0 {
            let active_sel = self.selected.min(total - 1);
            let skip = active_sel
                .saturating_sub(visible.saturating_sub(1))
                .min(total.saturating_sub(visible));

            for slot in 0..visible {
                let index = skip + slot;
                if index >= total {
                    break;
                }
                let y = list_y + slot as u16;
                let row = &rows[index];
                let selected = index == active_sel;
                let prefix = if selected { "▸ " } else { "  " };
                let row_style = if selected {
                    theme.selection()
                } else {
                    Style::default().fg(theme.fg)
                };
                let badge_style = if selected {
                    theme.selection()
                } else if row.is_image {
                    Style::default()
                        .fg(theme.confluence_teal)
                        .add_modifier(Modifier::BOLD)
                } else if row.is_html || row.is_markdown {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme.muted_style()
                };

                let size_str = format_size(row.size);
                let meta = match (size_str.is_empty(), row.author_name.as_deref()) {
                    (false, Some(author)) if !author.trim().is_empty() => {
                        format!(" ({size_str} · {author})")
                    }
                    (false, _) => format!(" ({size_str})"),
                    (true, Some(author)) if !author.trim().is_empty() => format!(" ({author})"),
                    _ => String::new(),
                };
                let icon = row.icon();
                let name_budget = (inner.width as usize)
                    .saturating_sub(prefix.len() + icon.len() + 2);
                let body = truncate_width(&format!("{}{}", row.filename, meta), name_budget);

                let line = Line::from(vec![
                    Span::styled(prefix, row_style),
                    Span::styled(icon, badge_style),
                    Span::styled(format!(" {body}"), row_style),
                ]);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem(index));
            }
        }

        if footer_rows >= 2 {
            let info_line = if let Some(notice) = &self.notice {
                Span::styled(
                    truncate_width(&format!(" {notice}"), inner.width as usize),
                    Style::default().fg(theme.warning),
                )
            } else {
                Span::styled(
                    truncate_width(
                        &format!(" Downloads: {}", app.config.resolved_download_dir().display()),
                        inner.width as usize,
                    ),
                    theme.muted_style(),
                )
            };
            frame.render_widget(
                Paragraph::new(info_line),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }

        let hint_y = inner.y + inner.height.saturating_sub(1);
        render_button_bar(
            frame,
            Rect::new(inner.x, hint_y, inner.width, 1),
            &[
                ("[d] Download", ButtonId::DownloadAttachment, modal_pill_primary(theme)),
                ("[u] Upload", ButtonId::UploadAttachment, modal_pill_secondary(theme)),
                ("[o] Browser", ButtonId::OpenBrowser, modal_pill_secondary(theme)),
            ],
            Some("· Enter/v preview · Esc close"),
            theme,
            hitmap,
        );
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
                id: "att-1".into(),
                filename: "screenshot.png".into(),
                mime_type: "image/png".into(),
                size: 2048,
                author_name: Some("Alice".into()),
                created_at: "2026-11-19T00:00:00Z".into(),
                content_url: "https://acme.atlassian.net/att/1".into(),
                thumbnail_url: None,
            },
            AttachmentItem {
                id: "att-2".into(),
                filename: "design-spec.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 2 * 1024 * 1024,
                author_name: Some("Bob".into()),
                created_at: "2026-11-19T01:00:00Z".into(),
                content_url: "https://acme.atlassian.net/att/2".into(),
                thumbnail_url: None,
            },
        ]
    }

    fn test_app() -> (AppState, UiState) {
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
    fn size_formatting_and_row_labels() {
        assert_eq!(format_size(0), "");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0 MB");

        let ov = AttachmentsOverlay::new("ENG-1", sample_attachments());
        let rows = ov.rows();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_image);
        assert_eq!(rows[0].icon(), "[IMG]");
        assert!(!rows[1].is_image);
        assert_eq!(rows[1].icon(), "[FILE]");
        assert!(rows[0].label().contains("screenshot.png"));
    }

    #[test]
    fn attachments_key_handling_preview_download_upload() {
        let mut ov = AttachmentsOverlay::new("ENG-1", sample_attachments());

        // Row 0 is an image: Enter and v open preview
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::PreviewImage(0)));

        assert_eq!(
            ov.handle(&Action::Input('v')),
            OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview)
        );

        // d downloads selected attachment
        assert_eq!(
            ov.handle(&Action::Input('d')),
            OverlayOutcome::Dispatch(Action::DownloadAttachment)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::Download(0)));

        // Move to row 1 (PDF): v sets notice instead of opening image preview; Enter downloads
        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected, 1);
        assert_eq!(ov.handle(&Action::Input('v')), OverlayOutcome::Handled);
        assert!(ov.notice.is_some());
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::DownloadAttachment)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::Download(1)));

        // u opens upload modal
        assert_eq!(
            ov.handle(&Action::Input('u')),
            OverlayOutcome::CloseAndDispatch(Action::UploadAttachment)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::Upload));

        assert_eq!(ov.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[tokio::test]
    async fn attachments_renders_cleanly_in_test_backend() {
        let (app, ui) = test_app();
        let ov = AttachmentsOverlay::new("ENG-1", sample_attachments());
        let mut hitmap = HitMap::new();
        let backend = TestBackend::new(84, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| ov.draw(f, f.area(), &app, &ui, &mut hitmap))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Attachments"));
        assert!(text.contains("ENG-1"));
        assert!(text.contains("screenshot.png"));
        assert!(text.contains("design-spec.pdf"));
        assert!(text.contains("[IMG]"));
        assert!(text.contains("[FILE]"));
        assert!(!hitmap.is_empty());
    }

    #[test]
    fn html_attachment_routes_v_and_enter_to_html_preview_not_image() {
        let mut items = sample_attachments();
        items.push(AttachmentItem {
            id: "att-3".into(),
            filename: "report.html".into(),
            mime_type: "text/html".into(),
            size: 1024,
            author_name: Some("Carol".into()),
            created_at: "2026-11-19T02:00:00Z".into(),
            content_url: "https://acme.atlassian.net/att/3".into(),
            thumbnail_url: None,
        });
        items.push(AttachmentItem {
            id: "att-4".into(),
            filename: "legacy.htm".into(),
            mime_type: "application/octet-stream".into(),
            size: 512,
            author_name: None,
            created_at: "2026-11-19T03:00:00Z".into(),
            content_url: "https://acme.atlassian.net/att/4".into(),
            thumbnail_url: None,
        });

        let mut ov = AttachmentsOverlay::new("ENG-1", items);
        let rows = ov.rows();
        assert!(!rows[2].is_image);
        assert!(rows[2].is_html);
        assert_eq!(rows[2].icon(), "[HTML]");
        assert!(!rows[3].is_image);
        assert!(rows[3].is_html);
        assert_eq!(rows[3].icon(), "[HTML]");

        ov.select(2);
        assert_eq!(
            ov.handle(&Action::Input('v')),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::PreviewHtml(2)));

        ov.select(3);
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::PreviewHtml(3)));
    }

    #[test]
    fn markdown_attachment_shows_md_badge_and_routes_to_preview() {
        let mut items = sample_attachments();
        items.push(AttachmentItem {
            id: "att-md-1".into(),
            filename: "README.md".into(),
            mime_type: "text/markdown".into(),
            size: 1024,
            author_name: Some("Alice".into()),
            created_at: "2026-11-19T04:00:00Z".into(),
            content_url: "https://acme.atlassian.net/att/md1".into(),
            thumbnail_url: None,
        });
        items.push(AttachmentItem {
            id: "att-md-2".into(),
            filename: "NOTES.md".into(),
            mime_type: "application/octet-stream".into(),
            size: 512,
            author_name: None,
            created_at: "2026-11-19T05:00:00Z".into(),
            content_url: "https://acme.atlassian.net/att/md2".into(),
            thumbnail_url: None,
        });

        let mut ov = AttachmentsOverlay::new("ENG-1", items);
        let rows = ov.rows();
        assert!(!rows[2].is_image);
        assert!(!rows[2].is_html);
        assert!(rows[2].is_markdown);
        assert_eq!(rows[2].icon(), "[MD]");
        assert!(rows[3].is_markdown);
        assert_eq!(rows[3].icon(), "[MD]");

        ov.select(2);
        assert_eq!(
            ov.handle(&Action::Input('v')),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::PreviewHtml(2)));
        assert!(ov.notice.is_none());

        ov.select(3);
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlPreview)
        );
        assert_eq!(ov.pending_command, Some(AttachmentCommand::PreviewHtml(3)));
    }
}
