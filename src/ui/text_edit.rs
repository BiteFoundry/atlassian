//! UTF-8 safe single-line & multiline text buffer + external `$EDITOR` helper.

use std::{path::Path, process::Command};

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

    let status = editor_command(split_editor_command(&editor), &path).status();

    let contents = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);

    let _ = status?;
    Ok(contents?
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_string())
}

/// Splits an `$EDITOR` value into the program and its arguments.
///
/// Double and single quotes group text that contains spaces. A value that is the path of an
/// existing file is the program as a whole, which covers an unquoted `C:\Program Files\...`.
/// A backslash is ordinary text because it separates path components on Windows.
fn split_editor_command(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if Path::new(raw).is_file() {
        return vec![raw.to_string()];
    }
    if raw.starts_with("~/") || raw.starts_with("~\\") {
        let expanded = crate::config::expand_tilde(raw);
        if expanded.is_file() {
            return vec![expanded.to_string_lossy().into_owned()];
        }
    }

    let mut parts = Vec::new();
    let mut current: Option<String> = None;
    let mut rest = raw;
    while let Some(ch) = rest.chars().next() {
        rest = &rest[ch.len_utf8()..];
        if ch.is_whitespace() {
            parts.extend(current.take());
        } else if let Some(end) = matches!(ch, '"' | '\'').then(|| rest.find(ch)).flatten() {
            current
                .get_or_insert_with(String::new)
                .push_str(&rest[..end]);
            rest = &rest[end + 1..];
        } else {
            // Also a quote that is never closed, such as the apostrophe in `O'Brien`.
            current.get_or_insert_with(String::new).push(ch);
        }
    }
    parts.extend(current);
    parts
}

fn expand_editor_program(program: &str) -> String {
    if program == "~" || program.starts_with("~/") || program.starts_with("~\\") {
        crate::config::expand_tilde(program)
            .to_string_lossy()
            .into_owned()
    } else {
        program.to_string()
    }
}

/// Program and arguments that launch the editor for `parts` (the program comes first).
///
/// Windows cannot start a program without an extension (`code`) or a `.cmd`/`.bat` shim
/// directly, so those run through `cmd /C`. The last argument is then a complete, already
/// quoted command line that has to reach `cmd` unchanged.
fn editor_invocation(mut parts: Vec<String>) -> (String, Vec<String>) {
    if let Some(program) = parts.first_mut() {
        *program = expand_editor_program(program);
    }
    #[cfg(windows)]
    if parts.first().is_some_and(|program| needs_cmd_shim(program)) {
        let line = parts
            .iter()
            .map(|part| quote_windows_arg(part))
            .collect::<Vec<_>>()
            .join(" ");
        return (
            "cmd".to_string(),
            vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                format!("\"{line}\""),
            ],
        );
    }
    let mut parts = parts.into_iter();
    let program = parts.next().unwrap_or_else(|| "vi".to_string());
    (program, parts.collect())
}

#[cfg(windows)]
fn needs_cmd_shim(program: &str) -> bool {
    match Path::new(program).extension() {
        None => true,
        Some(extension) => {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        }
    }
}

/// Quotes one argument the way Windows programs parse their command line.
#[cfg(windows)]
fn quote_windows_arg(arg: &str) -> String {
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, and the quote itself is escaped.
                quoted.extend(std::iter::repeat('\\').take(backslashes + 1));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        quoted.push(ch);
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes));
    quoted.push('"');
    quoted
}

fn editor_command(mut parts: Vec<String>, file: &Path) -> Command {
    if let Some(program) = parts.first_mut() {
        *program = expand_editor_program(program);
    }
    #[cfg(windows)]
    if parts.first().is_some_and(|program| needs_cmd_shim(program)) {
        use std::os::windows::process::CommandExt;

        parts.push(file.to_string_lossy().into_owned());
        let (program, args) = editor_invocation(parts);
        let mut command = Command::new(program);
        for arg in args {
            command.raw_arg(arg);
        }
        return command;
    }
    let (program, args) = editor_invocation(parts);
    let mut command = Command::new(program);
    command.args(args).arg(file);
    command
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

    #[test]
    fn split_editor_command_handles_quoted_program_with_spaces() {
        assert_eq!(
            split_editor_command(r#""C:\Program Files\Notepad++\notepad++.exe" -multiInst"#),
            [r"C:\Program Files\Notepad++\notepad++.exe", "-multiInst"]
        );
        assert_eq!(
            split_editor_command("'/opt/my editor/bin/edit' --wait"),
            ["/opt/my editor/bin/edit", "--wait"]
        );
        assert_eq!(
            split_editor_command(r#"vim -c "set ft=markdown""#),
            ["vim", "-c", "set ft=markdown"]
        );
        // A quote that is never closed is ordinary text.
        assert_eq!(
            split_editor_command(r"C:\Users\O'Brien\vim.exe -f"),
            [r"C:\Users\O'Brien\vim.exe", "-f"]
        );
    }

    #[test]
    fn split_editor_command_keeps_unquoted_existing_path_whole() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Program Files");
        std::fs::create_dir(&folder).unwrap();
        let program = folder.join("my editor.exe");
        std::fs::write(&program, b"").unwrap();
        let raw = program.to_str().unwrap();

        assert_eq!(split_editor_command(raw), [raw]);
        assert_eq!(split_editor_command(&format!("  {raw}  ")), [raw]);
    }

    #[test]
    fn split_editor_command_plain_unix_editor_with_args() {
        assert_eq!(split_editor_command("code --wait"), ["code", "--wait"]);
        assert_eq!(split_editor_command("open -W -t"), ["open", "-W", "-t"]);
        assert_eq!(split_editor_command("  vi  "), ["vi"]);
        assert!(split_editor_command("   ").is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_editor_cmd_shim_goes_through_cmd_c() {
        let parts = |program: &str| {
            vec![
                program.to_string(),
                "--wait".to_string(),
                r"C:\Temp\my notes.md".to_string(),
            ]
        };
        let through_cmd = |program: &str| {
            (
                "cmd".to_string(),
                vec![
                    "/D".to_string(),
                    "/S".to_string(),
                    "/C".to_string(),
                    format!(r#"""{program}" "--wait" "C:\Temp\my notes.md"""#),
                ],
            )
        };

        // `code` resolves to `code.cmd`, which only `cmd` can run.
        assert_eq!(editor_invocation(parts("code")), through_cmd("code"));
        assert_eq!(
            editor_invocation(parts(r"C:\Tools\My Editor\edit.CMD")),
            through_cmd(r"C:\Tools\My Editor\edit.CMD")
        );
        assert_eq!(editor_invocation(parts("edit.bat")), through_cmd("edit.bat"));

        // A real executable is started directly, as before.
        assert_eq!(
            editor_invocation(parts(r"C:\Program Files\Notepad++\notepad++.exe")),
            (
                r"C:\Program Files\Notepad++\notepad++.exe".to_string(),
                vec!["--wait".to_string(), r"C:\Temp\my notes.md".to_string()]
            )
        );

        assert_eq!(quote_windows_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_windows_arg("C:\\dir\\"), "\"C:\\dir\\\\\"");
    }

    #[cfg(windows)]
    #[test]
    fn windows_editor_cmd_shim_runs_with_spaces_in_paths() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("R&D tools");
        std::fs::create_dir(&folder).unwrap();
        // The shim writes its first argument into the file named by its second.
        std::fs::write(folder.join("fake editor.cmd"), "@echo off\r\necho %~1>%2\r\n").unwrap();
        let file = folder.join("my notes.md");

        // With the extension, and without it the way `code` is usually written.
        for program in ["fake editor.cmd", "fake editor"] {
            let _ = std::fs::remove_file(&file);
            let parts = vec![
                folder.join(program).to_str().unwrap().to_string(),
                "--wait".to_string(),
            ];
            let output = editor_command(parts, &file).output().unwrap();
            assert!(output.status.success(), "{program}: {output:?}");
            assert_eq!(
                std::fs::read_to_string(&file).unwrap().trim(),
                "--wait",
                "{program}"
            );
        }
    }

    #[test]
    fn editor_command_expands_tilde_in_program_only() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let expected = home.join("bin/my-editor.exe");
        let cmd = editor_command(
            vec![
                "~/bin/my-editor.exe".to_string(),
                "--config".to_string(),
                "~/keep-arg".to_string(),
            ],
            Path::new("draft.md"),
        );
        assert_eq!(Path::new(cmd.get_program()), expected.as_path());
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--config", "~/keep-arg", "draft.md"]);

        // Non-tilde programs stay untouched.
        assert_eq!(expand_editor_program("vim"), "vim");
        assert_eq!(expand_editor_program("~other/bin/vi"), "~other/bin/vi");
    }
}
