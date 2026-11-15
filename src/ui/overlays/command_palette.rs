//! Command palette (`Ctrl+P` / `:`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    doc_view::truncate_width,
    hitmap::{HitMap, HitTarget},
    keymap::Action,
    overlays::{centered_rect, OverlayOutcome},
    text_edit::TextBuffer,
};

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub struct CommandPalette {
    pub query: TextBuffer,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self {
            query: TextBuffer::new(),
            selected: 0,
        }
    }

    pub fn entries() -> Vec<PaletteEntry> {
        vec![
            PaletteEntry { label: "칸반 보드 탭으로 이동", shortcut: "Ctrl+1", action: Action::SwitchTab(0) },
            PaletteEntry { label: "이슈 탐색기 & JQL 탭으로 이동", shortcut: "Ctrl+2", action: Action::SwitchTab(1) },
            PaletteEntry { label: "내 작업 (My Work) 탭으로 이동", shortcut: "Ctrl+3", action: Action::SwitchTab(2) },
            PaletteEntry { label: "Confluence 위키/트리 탭으로 이동", shortcut: "Ctrl+4", action: Action::SwitchTab(3) },
            PaletteEntry { label: "통합 검색 탭으로 이동", shortcut: "Ctrl+5", action: Action::SwitchTab(4) },
            PaletteEntry { label: "프로젝트/디렉터리 & 동기화 탭으로 이동", shortcut: "Ctrl+6", action: Action::SwitchTab(5) },
            PaletteEntry { label: "이슈 상태 전환 (Transition)", shortcut: "m", action: Action::OpenTransition },
            PaletteEntry { label: "내게 할당 (Assign to me)", shortcut: "i", action: Action::AssignToMe },
            PaletteEntry { label: "담당자 지정 검색", shortcut: "a", action: Action::OpenAssignee },
            PaletteEntry { label: "댓글 작성", shortcut: "c", action: Action::OpenComment },
            PaletteEntry { label: "새 이슈 / 하위 페이지 생성", shortcut: "n", action: Action::NewItem },
            PaletteEntry { label: "이슈 / 페이지 편집 ($EDITOR 지원)", shortcut: "e", action: Action::EditItem },
            PaletteEntry { label: "연결된 Jira/Confluence 문서 점프", shortcut: "g", action: Action::OpenCrossLinks },
            PaletteEntry { label: "이전 문서로 돌아가기 (히스토리 팝)", shortcut: "Backspace", action: Action::PopNavHistory },
            PaletteEntry { label: "이미지 첨부파일 미리보기", shortcut: "v", action: Action::OpenImagePreview },
            PaletteEntry { label: "첨부파일 업로드", shortcut: "u", action: Action::UploadAttachment },
            PaletteEntry { label: "첨부파일 다운로드", shortcut: "d", action: Action::DownloadAttachment },
            PaletteEntry { label: "브라우저에서 열기", shortcut: "o", action: Action::OpenInBrowser },
            PaletteEntry { label: "JQL 입력창 포커스", shortcut: "J", action: Action::FocusJql },
            PaletteEntry { label: "전체 동기화 실행", shortcut: "r", action: Action::SyncAll },
            PaletteEntry { label: "인라인 이미지 썸네일 토글", shortcut: "", action: Action::ToggleInlineImages },
            PaletteEntry { label: "네비게이션 레일 토글", shortcut: "", action: Action::ToggleNavRail },
            PaletteEntry { label: "GitHub 릴리스 업데이트 확인", shortcut: "U", action: Action::CheckUpdate },
            PaletteEntry { label: "단축키 도움말 열기", shortcut: "?", action: Action::OpenHelp },
            PaletteEntry { label: "종료", shortcut: "q", action: Action::Quit },
        ]
    }

    pub fn filtered_entries(&self) -> Vec<PaletteEntry> {
        let q = self.query.text.trim().to_ascii_lowercase();
        let all = Self::entries();
        if q.is_empty() {
            return all;
        }
        all.into_iter()
            .filter(|e| {
                e.label.to_ascii_lowercase().contains(&q)
                    || e.shortcut.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let items = self.filtered_entries();
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::CursorUp | Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::CursorDown | Action::MoveDown => {
                if self.selected + 1 < items.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.query.insert_char(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.backspace();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.query.delete();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(s) => {
                self.query.insert_str(s);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Submit | Action::Activate => {
                if let Some(entry) = items.get(self.selected) {
                    OverlayOutcome::CloseAndDispatch(entry.action.clone())
                } else {
                    OverlayOutcome::Close
                }
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
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            66.min(area.width.saturating_sub(4)).max(28),
            20.min(area.height.saturating_sub(2)).max(10),
        );
        frame.render_widget(Clear, rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(" 명령 팔레트 (Ctrl+P / :) ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" > ", theme.accent_style()),
                    Span::raw(&self.query.text),
                    Span::styled("▏", theme.accent_style()),
                ]),
                Line::from(Span::styled(
                    "─".repeat(chunks[0].width as usize),
                    Style::default().fg(theme.border),
                )),
            ]),
            chunks[0],
        );

        let items = self.filtered_entries();
        let max_rows = chunks[1].height as usize;
        let start = if self.selected >= max_rows {
            self.selected - max_rows + 1
        } else {
            0
        };
        let mut lines = Vec::new();
        for (vis_idx, entry) in items.iter().skip(start).take(max_rows).enumerate() {
            let abs_idx = start + vis_idx;
            let is_sel = abs_idx == self.selected;
            let style = if is_sel { theme.selection() } else { Style::default() };
            let prefix = if is_sel { "▸ " } else { "  " };
            let sc = if entry.shortcut.is_empty() {
                String::new()
            } else {
                format!(" [{}]", entry.shortcut)
            };
            let text = truncate_width(
                &format!("{prefix}{}{sc}", entry.label),
                chunks[1].width as usize,
            );
            lines.push(Line::from(Span::styled(text, style)));
            hitmap.add_line(
                chunks[1].x,
                chunks[1].y + vis_idx as u16,
                chunks[1].width,
                HitTarget::OverlayItem(abs_idx),
            );
        }
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    }
}
