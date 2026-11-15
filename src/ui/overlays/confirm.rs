//! Confirmation modal (`ConfirmDialog`).

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
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub message: String,
    pub on_confirm: Option<Action>,
}

impl ConfirmDialog {
    pub fn new(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Self {
            message: message.into(),
            on_confirm,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::ConfirmYes | Action::Activate => match self.on_confirm.clone() {
                Some(act) => OverlayOutcome::CloseAndDispatch(act),
                None => OverlayOutcome::Close,
            },
            Action::ConfirmNo | Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
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
        let rect = centered_rect(area, 52.min(area.width.saturating_sub(4)).max(24), 7);
        frame.render_widget(Clear, rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(" 확인 ", theme.title(true))))
            .border_style(theme.border_style(true));
        let text = vec![
            Line::from(self.message.clone()),
            Line::from(""),
            Line::from(vec![
                Span::styled(" [y/Enter] 확인 ", Style::default().fg(theme.success)),
                Span::raw("   "),
                Span::styled(" [n/Esc] 취소 ", Style::default().fg(theme.muted)),
            ]),
        ];
        frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), rect);
    }
}
