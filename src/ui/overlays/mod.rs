//! Overlay stack, HelpOverlay (`?`), and modal dispatch.

pub mod assignee;
pub mod command_palette;
pub mod confirm;
pub mod cross_link;
pub mod image_preview;
pub mod issue_form;
pub mod page_form;
pub mod path_input;
pub mod comment;
pub mod transition;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    hitmap::HitMap,
    keymap::Action,
    overlays::{
        assignee::AssigneeOverlay, command_palette::CommandPalette, comment::CommentOverlay,
        confirm::ConfirmDialog, cross_link::CrossLinkOverlay, image_preview::ImagePreviewOverlay,
        issue_form::IssueFormOverlay, page_form::PageFormOverlay, path_input::PathInputOverlay,
        transition::TransitionOverlay,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
}

#[derive(Debug, Clone)]
pub enum Overlay {
    Transition(TransitionOverlay),
    Assignee(AssigneeOverlay),
    IssueForm(IssueFormOverlay),
    PageForm(PageFormOverlay),
    Comment(CommentOverlay),
    CrossLink(CrossLinkOverlay),
    ImagePreview(ImagePreviewOverlay),
    PathInput(PathInputOverlay),
    CommandPalette(CommandPalette),
    Help,
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn accepts_text(&self) -> bool {
        matches!(
            self,
            Overlay::Assignee(_)
                | Overlay::IssueForm(_)
                | Overlay::PageForm(_)
                | Overlay::Comment(_)
                | Overlay::PathInput(_)
                | Overlay::CommandPalette(_)
        )
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match self {
            Overlay::Transition(o) => o.handle(action),
            Overlay::Assignee(o) => o.handle(action),
            Overlay::IssueForm(o) => o.handle(action),
            Overlay::PageForm(o) => o.handle(action),
            Overlay::Comment(o) => o.handle(action),
            Overlay::CrossLink(o) => o.handle(action),
            Overlay::ImagePreview(o) => o.handle(action),
            Overlay::PathInput(o) => o.handle(action),
            Overlay::CommandPalette(o) => o.handle(action),
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
        &self,
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
            Overlay::ImagePreview(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::CommandPalette(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Confirm(o) => o.draw(frame, area, app, ui, hitmap),
            Overlay::Help => draw_help_overlay(frame, area, ui),
        }
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width).max(1);
    let h = height.min(area.height).max(1);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_help_overlay(frame: &mut Frame, area: Rect, ui: &UiState) {
    let theme = &ui.theme;
    let rect = centered_rect(
        area,
        74.min(area.width.saturating_sub(4)).max(32),
        24.min(area.height.saturating_sub(2)).max(12),
    );
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" Atlassian TUI 단축키 도움말 (?) ", theme.title(true))))
        .border_style(theme.border_style(true));

    let row = |k: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {k:<18}"), theme.accent_style()),
            Span::raw(desc.to_string()),
        ])
    };

    let lines = vec![
        Line::from(Span::styled(" [전역 & 워크스페이스 탭]", Style::default().fg(theme.confluence_teal))),
        row("1..6 / Ctrl+1..6", "탭 전환 (보드 / 이슈JQL / 내작업 / 위키 / 통합검색 / 디렉터리)"),
        row("Tab / Shift+Tab", "좌측 탐색 ↔ 메인 캔버스 ↔ 우측 서랍 포커스 순환"),
        row("Ctrl+P / :", "명령 팔레트 열기"),
        row("r / U / ?", "전체 동기화 / 업데이트 확인 / 도움말"),
        row("g / Backspace", "연결된 Jira↔Confluence 문서 점프 / 이전 히스토리 복귀"),
        Line::from(""),
        Line::from(Span::styled(" [Jira 보드 & 이슈]", Style::default().fg(theme.accent))),
        row("h/l, j/k, Enter", "칸반 컬럼·카드 이동 및 상세 서랍 열기"),
        row("m / i / a", "상태 전환 / 내게 할당(Assign to me) / 담당자 검색"),
        row("n / e / c", "새 이슈 생성 / 이슈 수정($EDITOR) / 댓글 작성"),
        row("J / /", "JQL 검색바 포커스 / 로컬 필터"),
        Line::from(""),
        Line::from(Span::styled(" [Confluence 위키 & 공통]", Style::default().fg(theme.success))),
        row("Space / → / ←", "페이지 트리 하위 확장(지연 로드) 및 접기"),
        row("t / e / n / c", "목차(TOC) 토글 / 페이지 편집($EDITOR) / 하위 생성 / 댓글"),
        row("u / d / v / o", "파일 업로드 / 다운로드 / 이미지 미리보기 / 브라우저 열기"),
        row("q / Esc", "종료 / 모달 닫기"),
    ];

    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), rect);
}
