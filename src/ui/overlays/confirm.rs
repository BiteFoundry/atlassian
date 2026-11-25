//! Confirmation modal (`ConfirmDialog`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    app::{AppState, UiState},
    hitmap::{ButtonId, HitMap},
    keymap::Action,
    overlays::{
        centered_rect, modal_block, modal_pill_muted, modal_pill_primary, register_modal_chrome,
        render_button_bar, OverlayOutcome,
    },
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
            Action::Submit
            | Action::ConfirmYes
            | Action::Activate
            | Action::Input('y')
            | Action::Input('Y') => match self.on_confirm.clone() {
                Some(act) => OverlayOutcome::CloseAndDispatch(act),
                None => OverlayOutcome::Close,
            },
            Action::ConfirmNo
            | Action::CloseOverlay
            | Action::Input('n')
            | Action::Input('N')
            | Action::Input('q') => OverlayOutcome::Close,
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
        let rect = centered_rect(area, 56.min(area.width.saturating_sub(4)).max(26), 8);
        frame.render_widget(Clear, rect);

        let block = modal_block(" Confirm ", theme, true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let msg_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(2).max(1),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", self.message),
                Style::default().fg(theme.fg),
            )))
            .wrap(Wrap { trim: true }),
            msg_area,
        );

        let btn_y = inner.y + inner.height.saturating_sub(1);
        let btn_area = Rect::new(inner.x, btn_y, inner.width, 1);
        render_button_bar(
            frame,
            btn_area,
            &[
                ("[Enter] Confirm", ButtonId::OverlaySubmit, modal_pill_primary(theme)),
                ("[Esc] Cancel", ButtonId::OverlayCancel, modal_pill_muted(theme)),
            ],
            None,
            theme,
            hitmap,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_dispatches_and_cancel_closes() {
        let mut dialog = ConfirmDialog::new("Quit Atlassian TUI?", Some(Action::Quit));
        assert_eq!(
            dialog.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::Quit)
        );
        assert_eq!(
            dialog.handle(&Action::ConfirmYes),
            OverlayOutcome::CloseAndDispatch(Action::Quit)
        );
        assert_eq!(dialog.handle(&Action::ConfirmNo), OverlayOutcome::Close);
        assert_eq!(dialog.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }
}
