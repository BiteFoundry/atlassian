//! Attachment file path input overlay (`u`).

use std::path::{Path, PathBuf};

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::HitMap,
    keymap::Action,
    overlays::{
        centered_rect, modal_block, read_clipboard_text, sanitize_single_line, OverlayOutcome,
    },
    text_edit::TextBuffer,
};

pub fn expand_path_with_home(input: &str, home: &Path) -> PathBuf {
    let trimmed = input.trim().trim_matches('"');
    if trimmed == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

pub fn expand_path(input: &str) -> PathBuf {
    expand_path_with_home(input, &home_dir())
}

fn home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadTarget {
    Issue(String),
    Page(String),
}

#[derive(Debug, Clone)]
pub struct PathInputOverlay {
    pub target: UploadTarget,
    /// Site of the issue or page the upload goes to.
    pub site_id: Option<String>,
    pub path: TextBuffer,
    pub error: Option<String>,
}

impl PathInputOverlay {
    pub fn new(target: UploadTarget) -> Self {
        Self {
            target,
            site_id: None,
            path: TextBuffer::new(),
            error: None,
        }
    }

    pub fn resolved_path(&self) -> PathBuf {
        expand_path(&self.path.text)
    }

    /// Pastes a file path from the system clipboard (`Ctrl+V`).
    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        self.path.insert_str(&cleaned);
        self.error = None;
        true
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Input(ch) => {
                self.path.insert_char(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.path.backspace();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.path.delete();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.path.move_left();
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.path.move_right();
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.path.move_home();
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.path.move_end();
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                let cleaned = sanitize_single_line(s);
                if !cleaned.is_empty() {
                    self.path.insert_str(&cleaned);
                    self.error = None;
                }
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => {
                let trimmed = self.path.text.trim().trim_matches('"');
                if trimmed.is_empty() {
                    self.error = Some("Please enter a file path".to_string());
                    return OverlayOutcome::Handled;
                }
                let expanded = expand_path(trimmed);
                if !expanded.exists() {
                    self.error = Some(format!("File not found: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                if !expanded.is_file() {
                    self.error = Some(format!("Not a file: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                self.path.set_text(expanded.to_string_lossy().to_string());
                OverlayOutcome::Dispatch(Action::Submit)
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
        let rect = centered_rect(area, 72.min(area.width.saturating_sub(4)).max(30), 8);
        frame.render_widget(Clear, rect);

        let target_label = match &self.target {
            UploadTarget::Issue(k) => format!("Issue {k}"),
            UploadTarget::Page(id) => format!("Page #{id}"),
        };
        let block = modal_block(format!(" Upload Attachment — {target_label} "), theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 4 {
            return;
        }

        let mut lines = vec![
            Line::from(Span::styled(
                " Local file path (~ expands to home directory)",
                theme.muted_style(),
            )),
            Line::from(vec![
                Span::styled(" > ", theme.accent_style()),
                Span::raw(&self.path.text),
                Span::styled("▏", theme.accent_style()),
            ]),
            Line::from(""),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                truncate_width(&format!(" Error: {err}"), inner.width as usize),
                Style::default().fg(theme.error),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                truncate_width(
                    " Enter confirm · Ctrl+V paste · Esc cancel",
                    inner.width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_leading_tilde() {
        let home = PathBuf::from("/home/user");
        assert_eq!(expand_path_with_home("~", &home), home);
        assert_eq!(expand_path_with_home("~/a/b.png", &home), home.join("a/b.png"));
        assert_eq!(
            expand_path_with_home(r"~\a\b.png", &home),
            home.join(r"a\b.png")
        );
        assert_eq!(
            expand_path_with_home(" x/~/y ", &home),
            PathBuf::from("x/~/y")
        );
    }

    #[test]
    fn empty_or_missing_path_shows_error_and_valid_file_submits() {
        let mut ov = PathInputOverlay::new(UploadTarget::Issue("ENG-1".into()));
        assert_eq!(ov.handle(&Action::Submit), OverlayOutcome::Handled);
        assert!(ov.error.is_some());

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path_str = tmp.path().to_string_lossy().to_string();
        ov.handle(&Action::Paste(format!("\"{path_str}\"\n")));
        assert_eq!(
            ov.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit)
        );
        assert_eq!(ov.path.text, path_str);
    }
}
