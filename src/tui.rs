//! Native-grade Atlassian TUI entry shell (`jira` / `confluence` / `conf`).

use std::{
    io::{self, Stdout},
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use crate::{
    atlassian::AtlassianClient,
    auth::AuthManager,
    config::{AppConfig, Persona},
    models::CoreEvent,
    storage::Storage,
    sync::{self, SyncCommand},
    ui::{
        self, action,
        app::{AppState, ExternalEditorTarget, UiMsg, UiState},
        event,
        keymap::Action,
        overlays::Overlay,
        text_edit,
    },
};

struct TerminalGuard {
    keyboard_enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let keyboard_enhanced = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )
        .is_ok();

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((Self { keyboard_enhanced }, terminal))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }

    fn resume(&self) -> Result<()> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        if self.keyboard_enhanced {
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            );
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    client: AtlassianClient,
    storage: Storage,
) -> Result<()> {
    run_with_persona(config, Persona::detect(), auth, client, storage).await
}

pub async fn run_with_persona(
    config: AppConfig,
    persona: Persona,
    auth: AuthManager,
    client: AtlassianClient,
    storage: Storage,
) -> Result<()> {
    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let sync_ctrl =
        sync::spawn_sync_engine(config.clone(), client.clone(), storage.clone(), core_tx);

    let mut ui = UiState::from_config_with_persona(&config, persona);
    let mut app = AppState::new_with_persona(
        config,
        persona,
        auth,
        client,
        storage,
        sync_ctrl.clone(),
        ui_tx,
    );
    app.load_cached_initial();
    app.spawn_bootstrap(&mut ui);

    let (guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        if let Some((target, initial)) = ui.editor_request.take() {
            guard.restore();
            let result = text_edit::external_edit(&initial);
            guard.resume()?;
            terminal.clear()?;

            match result {
                Ok(edited) => {
                    apply_external_edit_result(&mut ui, target, edited);
                    ui.success("외부 편집기 내용을 반영했습니다");
                }
                Err(err) => {
                    ui.error(format!("외부 편집기 실행 실패: {err}"));
                }
            }
        }

        let mut cursor_pos = None;
        terminal.draw(|frame| {
            cursor_pos = ui::draw(frame, &app, &mut ui);
        })?;
        if let Some((cx, cy)) = cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }

        tokio::select! {
            _ = tick.tick() => {
                ui.toasts.tick(std::time::Instant::now());
            }
            maybe_core = core_rx.recv() => {
                match maybe_core {
                    Some(ev) => app.handle_core_event(&mut ui, ev),
                    None => break,
                }
            }
            maybe_ui = ui_rx.recv() => {
                if let Some(msg) = maybe_ui {
                    app.handle_ui_msg(&mut ui, msg);
                }
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        if let Some(act) = event::key_to_action(
                            key,
                            ui.active_tab,
                            ui.focus,
                            ui.has_overlay(),
                            ui.accepts_text_input(),
                            &ui.keymap,
                        ) {
                            if action::dispatch(act, &mut app, &mut ui) {
                                break;
                            }
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                            if action::dispatch(act, &mut app, &mut ui) {
                                break;
                            }
                        }
                    }
                    Some(Ok(CrosstermEvent::Paste(text))) => {
                        if action::dispatch(Action::Paste(text), &mut app, &mut ui) {
                            break;
                        }
                    }
                    Some(Ok(CrosstermEvent::Resize(w, h))) => {
                        ui.term_width = w;
                        ui.term_height = h;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    let _ = sync_ctrl.request(SyncCommand::Shutdown);
    drop(guard);
    Ok(())
}

fn apply_external_edit_result(ui: &mut UiState, target: ExternalEditorTarget, edited: String) {
    let Some(top) = ui.overlays.last_mut() else {
        return;
    };
    match (target, top) {
        (ExternalEditorTarget::IssueFormDescription, Overlay::IssueForm(form)) => {
            form.description.set_text(edited);
        }
        (ExternalEditorTarget::PageFormBody, Overlay::PageForm(form)) => {
            form.body.set_text(edited);
        }
        (ExternalEditorTarget::CommentBody, Overlay::Comment(comment)) => {
            comment.body.set_text(edited);
        }
        _ => {}
    }
}
