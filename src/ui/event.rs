//! Crossterm key and mouse event translation to `Action`.

use crossterm::event::{KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};

use crate::ui::{
    app::{PaneFocus, WorkspaceTab},
    hitmap::{ButtonId, HitMap},
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

pub fn mouse_to_action(mouse: MouseEvent, _hitmap: &HitMap) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Action::MouseClick {
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::Down(MouseButton::Right) => Some(Action::MouseRightClick {
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::ScrollUp => Some(Action::MouseScroll {
            delta: -3,
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::ScrollDown => Some(Action::MouseScroll {
            delta: 3,
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::ScrollLeft => Some(Action::MouseScrollHoriz {
            delta: -1,
            row: mouse.row,
            col: mouse.column,
        }),
        MouseEventKind::ScrollRight => Some(Action::MouseScrollHoriz {
            delta: 1,
            row: mouse.row,
            col: mouse.column,
        }),
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
        ButtonId::ToggleNavRail => Action::ToggleNavRail,
        ButtonId::ToggleDensity => Action::ToggleDensity,
        ButtonId::CycleTheme => Action::CycleTheme,
        ButtonId::TogglePin => Action::TogglePin,
        ButtonId::Quit => Action::Quit,
        ButtonId::RunJql => Action::Activate,
        ButtonId::FocusLocalFilter => Action::FocusFilter,
        ButtonId::CloseLocalFilter => Action::CloseOverlay,
        ButtonId::RemoteSearch => Action::RemoteSearch,
        ButtonId::BoardColPrev => Action::MoveLeft,
        ButtonId::BoardColNext => Action::MoveRight,
        ButtonId::OverlaySubmit => Action::Submit,
        ButtonId::OverlayCancel => Action::CloseOverlay,
        ButtonId::OverlayExternalEditor => Action::ExternalEditor,
        ButtonId::OverlayDiscoverSites => Action::DiscoverCloudSites,
        ButtonId::OverlayCopyUrl => Action::Input('y'),
        ButtonId::OverlayFileParent => Action::CollapseNode,
        ButtonId::OverlayFileZoxide => Action::Input('z'),
        ButtonId::OverlayImagePrev => Action::MoveLeft,
        ButtonId::OverlayImageNext => Action::MoveRight,
        ButtonId::OverlayImageZoomIn => Action::Input('+'),
        ButtonId::OverlayImageZoomOut => Action::Input('-'),
        ButtonId::OverlayImageFit => Action::Input('0'),
        ButtonId::OverlayImageProtocol => Action::CycleImageProtocol,
    }
}
