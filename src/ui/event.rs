//! Crossterm key and mouse event translation to `Action`.

use crossterm::event::{KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};

use crate::ui::{
    app::{PaneFocus, WorkspaceTab},
    hitmap::{ButtonId, HitMap, HitTarget},
    keymap::{Action, Keymap},
};

pub fn key_to_action(
    key: KeyEvent,
    tab: WorkspaceTab,
    focus: PaneFocus,
    has_overlay: bool,
    accepts_text: bool,
    keymap: &Keymap,
) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    keymap.resolve(key, tab, focus, has_overlay, accepts_text)
}

pub fn mouse_to_action(mouse: MouseEvent, hitmap: &HitMap) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            match hitmap.hit(mouse.row, mouse.column) {
                Some(HitTarget::Button(btn)) => return Some(button_to_action(btn)),
                Some(HitTarget::OverlayClose) => return Some(Action::CloseOverlay),
                _ => {}
            }
            Some(Action::MouseClick {
                row: mouse.row,
                col: mouse.column,
            })
        }
        MouseEventKind::Down(MouseButton::Right) => Some(Action::MouseRightClick {
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::ScrollUp => Some(Action::ScrollUp),
        MouseEventKind::ScrollDown => Some(Action::ScrollDown),
        _ => None,
    }
}

pub fn button_to_action(btn: ButtonId) -> Action {
    match btn {
        ButtonId::SyncAll => Action::SyncAll,
        ButtonId::CommandPalette => Action::OpenCommandPalette,
        ButtonId::Help => Action::OpenHelp,
        ButtonId::Transition => Action::OpenTransition,
        ButtonId::AssignToMe => Action::AssignToMe,
        ButtonId::Assignee => Action::OpenAssignee,
        ButtonId::Comment => Action::OpenComment,
        ButtonId::Edit => Action::EditItem,
        ButtonId::NewItem => Action::NewItem,
        ButtonId::UploadAttachment => Action::UploadAttachment,
        ButtonId::DownloadAttachment => Action::DownloadAttachment,
        ButtonId::ImagePreview => Action::OpenImagePreview,
        ButtonId::HtmlPreview => Action::OpenHtmlPreview,
        ButtonId::CrossLinks => Action::OpenCrossLinks,
        ButtonId::OpenBrowser => Action::OpenInBrowser,
        ButtonId::ToggleToc => Action::ToggleDrawerOrToc,
        ButtonId::FocusJql => Action::FocusJql,
        ButtonId::BackHistory => Action::PopNavHistory,
        ButtonId::CheckUpdate => Action::CheckUpdate,
        ButtonId::ToggleInlineImages => Action::ToggleInlineImages,
        ButtonId::ToggleDrawer => Action::ToggleDrawerOrToc,
        ButtonId::SiteSwitcher => Action::OpenSiteSwitcher,
    }
}
