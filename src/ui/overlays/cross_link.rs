//! Cross-app Jira <-> Confluence link picker modal (`g`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::CrossRefTarget,
    ui::{
        app::{AppState, UiState},
        doc_view::truncate_width,
        hitmap::{HitMap, HitTarget},
        keymap::Action,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct CrossLinkOverlay {
    pub source_label: String,
    pub targets: Vec<CrossRefTarget>,
    pub selected: usize,
}

impl CrossLinkOverlay {
    pub fn new(source_label: impl Into<String>, targets: Vec<CrossRefTarget>) -> Self {
        Self {
            source_label: source_label.into(),
            targets,
            selected: 0,
        }
    }

    pub fn selected_target(&self) -> Option<&CrossRefTarget> {
        self.targets.get(self.selected)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('g') => OverlayOutcome::Close,
            Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if self.selected + 1 < self.targets.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.selected_target().is_some() {
                    OverlayOutcome::Dispatch(Action::Submit)
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
            68.min(area.width.saturating_sub(4)).max(28),
            18.min(area.height.saturating_sub(2)).max(8),
        );
        frame.render_widget(Clear, rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                format!(" 연결된 Jira/Confluence 이동 — {} ", self.source_label),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let max_rows = inner.height.saturating_sub(1) as usize;
        let mut lines = Vec::new();
        if self.targets.is_empty() {
            lines.push(Line::from(Span::styled(
                " 연결된 이슈나 Confluence 페이지가 없습니다.",
                theme.muted_style(),
            )));
        } else {
            for (idx, target) in self.targets.iter().take(max_rows).enumerate() {
                let is_sel = idx == self.selected;
                let row_style = if is_sel {
                    theme.selection()
                } else {
                    Style::default()
                };
                let prefix = if is_sel { "▸ " } else { "  " };
                let line = match target {
                    CrossRefTarget::JiraIssue { key, label } => {
                        let extra = label
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .map(|s| format!(" — {s}"))
                            .unwrap_or_default();
                        Line::from(vec![
                            Span::styled(prefix, row_style),
                            Span::styled(
                                " [JIRA] ",
                                Style::default()
                                    .bg(theme.jira_cobalt)
                                    .fg(ratatui::style::Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                truncate_width(&format!(" {key}{extra}"), inner.width.saturating_sub(12) as usize),
                                row_style,
                            ),
                        ])
                    }
                    CrossRefTarget::ConfluencePage { page_id, title } => {
                        let lbl = title
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(page_id);
                        Line::from(vec![
                            Span::styled(prefix, row_style),
                            Span::styled(
                                " [WIKI] ",
                                Style::default()
                                    .bg(theme.confluence_teal)
                                    .fg(ratatui::style::Color::Rgb(9, 30, 66))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                truncate_width(&format!(" {lbl} (#{page_id})"), inner.width.saturating_sub(12) as usize),
                                row_style,
                            ),
                        ])
                    }
                };
                lines.push(line);
                hitmap.add_line(inner.x, inner.y + idx as u16, inner.width, HitTarget::OverlayItem(idx));
            }
        }

        let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(" j/k: 이동   Enter: 점프   Backspace: 이전으로   Esc: 닫기", footer_area.width as usize),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ))),
            footer_area,
        );
    }
}
