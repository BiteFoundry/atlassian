//! Jira issue transition picker modal (`m`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::{
    models::IssueTransition,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{centered_rect, modal_block, register_modal_chrome, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct TransitionOverlay {
    pub issue_key: String,
    /// Site of the row the overlay was opened from; the transition is sent there.
    pub site_id: Option<String>,
    pub transitions: Vec<IssueTransition>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl TransitionOverlay {
    pub fn new(issue_key: impl Into<String>) -> Self {
        Self {
            issue_key: issue_key.into(),
            site_id: None,
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

    pub fn select(&mut self, index: usize) {
        if index < self.transitions.len() {
            self.selected = index;
        }
    }

    pub fn selected_transition(&self) -> Option<&IssueTransition> {
        self.transitions.get(self.selected)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::MoveUp | Action::CursorUp | Action::PrevField => {
                if !self.transitions.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::NextField => {
                if self.selected + 1 < self.transitions.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Submit => {
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
        let rect = centered_rect(
            area,
            58.min(area.width.saturating_sub(4)).max(26),
            16.min(area.height.saturating_sub(2)).max(8),
        );
        frame.render_widget(Clear, rect);

        let block = modal_block(format!(" Transition Status — {} ", self.issue_key), theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let list_h = inner.height.saturating_sub(1) as usize;
        let mut lines = Vec::new();
        if self.loading {
            lines.push(Line::from(Span::styled(
                " Loading available transitions…",
                theme.muted_style(),
            )));
        } else if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme.error),
            )));
        } else if self.transitions.is_empty() {
            lines.push(Line::from(Span::styled(
                " No available transitions for this issue.",
                theme.muted_style(),
            )));
        } else {
            let start = if self.selected >= list_h && list_h > 0 {
                self.selected - list_h + 1
            } else {
                0
            };
            for (vis_idx, tr) in self.transitions.iter().skip(start).take(list_h).enumerate() {
                let idx = start + vis_idx;
                let is_sel = idx == self.selected;
                let row_style = if is_sel {
                    theme.selection()
                } else {
                    Style::default().fg(theme.fg)
                };
                let marker = if is_sel { "▸ " } else { "  " };
                let name = truncate_width(&tr.name, inner.width.saturating_sub(22) as usize);
                let lozenge = theme.status_lozenge_span(&tr.to_status.name, tr.to_status.category);
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker}{name}  →  "), row_style),
                    lozenge,
                ]));
                hitmap.add_line(
                    inner.x,
                    inner.y + vis_idx as u16,
                    inner.width,
                    HitTarget::OverlayItem(idx),
                );
            }
        }

        let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(
                    " j/k navigate · Enter confirm · Esc cancel",
                    footer_area.width as usize,
                ),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{IssueStatus, StatusCategory};

    #[test]
    fn transition_selection_and_submit() {
        let mut ov = TransitionOverlay::new("ENG-5");
        assert!(ov.loading);
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Handled);

        ov.set_transitions(vec![
            IssueTransition {
                id: "11".into(),
                name: "Start Progress".into(),
                to_status: IssueStatus {
                    id: "2".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
            },
            IssueTransition {
                id: "21".into(),
                name: "Done".into(),
                to_status: IssueStatus {
                    id: "3".into(),
                    name: "Done".into(),
                    category: StatusCategory::Done,
                },
            },
        ]);
        assert!(!ov.loading);
        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected_transition().unwrap().id, "21");
        assert_eq!(
            ov.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::Submit)
        );
    }
}
