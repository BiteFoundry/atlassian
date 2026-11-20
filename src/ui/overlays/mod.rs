//! Overlay stack, HelpOverlay (`?`), shared modal chrome, clipboard helpers, and modal dispatch.

pub mod assignee;
pub mod attachments;
pub mod command_palette;
pub mod comment;
pub mod confirm;
pub mod context_menu;
pub mod cross_link;
pub mod file_picker;
pub mod html_preview;
pub mod image_preview;
pub mod issue_form;
pub mod links;
pub mod page_form;
pub mod path_input;
pub mod settings;
pub mod site_switcher;
pub mod transition;

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::HitMap,
    keymap::Action,
    overlays::{
        assignee::AssigneeOverlay, attachments::AttachmentsOverlay,
        command_palette::CommandPalette, comment::CommentOverlay, confirm::ConfirmDialog,
        context_menu::ContextMenuOverlay, cross_link::CrossLinkOverlay,
        file_picker::FilePickerOverlay, html_preview::HtmlPreviewOverlay,
        image_preview::ImagePreviewOverlay,
        issue_form::IssueFormOverlay, links::LinksOverlay, page_form::PageFormOverlay,
        path_input::PathInputOverlay, settings::SettingsOverlay,
        site_switcher::SiteSwitcherOverlay, transition::TransitionOverlay,
    },
    theme::Theme,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Transition,
    Assignee,
    IssueForm,
    PageForm,
    Comment,
    CrossLink,
    Links,
    ImagePreview,
    HtmlPreview,
    PathInput,
    FilePicker,
    CommandPalette,
    SiteSwitcher,
    Settings,
    ContextMenu,
    Attachments,
    Help,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
}

#[derive(Debug)]
pub enum Overlay {
    Transition(TransitionOverlay),
    Assignee(AssigneeOverlay),
    IssueForm(IssueFormOverlay),
    PageForm(PageFormOverlay),
    Comment(CommentOverlay),
    CrossLink(CrossLinkOverlay),
    Links(LinksOverlay),
    ImagePreview(ImagePreviewOverlay),
    HtmlPreview(HtmlPreviewOverlay),
    PathInput(PathInputOverlay),
    FilePicker(FilePickerOverlay),
    CommandPalette(CommandPalette),
    SiteSwitcher(SiteSwitcherOverlay),
    Settings(SettingsOverlay),
    ContextMenu(ContextMenuOverlay),
    Attachments(AttachmentsOverlay),
    Help,
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::Transition(_) => OverlayKind::Transition,
            Overlay::Assignee(_) => OverlayKind::Assignee,
            Overlay::IssueForm(_) => OverlayKind::IssueForm,
            Overlay::PageForm(_) => OverlayKind::PageForm,
            Overlay::Comment(_) => OverlayKind::Comment,
            Overlay::CrossLink(_) => OverlayKind::CrossLink,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::HtmlPreview(_) => OverlayKind::HtmlPreview,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::SiteSwitcher(_) => OverlayKind::SiteSwitcher,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::Help => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsOverlay::new())
    }

    pub fn context_menu(menu: ContextMenuOverlay) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn attachments(overlay: AttachmentsOverlay) -> Self {
        Overlay::Attachments(overlay)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn site_switcher(overlay: SiteSwitcherOverlay) -> Self {
        Overlay::SiteSwitcher(overlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn help() -> Self {
        Overlay::Help
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::HtmlPreview(o) => o.is_editing_input(),
            Overlay::Assignee(_)
            | Overlay::IssueForm(_)
            | Overlay::PageForm(_)
            | Overlay::Comment(_)
            | Overlay::PathInput(_)
            | Overlay::FilePicker(_)
            | Overlay::CommandPalette(_)
            | Overlay::SiteSwitcher(_) => true,
            _ => false,
        }
    }

    /// Returns the `DraftStore` key for overlays that support local draft auto-saving.
    pub fn draft_key(&self) -> Option<String> {
        match self {
            Overlay::IssueForm(o) => Some(o.draft_key()),
            Overlay::PageForm(o) => Some(o.draft_key()),
            Overlay::Comment(o) => Some(o.draft_key()),
            _ => None,
        }
    }

    /// Pastes text from the system clipboard (`Ctrl+V`) into the active text field if supported.
    pub fn paste_from_clipboard(&mut self) -> bool {
        match self {
            Overlay::Assignee(o) => o.paste_from_clipboard(),
            Overlay::IssueForm(o) => o.paste_from_clipboard(),
            Overlay::PageForm(o) => o.paste_from_clipboard(),
            Overlay::Comment(o) => o.paste_from_clipboard(),
            Overlay::HtmlPreview(o) => o.paste_from_clipboard(),
            Overlay::PathInput(o) => o.paste_from_clipboard(),
            Overlay::FilePicker(o) => o.paste_from_clipboard(),
            Overlay::CommandPalette(o) => o.paste_from_clipboard(),
            Overlay::SiteSwitcher(o) => o.paste_from_clipboard(),
            _ => false,
        }
    }

    /// Selects a list item by index (e.g. from mouse hitmap clicks).
    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::Transition(o) => o.select(index),
            Overlay::Assignee(o) => o.select(index),
            Overlay::CrossLink(o) => o.select(index),
            Overlay::Links(o) => o.select(index),
            Overlay::FilePicker(o) => o.select(index),
            Overlay::CommandPalette(o) => o.select(index),
            Overlay::SiteSwitcher(o) => o.select(index),
            Overlay::Settings(o) => o.select(index),
            Overlay::ContextMenu(o) => o.select(index),
            Overlay::Attachments(o) => o.select(index),
            Overlay::IssueForm(_)
            | Overlay::PageForm(_)
            | Overlay::Comment(_)
            | Overlay::ImagePreview(_)
            | Overlay::HtmlPreview(_)
            | Overlay::PathInput(_)
            | Overlay::Help
            | Overlay::Confirm(_) => {}
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match self {
            Overlay::Transition(o) => o.handle(action),
            Overlay::Assignee(o) => o.handle(action),
            Overlay::IssueForm(o) => o.handle(action),
            Overlay::PageForm(o) => o.handle(action),
            Overlay::Comment(o) => o.handle(action),
            Overlay::CrossLink(o) => o.handle(action),
            Overlay::Links(o) => o.handle(action),
            Overlay::ImagePreview(o) => o.handle(action),
            Overlay::HtmlPreview(o) => o.handle(action),
            Overlay::PathInput(o) => o.handle(action),
            Overlay::FilePicker(o) => o.handle(action),
            Overlay::CommandPalette(o) => o.handle(action),
            Overlay::SiteSwitcher(o) => o.handle(action),
            Overlay::Settings(o) => o.handle(action),
            Overlay::ContextMenu(o) => o.handle(action),
            Overlay::Attachments(o) => o.handle(action),
            Overlay::Confirm(o) => o.handle(action),
            Overlay::Help => match action {
                Action::CloseOverlay | Action::OpenHelp | Action::Input('q') | Action::Activate => {
                    OverlayOutcome::Close
                }
                _ => OverlayOutcome::Handled,
            },
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
        match self {
            Overlay::Transition(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Assignee(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::IssueForm(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::PageForm(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Comment(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::CrossLink(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Links(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::ImagePreview(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::HtmlPreview(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::FilePicker(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::CommandPalette(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::SiteSwitcher(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Settings(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::ContextMenu(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Attachments(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Confirm(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Help => draw_help_overlay(frame, area, ui, hitmap),
        }
    }
}

/// Shared elevated rounded modal chrome aligned with `teams` and `outlook`.
pub fn modal_block<'a>(title: impl Into<String>, theme: &Theme, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(theme.card_bg).fg(theme.fg))
        .title(Line::from(Span::styled(title.into(), theme.title(focused))))
        .border_style(theme.border_style(focused))
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width).max(1);
    let h = height.min(area.height).max(1);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Positions a popup rectangle at `(col, row)` while clamping it inside `area`.
pub fn anchored_rect(area: Rect, anchor: (u16, u16), width: u16, height: u16) -> Rect {
    let w = width.min(area.width).max(1);
    let h = height.min(area.height).max(1);
    let max_x = area.x + area.width.saturating_sub(w);
    let max_y = area.y + area.height.saturating_sub(h);
    let x = anchor.0.max(area.x).min(max_x);
    let y = anchor.1.max(area.y).min(max_y);
    Rect::new(x, y, w, h)
}

/// Reads plain text from the OS clipboard for `Ctrl+V` paste in text forms.
pub fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let text = clipboard.get_text().ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Encodes raw RGBA pixels into a PNG file at `<dir>/atlassian-pasted-<unix-ms>.png`.
pub fn encode_rgba_to_temp_png(
    width: usize,
    height: usize,
    rgba: &[u8],
    dir: &Path,
) -> Result<PathBuf, String> {
    if width == 0 || height == 0 {
        return Err("Clipboard image has 0x0 dimensions".to_string());
    }
    let expected = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "Clipboard image dimensions overflowed".to_string())?;
    if rgba.len() < expected {
        return Err(format!(
            "Clipboard image buffer too short ({} < {expected})",
            rgba.len()
        ));
    }
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba[..expected].to_vec())
        .ok_or_else(|| "Failed to build RGBA image buffer".to_string())?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("atlassian-pasted-{unix_ms}.png"));
    image::DynamicImage::ImageRgba8(img)
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode pasted PNG: {e}"))?;
    Ok(path)
}

/// Checks if the OS clipboard holds an image (`arboard::Clipboard::get_image()`), and if so
/// saves it as `<temp_dir>/atlassian-pasted-<unix-ms>.png`. Returns `None` if clipboard has no image.
pub fn read_clipboard_image_to_temp() -> Option<Result<PathBuf, String>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    if img.width == 0 || img.height == 0 || img.bytes.is_empty() {
        return None;
    }
    Some(encode_rgba_to_temp_png(
        img.width,
        img.height,
        img.bytes.as_ref(),
        &std::env::temp_dir(),
    ))
}

/// Copies plain text to the OS clipboard (used by context menu Copy key / Copy URL).
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("Clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("Failed to copy to clipboard: {e}"))
}

/// Collapses CR/LF characters for single-line input fields on paste.
pub fn sanitize_single_line(text: &str) -> String {
    text.replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Global & Workspace Tabs",
        &[
            ("1..6 / Ctrl+1..6", "Switch Board / Issues / My Work / Wiki / Search / Dir"),
            ("Tab / Shift+Tab", "Cycle focus Nav <-> Main <-> Detail drawer"),
            ("Ctrl+P / :", "Open Command Palette"),
            ("Ctrl+S", "Switch site / All Sites federation"),
            ("Ctrl+,", "Open Settings (theme, density, protocol)"),
            ("g / Backspace", "Jump to cross-linked Jira/Confluence / nav back"),
            ("r / U / ?", "Sync all / check updates / keyboard shortcuts help"),
        ],
    ),
    (
        "Jira Board & Issues",
        &[
            ("h/l, j/k, Enter", "Move columns/cards and open detail drawer"),
            ("m / i / a", "Transition status / Assign to me / Change assignee"),
            ("n / e / c", "New issue / Edit issue ($EDITOR) / Add comment"),
            ("J / /", "Focus JQL query bar / Local quick filter"),
            ("Right-click", "Open context menu for issue or page actions"),
        ],
    ),
    (
        "Confluence Wiki & Media",
        &[
            ("Space / → / ←", "Expand (lazy-load) or collapse page tree node"),
            ("t / e / n / c", "Toggle TOC / Edit page / New child page / Comment"),
            ("A / v / H / h", "Attachments / Images / HTML Preview / Inline hrefs"),
            ("u / Ctrl+F / d / o", "Link extractor / File picker / Download / Browser"),
            ("p", "Pin or unpin project / space in Directory"),
        ],
    ),
    (
        "Forms & Editing",
        &[
            ("Tab / Shift+Tab", "Move between form fields"),
            ("Shift+Enter", "Insert newline in description / comment body"),
            ("Ctrl+V", "Paste text or attach clipboard image"),
            ("Ctrl+E", "Open active Markdown body in external $EDITOR"),
            ("Ctrl+S / Enter", "Confirm and submit form"),
            ("Esc / q", "Cancel modal / Quit Atlassian TUI"),
        ],
    ),
];

fn draw_help_overlay(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let wide = area.width >= 92;
    let rect = centered_rect(
        area,
        if wide {
            area.width.saturating_sub(4).min(102)
        } else {
            area.width.saturating_sub(4).min(76).max(32)
        },
        area.height.saturating_sub(2).min(if wide { 22 } else { 28 }).max(12),
    );
    frame.render_widget(Clear, rect);
    hitmap.add(rect, crate::ui::hitmap::HitTarget::OverlayBody);

    let block = modal_block(" Atlassian TUI Keyboard Shortcuts (?) ", theme, true);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let body_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);

    if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(body_area);
        let half_h = (body_area.height / 2).max(1);

        for (idx, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
            let target = cols[idx % 2];
            if target.width == 0 {
                continue;
            }
            let y_off = if idx < 2 { 0 } else { half_h };
            let sub = Rect::new(
                target.x,
                target.y + y_off,
                target.width,
                target.height.saturating_sub(y_off),
            );
            let mut lines = Vec::with_capacity(entries.len() + 1);
            lines.push(Line::from(Span::styled(
                format!(" {title}"),
                Style::default()
                    .fg(section_color(idx, theme))
                    .add_modifier(Modifier::BOLD),
            )));
            for (k, desc) in *entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {k:<18}"), theme.accent_style()),
                    Span::raw(desc.to_string()),
                ]));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sub);
        }
    } else {
        let mut lines = Vec::new();
        for (idx, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!(" [{title}]"),
                Style::default()
                    .fg(section_color(idx, theme))
                    .add_modifier(Modifier::BOLD),
            )));
            for (k, desc) in *entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {k:<18}"), theme.accent_style()),
                    Span::raw(desc.to_string()),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
    }

    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(" Enter / Esc / ? close help", footer_area.width as usize),
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        )),
        footer_area,
    );
}

fn section_color(idx: usize, theme: &Theme) -> ratatui::style::Color {
    match idx {
        0 => theme.confluence_teal,
        1 => theme.accent,
        2 => theme.success,
        _ => theme.warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_rect_clamps_within_viewport() {
        let area = Rect::new(0, 0, 80, 24);
        let r = anchored_rect(area, (75, 22), 30, 10);
        assert_eq!(r.x, 50);
        assert_eq!(r.y, 14);
        assert_eq!(r.width, 30);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn overlay_kind_and_draft_keys_map_correctly() {
        let settings = Overlay::settings();
        assert_eq!(settings.kind(), OverlayKind::Settings);
        assert!(!settings.accepts_text());
        assert_eq!(settings.draft_key(), None);

        let comment = Overlay::Comment(CommentOverlay::new(
            comment::CommentTarget::Issue("ENG-12".into()),
        ));
        assert_eq!(comment.kind(), OverlayKind::Comment);
        assert!(comment.accepts_text());
        assert_eq!(comment.draft_key().as_deref(), Some("comment:issue:ENG-12"));

        let att = Overlay::attachments(AttachmentsOverlay::new("ENG-12", vec![]));
        assert_eq!(att.kind(), OverlayKind::Attachments);
        assert!(!att.accepts_text());

        let ctx = Overlay::context_menu(ContextMenuOverlay::for_global((0, 0)));
        assert_eq!(ctx.kind(), OverlayKind::ContextMenu);
        assert!(!ctx.accepts_text());

        let switcher = Overlay::site_switcher(SiteSwitcherOverlay::new(vec![]));
        assert_eq!(switcher.kind(), OverlayKind::SiteSwitcher);
        assert!(switcher.accepts_text());
        assert_eq!(switcher.draft_key(), None);
    }

    #[test]
    fn sanitize_single_line_strips_newlines() {
        assert_eq!(sanitize_single_line("  foo\r\nbar\nbaz  "), "foo bar baz");
    }

    #[test]
    fn encode_rgba_to_temp_png_creates_valid_png_with_expected_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = vec![255u8, 0, 128, 255, 0, 255, 64, 255, 32, 64, 96, 255, 10, 20, 30, 255];
        let path = encode_rgba_to_temp_png(2, 2, &rgba, dir.path()).unwrap();
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(fname.starts_with("atlassian-pasted-"));
        assert!(fname.ends_with(".png"));
        let decoded = image::open(&path).unwrap();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);

        assert!(encode_rgba_to_temp_png(0, 2, &rgba, dir.path()).is_err());
        assert!(encode_rgba_to_temp_png(2, 2, &[0u8; 4], dir.path()).is_err());
    }
}
