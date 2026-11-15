//! Jira issue transition picker modal (`m`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::IssueTransition,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct TransitionOverlay {
    pub issue_key: String,
    pub transitions: Vec<IssueTransition>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl TransitionOverlay {
    pub fn new(issue_key: impl Into<String>) -> Self {
        Self {
            issue_key: issue_key.into(),
            transitions: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
        }
    }

    pub fn set_transitions(&mut self, transitions: Vec<IssueTransition>) {
        self.transitions = transitions;
        self.loading = false;
        self.error = None;
        if self.selected >= self.transitions.len() {
            self.selected = 0;
        }
    }

    pub fn set_error(&mut self, err: String) {
        self.loading = false;
        self.error = Some(err);
    }

    pub fn selected_transition(&self) -> Option<&IssueTransition> {
        self.transitions.get(self.selected)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::MoveUp => {
                if !self.transitions.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if self.selected + 1 < self.transitions.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.selected_transition().is_some() {
                    OverlayOutcome::Dispatch(Action::Submit)
                } else {
                    OverlayOutcome::Handled
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
        let rect = centered_rect(area, 56.min(area.width.saturating_sub(4)).max(24), 16.min(area.height.saturating_sub(2)).max(8));
        frame.render_widget(Clear, rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                format!(" 상태 전환 — {} ", self.issue_key),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let list_h = inner.height.saturating_sub(1) as usize;
        let mut lines = Vec::new();
        if self.loading {
            lines.push(Line::from(Span::styled(
                " 전환 가능한 상태를 불러오는 중…",
                theme.muted_style(),
            )));
        } else if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" 오류: {err}"),
                Style::default().fg(theme.error),
            )));
        } else if self.transitions.is_empty() {
            lines.push(Line::from(Span::styled(
                " 사용 가능한 상태 전환이 없습니다.",
                theme.muted_style(),
            )));
        } else {
            for (idx, tr) in self.transitions.iter().take(list_h).enumerate() {
                let is_sel = idx == self.selected;
                let row_style = if is_sel {
                    theme.selection()
                } else {
                    Style::default()
                };
                let marker = if is_sel { "▸ " } else { "  " };
                let name = truncate_width(&tr.name, inner.width.saturating_sub(22) as usize);
                let lozenge = theme.status_lozenge_span(&tr.to_status.name, tr.to_status.category);
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker}{name}  →  "), row_style),
                    lozenge,
                ]));
                hitmap.add_line(inner.x, inner.y + idx as u16, inner.width, HitTarget::OverlayItem(idx));
            }
        }

        let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(" j/k: 이동   Enter: 전환 적용   Esc: 닫기", footer_area.width as usize),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))),
            footer_area,
        );
    }
}
