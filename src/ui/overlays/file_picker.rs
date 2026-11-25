//! Interactive local File Picker modal overlay (`Ctrl+F`) with type-to-filter
//! and optional `zoxide` directory jump (`z` via `zoxide query -l`).

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::{ButtonId, HitMap, HitTarget},
    keymap::Action,
    overlays::{
        centered_rect, modal_block, modal_pill_secondary, path_input::UploadTarget,
        read_clipboard_text, register_modal_chrome, render_button_bar, sanitize_single_line,
        OverlayOutcome,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePickerMode {
    Browse,
    Zoxide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub is_parent: bool,
}

#[derive(Debug, Clone)]
pub struct FilePickerOverlay {
    pub target: UploadTarget,
    /// Site of the issue or page the upload goes to.
    pub site_id: Option<String>,
    pub current_dir: PathBuf,
    pub entries: Vec<FilePickerEntry>,
    pub filter: String,
    pub selected: usize,
    pub mode: FilePickerMode,
    pub zoxide_dirs: Vec<PathBuf>,
    pub status_msg: Option<String>,
    pub selected_file: Option<PathBuf>,
}

impl FilePickerOverlay {
    pub fn new(target: UploadTarget) -> Self {
        let start_dir = std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::new_in_dir(target, start_dir)
    }

    pub fn new_in_dir(target: UploadTarget, dir: PathBuf) -> Self {
        let mut ov = Self {
            target,
            site_id: None,
            current_dir: dir,
            entries: Vec::new(),
            filter: String::new(),
            selected: 0,
            mode: FilePickerMode::Browse,
            zoxide_dirs: Vec::new(),
            status_msg: None,
            selected_file: None,
        };
        ov.reload_entries();
        ov
    }

    pub fn reload_entries(&mut self) {
        let mut items = Vec::new();
        if let Some(parent) = self.current_dir.parent() {
            items.push(FilePickerEntry {
                name: "..".to_string(),
                path: parent.to_path_buf(),
                is_dir: true,
                size: 0,
                is_parent: true,
            });
        }

        match fs::read_dir(&self.current_dir) {
            Ok(read_dir) => {
                let mut dirs_vec = Vec::new();
                let mut files_vec = Vec::new();
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    let meta = entry.metadata().ok();
                    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                    let size = meta.as_ref().map_or(0, |m| if m.is_file() { m.len() } else { 0 });
                    let item = FilePickerEntry {
                        name,
                        path,
                        is_dir,
                        size,
                        is_parent: false,
                    };
                    if is_dir {
                        dirs_vec.push(item);
                    } else {
                        files_vec.push(item);
                    }
                }
                dirs_vec.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
                files_vec
                    .sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
                items.extend(dirs_vec);
                items.extend(files_vec);
                self.status_msg = None;
            }
            Err(err) => {
                self.status_msg = Some(format!("Cannot read directory: {err}"));
            }
        }

        self.entries = items;
        self.selected = 0;
    }

    pub fn navigate_to(&mut self, dir: PathBuf) {
        if dir.is_dir() {
            self.current_dir = dir;
            self.filter.clear();
            self.mode = FilePickerMode::Browse;
            self.reload_entries();
        } else {
            self.status_msg = Some(format!("Directory does not exist: {}", dir.display()));
        }
    }

    pub fn go_parent(&mut self) {
        if let Some(parent) = self.current_dir.parent().map(Path::to_path_buf) {
            self.navigate_to(parent);
        }
    }

    pub fn enter_zoxide_with(&mut self, dirs: Vec<PathBuf>) {
        if dirs.is_empty() {
            self.status_msg = Some("No zoxide directories available".to_string());
            return;
        }
        self.zoxide_dirs = dirs;
        self.mode = FilePickerMode::Zoxide;
        self.filter.clear();
        self.selected = 0;
        self.status_msg = None;
    }

    pub fn visible_entries(&self) -> Vec<&FilePickerEntry> {
        let q = self.filter.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.entries.iter().collect();
        }
        self.entries
            .iter()
            .filter(|e| !e.is_parent && e.name.to_ascii_lowercase().contains(&q))
            .collect()
    }

    pub fn visible_zoxide_dirs(&self) -> Vec<&PathBuf> {
        let q = self.filter.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.zoxide_dirs.iter().collect();
        }
        self.zoxide_dirs
            .iter()
            .filter(|p| p.to_string_lossy().to_ascii_lowercase().contains(&q))
            .collect()
    }

    fn visible_len(&self) -> usize {
        match self.mode {
            FilePickerMode::Browse => self.visible_entries().len(),
            FilePickerMode::Zoxide => self.visible_zoxide_dirs().len(),
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.visible_len() {
            self.selected = index;
        }
    }

    /// Returns `true` when the currently selected entry is a directory (or zoxide jump target).
    pub fn is_selected_directory(&self) -> bool {
        match self.mode {
            FilePickerMode::Zoxide => !self.visible_zoxide_dirs().is_empty(),
            FilePickerMode::Browse => self
                .visible_entries()
                .get(self.selected)
                .is_some_and(|entry| entry.is_dir),
        }
    }

    pub fn paste_from_clipboard(&mut self) -> bool {
        let Some(text) = read_clipboard_text() else {
            return false;
        };
        let cleaned = sanitize_single_line(&text);
        if cleaned.is_empty() {
            return false;
        }
        let candidate = PathBuf::from(&cleaned);
        if candidate.is_dir() {
            self.navigate_to(candidate);
            return true;
        }
        self.filter.push_str(&cleaned);
        self.selected = 0;
        true
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => {
                if self.mode == FilePickerMode::Zoxide {
                    self.mode = FilePickerMode::Browse;
                    self.filter.clear();
                    self.selected = 0;
                    self.status_msg = None;
                    OverlayOutcome::Handled
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.selected = 0;
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::CursorUp | Action::MoveUp | Action::PrevField => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::CursorDown | Action::MoveDown | Action::NextField => {
                let len = self.visible_len();
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                let len = self.visible_len();
                if len > 0 {
                    self.selected = (self.selected + 8).min(len - 1);
                }
                OverlayOutcome::Handled
            }
            Action::CollapseNode => {
                if self.mode == FilePickerMode::Zoxide {
                    self.mode = FilePickerMode::Browse;
                    self.filter.clear();
                    self.selected = 0;
                    self.status_msg = None;
                } else {
                    self.go_parent();
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                if !self.filter.is_empty() {
                    self.filter.pop();
                    self.selected = 0;
                    self.status_msg = None;
                } else if self.mode == FilePickerMode::Zoxide {
                    self.mode = FilePickerMode::Browse;
                    self.selected = 0;
                    self.status_msg = None;
                } else {
                    self.go_parent();
                }
                OverlayOutcome::Handled
            }
            Action::Input('z') if self.mode == FilePickerMode::Browse && self.filter.is_empty() => {
                match query_zoxide_dirs() {
                    Ok(dirs) if !dirs.is_empty() => self.enter_zoxide_with(dirs),
                    Ok(_) => {
                        self.status_msg = Some("zoxide returned no directories".to_string());
                    }
                    Err(err) => {
                        self.status_msg = Some(err);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.filter.push(*ch);
                self.selected = 0;
                self.status_msg = None;
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                let cleaned = sanitize_single_line(s);
                if !cleaned.is_empty() {
                    self.filter.push_str(&cleaned);
                    self.selected = 0;
                    self.status_msg = None;
                }
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => match self.mode {
                FilePickerMode::Zoxide => {
                    let chosen = self.visible_zoxide_dirs().get(self.selected).cloned().cloned();
                    if let Some(dir) = chosen {
                        self.navigate_to(dir);
                    }
                    OverlayOutcome::Handled
                }
                FilePickerMode::Browse => {
                    let chosen = self.visible_entries().get(self.selected).cloned().cloned();
                    let Some(entry) = chosen else {
                        return OverlayOutcome::Handled;
                    };
                    if entry.is_dir {
                        self.navigate_to(entry.path);
                        OverlayOutcome::Handled
                    } else if entry.path.is_file() {
                        self.selected_file = Some(entry.path);
                        OverlayOutcome::Dispatch(Action::Submit)
                    } else {
                        self.status_msg = Some(format!("Not a readable file: {}", entry.name));
                        OverlayOutcome::Handled
                    }
                }
            },
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            84.min(area.width.saturating_sub(4)).max(36),
            22.min(area.height.saturating_sub(2)).max(10),
        );
        frame.render_widget(Clear, rect);

        let target_label = match &self.target {
            UploadTarget::Issue(k) => format!("Issue {k}"),
            UploadTarget::Page(id) => format!("Page #{id}"),
        };
        let mode_tag = match self.mode {
            FilePickerMode::Browse => "File Picker",
            FilePickerMode::Zoxide => "zoxide Jump",
        };
        let block = modal_block(format!(" {mode_tag} — Upload to {target_label} "), theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 5 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let dir_str = self.current_dir.to_string_lossy();
        let filter_label = match self.mode {
            FilePickerMode::Browse => " Filter: ",
            FilePickerMode::Zoxide => " zoxide: ",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" Dir: ", theme.muted_style()),
                    Span::styled(
                        truncate_width(&dir_str, chunks[0].width.saturating_sub(6) as usize),
                        Style::default().fg(theme.confluence_teal),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(filter_label, theme.accent_style()),
                    Span::raw(&self.filter),
                    Span::styled("▏", theme.accent_style()),
                ]),
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let max_rows = chunks[1].height as usize;
        let mut lines = Vec::new();
        let w = chunks[1].width as usize;

        match self.mode {
            FilePickerMode::Browse => {
                let items = self.visible_entries();
                if items.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  No matching files or directories",
                        theme.muted_style(),
                    )));
                } else {
                    let sel = self.selected.min(items.len().saturating_sub(1));
                    let start = if sel >= max_rows && max_rows > 0 {
                        sel - max_rows + 1
                    } else {
                        0
                    };
                    for (vis_idx, entry) in items.iter().skip(start).take(max_rows).enumerate() {
                        let abs_idx = start + vis_idx;
                        let is_sel = abs_idx == sel;
                        let row_style = if is_sel {
                            theme.selection()
                        } else {
                            Style::default().fg(theme.fg)
                        };
                        let prefix = if is_sel { "▸ " } else { "  " };
                        let tag = if entry.is_dir { "[DIR] " } else { "      " };
                        let size_str = if entry.is_dir {
                            String::new()
                        } else {
                            format!(" {}", format_size(entry.size))
                        };
                        let name_budget = w.saturating_sub(prefix.len() + tag.len() + size_str.len() + 1);
                        let display_name = if entry.is_dir && !entry.is_parent {
                            format!("{}/", entry.name)
                        } else {
                            entry.name.clone()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(prefix, row_style),
                            Span::styled(
                                tag,
                                if entry.is_dir {
                                    theme.accent_style().patch(if is_sel {
                                        row_style
                                    } else {
                                        Style::default()
                                    })
                                } else {
                                    row_style
                                },
                            ),
                            Span::styled(truncate_width(&display_name, name_budget), row_style),
                            Span::styled(
                                size_str,
                                if is_sel {
                                    theme.selection().remove_modifier(Modifier::BOLD)
                                } else {
                                    theme.muted_style()
                                },
                            ),
                        ]));
                        hitmap.add_line(
                            chunks[1].x,
                            chunks[1].y + vis_idx as u16,
                            chunks[1].width,
                            HitTarget::OverlayItem(abs_idx),
                        );
                    }
                }
            }
            FilePickerMode::Zoxide => {
                let dirs = self.visible_zoxide_dirs();
                if dirs.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  No matching zoxide directories",
                        theme.muted_style(),
                    )));
                } else {
                    let sel = self.selected.min(dirs.len().saturating_sub(1));
                    let start = if sel >= max_rows && max_rows > 0 {
                        sel - max_rows + 1
                    } else {
                        0
                    };
                    for (vis_idx, dir) in dirs.iter().skip(start).take(max_rows).enumerate() {
                        let abs_idx = start + vis_idx;
                        let is_sel = abs_idx == sel;
                        let row_style = if is_sel {
                            theme.selection()
                        } else {
                            Style::default().fg(theme.fg)
                        };
                        let prefix = if is_sel { "▸ " } else { "  " };
                        let pstr = dir.to_string_lossy();
                        lines.push(Line::from(vec![
                            Span::styled(prefix, row_style),
                            Span::styled(
                                truncate_width(&pstr, w.saturating_sub(prefix.len())),
                                row_style,
                            ),
                        ]));
                        hitmap.add_line(
                            chunks[1].x,
                            chunks[1].y + vis_idx as u16,
                            chunks[1].width,
                            HitTarget::OverlayItem(abs_idx),
                        );
                    }
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);

        let trailing = if let Some(msg) = &self.status_msg {
            format!("· {msg}")
        } else if self.mode == FilePickerMode::Zoxide {
            "· Enter jump · Esc browse".to_string()
        } else {
            "· Enter open/upload · Backspace parent · Esc close".to_string()
        };
        render_button_bar(
            frame,
            chunks[2],
            &[
                ("[..] Parent", ButtonId::OverlayFileParent, modal_pill_secondary(theme)),
                ("[z] Zoxide", ButtonId::OverlayFileZoxide, modal_pill_secondary(theme)),
            ],
            Some(&trailing),
            theme,
            hitmap,
        );
    }
}

/// Parses newline-delimited output of `zoxide query -l` into unique `PathBuf`s.
pub fn parse_zoxide_output(stdout: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }
    dirs
}

/// Queries installed `zoxide` (`zoxide query -l`) for frecency-ranked directories.
pub fn query_zoxide_dirs() -> Result<Vec<PathBuf>, String> {
    let output = Command::new("zoxide")
        .args(["query", "-l"])
        .output()
        .map_err(|_| "zoxide is not installed on PATH".to_string())?;
    if !output.status.success() {
        return Err("zoxide query failed".to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_zoxide_output(&stdout))
}

fn format_size(bytes: u64) -> String {
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

    #[test]
    fn browses_filters_navigates_and_selects_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("alpha_dir");
        fs::create_dir(&sub).unwrap();
        let nested_file = sub.join("diagram.png");
        fs::write(&nested_file, b"png").unwrap();
        let root_file = tmp.path().join("notes.txt");
        fs::write(&root_file, b"hello").unwrap();

        let mut ov = FilePickerOverlay::new_in_dir(
            UploadTarget::Issue("ENG-42".into()),
            tmp.path().to_path_buf(),
        );

        // Directories sorted before files (after `..` if parent exists)
        let names: Vec<&str> = ov.visible_entries().iter().map(|e| e.name.as_str()).collect();
        let dir_pos = names.iter().position(|&n| n == "alpha_dir").unwrap();
        let file_pos = names.iter().position(|&n| n == "notes.txt").unwrap();
        assert!(dir_pos < file_pos);

        // Type-to-filter narrows to alpha_dir
        ov.handle(&Action::Input('a'));
        ov.handle(&Action::Input('l'));
        assert_eq!(ov.visible_entries().len(), 1);
        assert_eq!(ov.visible_entries()[0].name, "alpha_dir");

        // Enter navigates into alpha_dir and clears filter
        assert_eq!(ov.handle(&Action::Submit), OverlayOutcome::Handled);
        assert_eq!(ov.current_dir, sub);
        assert!(ov.filter.is_empty());

        // Filter and select diagram.png
        ov.handle(&Action::Input('d'));
        ov.handle(&Action::Input('i'));
        assert_eq!(ov.visible_entries().len(), 1);
        assert_eq!(
            ov.handle(&Action::Submit),
            OverlayOutcome::Dispatch(Action::Submit)
        );
        assert_eq!(ov.selected_file, Some(nested_file));

        // Backspace clears filter first, then navigates to parent
        ov.selected_file = None;
        ov.handle(&Action::Backspace);
        assert_eq!(ov.filter, "d");
        ov.handle(&Action::Backspace);
        assert!(ov.filter.is_empty());
        ov.handle(&Action::Backspace);
        assert_eq!(ov.current_dir, tmp.path());
    }

    #[test]
    fn parses_zoxide_output_and_jumps_to_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("proj-a");
        let d2 = tmp.path().join("proj-b");
        fs::create_dir(&d1).unwrap();
        fs::create_dir(&d2).unwrap();

        let raw = format!("{}\n{}\n{}\n\n", d1.display(), d2.display(), d1.display());
        let parsed = parse_zoxide_output(&raw);
        assert_eq!(parsed, vec![d1.clone(), d2.clone()]);

        let mut ov = FilePickerOverlay::new_in_dir(
            UploadTarget::Page("900".into()),
            tmp.path().to_path_buf(),
        );
        ov.enter_zoxide_with(parsed);
        assert_eq!(ov.mode, FilePickerMode::Zoxide);
        assert_eq!(ov.visible_zoxide_dirs().len(), 2);

        for ch in d2.to_string_lossy().chars() {
            ov.handle(&Action::Input(ch));
        }
        assert_eq!(ov.visible_zoxide_dirs().len(), 1);
        assert_eq!(ov.handle(&Action::Submit), OverlayOutcome::Handled);
        assert_eq!(ov.mode, FilePickerMode::Browse);
        assert_eq!(ov.current_dir, d2);
    }
}
