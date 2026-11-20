//! Keyboard bindings & `Action` definitions for Jira + Confluence TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::{PaneFocus, WorkspaceTab};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    SwitchTab(usize),
    FocusNextPane,
    FocusPrevPane,

    // Directional / list navigation
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    GoTop,
    GoBottom,
    Activate,
    ExpandOrToggle,
    CollapseNode,

    // Jira / Confluence domain actions
    OpenTransition,
    AssignToMe,
    OpenAssignee,
    OpenComment,
    NewItem,
    EditItem,
    UploadAttachment,
    OpenFilePicker,
    DownloadAttachment,
    OpenImagePreview,
    OpenHtmlPreview,
    OpenCrossLinks,
    OpenLinks,
    ToggleHrefDisplay,
    PopNavHistory,
    OpenInBrowser,
    FocusJql,
    FocusFilter,
    ToggleDrawerOrToc,
    TogglePin,
    SyncAll,
    RemoteSearch,
    CheckUpdate,
    ToggleInlineImages,
    ToggleNavRail,
    ToggleNotify,
    ToggleDensity,
    CycleTheme,
    CycleImageProtocol,

    // Overlays
    OpenCommandPalette,
    OpenSiteSwitcher,
    DiscoverCloudSites,
    OpenSettings,
    OpenContextMenu,
    OpenAttachments,
    OpenHelp,
    CloseOverlay,
    ConfirmYes,
    ConfirmNo,

    // Text editing / forms / modals
    Input(char),
    Backspace,
    Delete,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,
    InsertNewline,
    Paste(String),
    PasteClipboard,
    Submit,
    ExternalEditor,
    NextField,
    PrevField,

    // Mouse
    MouseClick { row: u16, col: u16 },
    MouseRightClick { row: u16, col: u16 },
    LoadPreviewImage,
    DownloadPreviewImage,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap;

impl Keymap {
    pub fn resolve(
        &self,
        key: KeyEvent,
        tab: WorkspaceTab,
        focus: PaneFocus,
        has_overlay: bool,
        accepts_text: bool,
    ) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            return Some(Action::Quit);
        }

        // Global workspace tab switching (Ctrl+1..6 / Alt+1..6).
        if (ctrl || alt) && !has_overlay {
            if let KeyCode::Char(c @ '1'..='6') = key.code {
                return Some(Action::SwitchTab((c as usize) - ('1' as usize)));
            }
        }

        // Alt+Left pops Cross-App nav history globally when no modal is open.
        if alt && key.code == KeyCode::Left && !has_overlay {
            return Some(Action::PopNavHistory);
        }

        if ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
            return Some(Action::OpenCommandPalette);
        }

        if ctrl && key.code == KeyCode::Char(',') {
            return Some(Action::OpenSettings);
        }

        if ctrl && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B')) && !has_overlay {
            return Some(Action::ToggleNavRail);
        }

        if alt && matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) && !has_overlay {
            return Some(Action::OpenContextMenu);
        }

        if ctrl && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) && !has_overlay {
            return Some(Action::RemoteSearch);
        }

        if ctrl && matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E')) && accepts_text {
            return Some(Action::ExternalEditor);
        }

        if ctrl && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            && (accepts_text || !has_overlay)
        {
            return Some(Action::PasteClipboard);
        }

        if ctrl && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F')) && !has_overlay {
            return Some(Action::OpenFilePicker);
        }

        if ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) {
            if accepts_text {
                return Some(Action::Submit);
            }
            return Some(Action::OpenSiteSwitcher);
        }

        if alt && key.code == KeyCode::Enter && accepts_text {
            return Some(Action::Submit);
        }

        if key.code == KeyCode::Esc {
            if has_overlay || accepts_text {
                return Some(Action::CloseOverlay);
            }
            return None;
        }

        // Text-input mode (overlay form, JQL bar, filter bar, search bar).
        if accepts_text {
            return match key.code {
                KeyCode::Enter if shift => Some(Action::InsertNewline),
                KeyCode::Enter => Some(Action::Submit),
                KeyCode::Tab => Some(Action::NextField),
                KeyCode::BackTab => Some(Action::PrevField),
                KeyCode::Backspace => Some(Action::Backspace),
                KeyCode::Delete => Some(Action::Delete),
                KeyCode::Left => Some(Action::CursorLeft),
                KeyCode::Right => Some(Action::CursorRight),
                KeyCode::Up => Some(Action::CursorUp),
                KeyCode::Down => Some(Action::CursorDown),
                KeyCode::Home => Some(Action::CursorHome),
                KeyCode::End => Some(Action::CursorEnd),
                KeyCode::Char(c) if !ctrl && !alt => Some(Action::Input(c)),
                _ => None,
            };
        }

        // Non-text overlay mode (e.g. TransitionOverlay, CrossLinkOverlay, ImagePreview, HtmlPreview, Confirm, Help).
        if has_overlay {
            return match key.code {
                KeyCode::Tab => Some(Action::NextField),
                KeyCode::BackTab => Some(Action::PrevField),
                KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
                KeyCode::Left | KeyCode::Char('h') => Some(Action::MoveLeft),
                KeyCode::Right | KeyCode::Char('l') => Some(Action::MoveRight),
                KeyCode::PageUp => Some(Action::PageUp),
                KeyCode::PageDown => Some(Action::PageDown),
                KeyCode::Home => Some(Action::GoTop),
                KeyCode::End => Some(Action::GoBottom),
                KeyCode::Enter => Some(Action::Activate),
                KeyCode::Backspace => Some(Action::CloseOverlay),
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::ConfirmYes),
                KeyCode::Char('n') | KeyCode::Char('N') => Some(Action::ConfirmNo),
                KeyCode::Char(c) if !ctrl && !alt => Some(Action::Input(c)),
                _ => None,
            };
        }

        // Main workspace command mode.
        if ctrl {
            return match key.code {
                KeyCode::Char('u') => Some(Action::PageUp),
                KeyCode::Char('d') => Some(Action::PageDown),
                _ => None,
            };
        }

        match key.code {
            KeyCode::Char(c @ '1'..='6') => Some(Action::SwitchTab((c as usize) - ('1' as usize))),
            KeyCode::Tab => Some(Action::FocusNextPane),
            KeyCode::BackTab => Some(Action::FocusPrevPane),
            KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
            KeyCode::Left => {
                if tab == WorkspaceTab::Confluence && focus == PaneFocus::Nav {
                    Some(Action::CollapseNode)
                } else {
                    Some(Action::MoveLeft)
                }
            }
            KeyCode::Char('h') => {
                if matches!(
                    (tab, focus),
                    (WorkspaceTab::Confluence, PaneFocus::Main)
                        | (WorkspaceTab::Issues, PaneFocus::Main | PaneFocus::Detail)
                        | (WorkspaceTab::Board, PaneFocus::Detail)
                ) {
                    Some(Action::ToggleHrefDisplay)
                } else if tab == WorkspaceTab::Confluence && focus == PaneFocus::Nav {
                    Some(Action::CollapseNode)
                } else {
                    Some(Action::MoveLeft)
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if tab == WorkspaceTab::Confluence && focus == PaneFocus::Nav {
                    Some(Action::ExpandOrToggle)
                } else {
                    Some(Action::MoveRight)
                }
            }
            KeyCode::Char(' ') => {
                if tab == WorkspaceTab::Confluence && focus == PaneFocus::Nav {
                    Some(Action::ExpandOrToggle)
                } else {
                    Some(Action::PageDown)
                }
            }
            KeyCode::PageUp => Some(Action::PageUp),
            KeyCode::PageDown => Some(Action::PageDown),
            KeyCode::Home | KeyCode::Char('G') => {
                if key.code == KeyCode::Home {
                    Some(Action::GoTop)
                } else {
                    Some(Action::GoBottom)
                }
            }
            KeyCode::End => Some(Action::GoBottom),
            KeyCode::Enter => Some(Action::Activate),
            KeyCode::Backspace => Some(Action::PopNavHistory),
            KeyCode::Char('m') => Some(Action::OpenTransition),
            KeyCode::Char('i') => Some(Action::AssignToMe),
            KeyCode::Char('a') => Some(Action::OpenAssignee),
            KeyCode::Char('A') => Some(Action::OpenAttachments),
            KeyCode::Char('c') => Some(Action::OpenComment),
            KeyCode::Char('e') => Some(Action::EditItem),
            KeyCode::Char('n') => Some(Action::NewItem),
            KeyCode::Char('u') => Some(Action::OpenLinks),
            KeyCode::Char('d') => Some(Action::DownloadAttachment),
            KeyCode::Char('v') => Some(Action::OpenImagePreview),
            KeyCode::Char('H') => Some(Action::OpenHtmlPreview),
            KeyCode::Char('g') => Some(Action::OpenCrossLinks),
            KeyCode::Char('o') => Some(Action::OpenInBrowser),
            KeyCode::Char('J') => Some(Action::FocusJql),
            KeyCode::Char('/') => Some(Action::FocusFilter),
            KeyCode::Char('t') => Some(Action::ToggleDrawerOrToc),
            KeyCode::Char('p') => Some(Action::TogglePin),
            KeyCode::Char('r') => Some(Action::SyncAll),
            KeyCode::Char('U') => Some(Action::CheckUpdate),
            KeyCode::Char('.') => Some(Action::OpenContextMenu),
            KeyCode::Char(':') => Some(Action::OpenCommandPalette),
            KeyCode::Char('?') => Some(Action::OpenHelp),
            KeyCode::Char('q') => Some(Action::Quit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn keymap_maps_jira_and_confluence_shortcuts() {
        let km = Keymap;
        assert_eq!(
            km.resolve(key(KeyCode::Char('i')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::AssignToMe)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('m')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::OpenTransition)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('s')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::OpenSiteSwitcher)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('s')), WorkspaceTab::Issues, PaneFocus::Main, true, true),
            Some(Action::Submit)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('g')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::OpenCrossLinks)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('v')), WorkspaceTab::Confluence, PaneFocus::Main, false, false),
            Some(Action::OpenImagePreview)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('J')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::FocusJql)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('4')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::SwitchTab(3))
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char(',')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::OpenSettings)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('b')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::ToggleNavRail)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('.')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::OpenContextMenu)
        );
        assert_eq!(
            km.resolve(
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT),
                WorkspaceTab::Board,
                PaneFocus::Main,
                false,
                false
            ),
            Some(Action::OpenContextMenu)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('A')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::OpenAttachments)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('u')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::OpenLinks)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('f')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::OpenFilePicker)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('v')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::PasteClipboard)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('h')), WorkspaceTab::Confluence, PaneFocus::Main, false, false),
            Some(Action::ToggleHrefDisplay)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('h')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::ToggleHrefDisplay)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('h')), WorkspaceTab::Board, PaneFocus::Detail, false, false),
            Some(Action::ToggleHrefDisplay)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('h')), WorkspaceTab::Board, PaneFocus::Main, false, false),
            Some(Action::MoveLeft)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('h')), WorkspaceTab::Confluence, PaneFocus::Nav, false, false),
            Some(Action::CollapseNode)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('H')), WorkspaceTab::Confluence, PaneFocus::Main, false, false),
            Some(Action::OpenHtmlPreview)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('H')), WorkspaceTab::Issues, PaneFocus::Main, false, false),
            Some(Action::OpenHtmlPreview)
        );
        assert_eq!(
            km.resolve(key(KeyCode::Tab), WorkspaceTab::Confluence, PaneFocus::Main, true, false),
            Some(Action::NextField)
        );
        assert_eq!(
            km.resolve(key(KeyCode::BackTab), WorkspaceTab::Confluence, PaneFocus::Main, true, false),
            Some(Action::PrevField)
        );
    }

    #[test]
    fn keymap_sends_plain_letters_as_input_when_accepting_text() {
        let km = Keymap;
        assert_eq!(
            km.resolve(key(KeyCode::Char('i')), WorkspaceTab::Issues, PaneFocus::JqlBar, false, true),
            Some(Action::Input('i'))
        );
        assert_eq!(
            km.resolve(key(KeyCode::Char('J')), WorkspaceTab::Issues, PaneFocus::JqlBar, false, true),
            Some(Action::Input('J'))
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('e')), WorkspaceTab::Issues, PaneFocus::Main, true, true),
            Some(Action::ExternalEditor)
        );
        assert_eq!(
            km.resolve(ctrl(KeyCode::Char('v')), WorkspaceTab::Issues, PaneFocus::Main, true, true),
            Some(Action::PasteClipboard)
        );
    }
}
