//! UTF-8 safe single-line & multiline text buffer + external `$EDITOR` helper.

use std::process::Command;

use anyhow::Result;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextBuffer {
    pub text: String,
    /// Byte offset (always aligned to UTF-8 char boundary).
    pub cursor: usize,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        let t = text.into();
        let len = t.len();
        Self {
            text: t,
            cursor: len,
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\r' {
            return;
        }
        self.clamp_cursor();
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        self.clamp_cursor();
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
    }

    pub fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
    }

    pub fn move_left(&mut self) {
        self.clamp_cursor();
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        self.clamp_cursor();
        if self.cursor < self.text.len() {
            self.cursor = next_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.clamp_cursor();
        let prefix = &self.text[..self.cursor];
        self.cursor = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.clamp_cursor();
        if let Some(rel) = self.text[self.cursor..].find('\n') {
            self.cursor += rel;
        } else {
            self.cursor = self.text.len();
        }
    }

    pub fn move_up(&mut self) {
        self.clamp_cursor();
        let before = &self.text[..self.cursor];
        let Some(cur_nl) = before.rfind('\n') else {
            self.cursor = 0;
            return;
        };
        let col_chars = before[cur_nl + 1..].chars().count();
        let prev_start = before[..cur_nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let prev_line = &self.text[prev_start..cur_nl];
        let offset = prev_line
            .char_indices()
            .nth(col_chars)
            .map(|(i, _)| i)
            .unwrap_or(prev_line.len());
        self.cursor = prev_start + offset;
    }

    pub fn move_down(&mut self) {
        self.clamp_cursor();
        let Some(rel_nl) = self.text[self.cursor..].find('\n') else {
            self.cursor = self.text.len();
            return;
        };
        let cur_line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col_chars = self.text[cur_line_start..self.cursor].chars().count();
        let next_start = self.cursor + rel_nl + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map(|r| next_start + r)
            .unwrap_or(self.text.len());
        let next_line = &self.text[next_start..next_end];
        let offset = next_line
            .char_indices()
            .nth(col_chars)
            .map(|(i, _)| i)
            .unwrap_or(next_line.len());
        self.cursor = next_start + offset;
    }

    /// Returns `(line_idx, col_display_width)` of the cursor for rendering.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        use unicode_width::UnicodeWidthStr;
        let cur = self.cursor.min(self.text.len());
        let before = &self.text[..cur];
        let line = before.matches('\n').count();
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = before[line_start..].width();
        (line, col)
    }

    fn clamp_cursor(&mut self) {
        if self.cursor > self.text.len() {
            self.cursor = self.text.len();
        }
        while self.cursor > 0 && !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Launches `$EDITOR` / `$VISUAL` (or `notepad` on Windows, `vi` on Unix) with a temporary Markdown file.
pub fn external_edit(current: &str) -> Result<String> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "atlassian-tui-edit-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));

    std::fs::write(&path, current)?;

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map(|v| v.trim().to_string())
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default_editor);

    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = Command::new(program).args(parts).arg(&path).status();

    let contents = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);

    let _ = status?;
    Ok(contents?
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_string())
}

fn default_editor() -> String {
    if cfg!(windows) {
        "notepad".to_string()
    } else if cfg!(target_os = "macos") {
        "open -W -t".to_string()
    } else {
        "vi".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_editing_and_multiline_navigation() {
        let mut buf = TextBuffer::new();
        buf.insert_str("가나\n라a");
        assert_eq!(buf.cursor_line_col(), (1, 3));
        buf.backspace();
        assert_eq!(buf.text, "가나\n라");
        buf.move_up();
        assert_eq!(buf.cursor_line_col().0, 0);
        buf.move_home();
        assert_eq!(buf.cursor, 0);
        buf.move_end();
        assert_eq!(buf.cursor, "가나".len());
    }
}
