//! Native-grade Atlassian TUI entry shell (`jira` / `confluence` / `conf`):
//! terminal raw mode, mouse capture, keyboard enhancement, synchronized pixel graphics overlay loop,
//! event coalescing, and external editor handoff.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Stdout, Write},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use futures::{FutureExt, StreamExt};
use ratatui::{backend::CrosstermBackend, layout::Rect, style::Color, Terminal};
use tokio::sync::mpsc;

use crate::{
    atlassian::AtlassianClient,
    auth::AuthManager,
    config::{AppConfig, Persona},
    img_preview::{
        self, DecodedImageCache, RenderedTerminalImage, TerminalGraphicsCaps,
        TerminalImageProtocol,
    },
    models::CoreEvent,
    storage::Storage,
    sync::{self, SyncCommand},
    ui::{
        self, action,
        app::{AppState, ExternalEditorTarget, UiMsg, UiState},
        event,
        keymap::Action,
        overlays::{image_preview::PendingPixelGraphic, Overlay},
        text_edit,
    },
};

const MAX_COALESCED_EVENTS: usize = 64;
const MAX_RAW_IMAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RAW_IMAGE_CACHE_ENTRIES: usize = 32;

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
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
        let keyboard_enhanced = if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )
            .is_ok()
        } else {
            false
        };

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
            },
            terminal,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.flush();
        }
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

struct PixelDecodeMsg {
    cache_key: String,
    att_id: String,
    raw_bytes: Option<Arc<[u8]>>,
    rendered: std::result::Result<RenderedTerminalImage, String>,
}

/// Manages startup-probed terminal graphics capabilities, the 8 MiB decoded image cache,
/// raw attachment byte reuse across protocol/zoom toggles, and synchronized pixel placement state.
struct PixelPreviewRuntime {
    graphics_caps: TerminalGraphicsCaps,
    decoded_cache: DecodedImageCache,
    raw_cache: HashMap<String, Arc<[u8]>>,
    raw_order: VecDeque<String>,
    raw_bytes: usize,
    in_flight: HashSet<String>,
    active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
    pixel_tx: mpsc::UnboundedSender<PixelDecodeMsg>,
    pixel_rx: mpsc::UnboundedReceiver<PixelDecodeMsg>,
}

impl PixelPreviewRuntime {
    fn new(graphics_caps: TerminalGraphicsCaps) -> Self {
        let (pixel_tx, pixel_rx) = mpsc::unbounded_channel();
        Self {
            graphics_caps,
            decoded_cache: DecodedImageCache::new(),
            raw_cache: HashMap::new(),
            raw_order: VecDeque::new(),
            raw_bytes: 0,
            in_flight: HashSet::new(),
            active_pixel_graphic: None,
            pixel_tx,
            pixel_rx,
        }
    }

    fn on_resize(&mut self) {
        self.graphics_caps = self.graphics_caps.refresh_cell_pixel_size();
    }

    fn insert_raw_bytes(&mut self, att_id: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.raw_cache.remove(&att_id) {
            self.raw_bytes = self.raw_bytes.saturating_sub(prev.len());
            self.raw_order.retain(|k| k != &att_id);
        }
        self.raw_cache.insert(att_id.clone(), bytes);
        self.raw_order.push_back(att_id);
        self.raw_bytes = self.raw_bytes.saturating_add(entry_bytes);

        while self.raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.raw_cache.remove(&evicted) {
                self.raw_bytes = self.raw_bytes.saturating_sub(removed.len());
            }
        }

        while self.raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.raw_order.len() > 1 {
            let Some(evicted) = self.raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.raw_cache.remove(&evicted) {
                self.raw_bytes = self.raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn sync_overlay_state(&mut self, app: &mut AppState, ui: &mut UiState) {
        let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() else {
            return;
        };
        ov.apply_graphics_caps(self.graphics_caps);
        ov.active_pixel_graphic = self.active_pixel_graphic.clone();

        let Some(att) = ov.current().cloned() else {
            return;
        };
        if !ov.protocol.is_pixel() {
            return;
        }

        let (cols, rows) = ov.target_dimensions(ui.term_width, ui.term_height);
        let cache_key = img_preview::preview_cache_key(&att.id, cols, rows, ov.protocol);

        if let Some(cached) = self.decoded_cache.get(&cache_key) {
            if !app.image_cache.contains_key(&cache_key) {
                app.image_cache.insert(cache_key, cached.clone());
            }
            return;
        }

        if app.image_cache.contains_key(&cache_key)
            || self.in_flight.contains(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }

        self.in_flight.insert(cache_key.clone());
        let cached_raw = self.raw_cache.get(&att.id).cloned();
        let client = app.client.clone();
        let tx = self.pixel_tx.clone();
        let caps = ov.graphics_caps();
        let bg_rgb = theme_preview_bg_rgb(&ui.theme);
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let (raw_opt, bytes_res): (Option<Arc<[u8]>>, Result<Arc<[u8]>, String>) =
                if let Some(raw) = cached_raw {
                    (None, Ok(raw))
                } else {
                    match client.fetch_attachment_bytes(&att).await {
                        Ok(b) => {
                            let arc: Arc<[u8]> = Arc::from(b);
                            (Some(arc.clone()), Ok(arc))
                        }
                        Err(err) => (None, Err(err.to_string())),
                    }
                };

            let rendered = match bytes_res {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    img_preview::decode_and_render_for_protocol(&bytes, cols, rows, bg_rgb, caps)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(format!("Image decode task failed: {e}"))),
                Err(err) => Err(err),
            };

            let _ = tx.send(PixelDecodeMsg {
                cache_key,
                att_id,
                raw_bytes: raw_opt,
                rendered,
            });
        });
    }

    fn handle_pixel_msg(&mut self, app: &mut AppState, ui: &mut UiState, msg: PixelDecodeMsg) {
        self.in_flight.remove(&msg.cache_key);
        if let Some(raw) = msg.raw_bytes {
            self.insert_raw_bytes(msg.att_id, raw);
        }
        match msg.rendered {
            Ok(img) => {
                let evicted = self.decoded_cache.insert(msg.cache_key.clone(), img.clone());
                for old_key in evicted {
                    app.image_cache.remove(&old_key);
                }
                app.image_cache.insert(msg.cache_key, img);
            }
            Err(err) => {
                if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() {
                    ov.error = Some(err);
                }
            }
        }
    }

    fn set_active_pixel_graphic(
        &mut self,
        ui: &mut UiState,
        active: Option<(String, Rect, TerminalImageProtocol)>,
    ) {
        self.active_pixel_graphic = active.clone();
        if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() {
            ov.active_pixel_graphic = active;
        }
    }

    fn clear_kitty_before_suspend(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        ui: &mut UiState,
    ) {
        if self
            .active_pixel_graphic
            .as_ref()
            .is_some_and(|(_, _, proto)| *proto == TerminalImageProtocol::Kitty)
        {
            let _ = terminal
                .backend_mut()
                .write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = terminal.backend_mut().flush();
        }
        self.set_active_pixel_graphic(ui, None);
    }
}

fn theme_preview_bg_rgb(theme: &ui::theme::Theme) -> (u8, u8, u8) {
    match theme.card_bg {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => img_preview::DEFAULT_PREVIEW_BG_RGB,
    }
}

fn current_pending_pixel_graphic(
    frame_area: Rect,
    app: &AppState,
    ui: &UiState,
) -> Option<PendingPixelGraphic> {
    let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
        return None;
    };
    ov.pending_pixel_graphic(frame_area, &app.image_cache)
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
    pixel_rt: &mut PixelPreviewRuntime,
) -> Result<()> {
    pixel_rt.sync_overlay_state(app, ui);

    let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
    let prev_graphic = pixel_rt.active_pixel_graphic.clone();

    let draw_res: Result<()> = (|| {
        let mut cursor_pos = None;
        let mut frame_area = Rect::default();
        terminal.draw(|frame| {
            frame_area = frame.area();
            cursor_pos = ui::draw(frame, app, ui);
        })?;

        let pending = current_pending_pixel_graphic(frame_area, app, ui);
        let pending_matches_prev = match (&prev_graphic, &pending) {
            (None, None) => true,
            (Some((pk, pr, pp)), Some(pg)) => {
                pk == &pg.cache_key && pr == &pg.rect && pp == &pg.protocol
            }
            _ => false,
        };

        if !pending_matches_prev {
            if let Some((_, _, prev_proto)) = &prev_graphic {
                if *prev_proto == TerminalImageProtocol::Kitty {
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                }
                terminal.clear()?;
                pixel_rt.set_active_pixel_graphic(ui, None);
                terminal.draw(|frame| {
                    frame_area = frame.area();
                    cursor_pos = ui::draw(frame, app, ui);
                })?;
            }

            let pending_after_clear = current_pending_pixel_graphic(frame_area, app, ui);
            if let Some(pg) = pending_after_clear {
                let already_active = pixel_rt.active_pixel_graphic.as_ref().is_some_and(
                    |(ak, ar, ap)| ak == &pg.cache_key && *ar == pg.rect && *ap == pg.protocol,
                );
                if !already_active {
                    let payload = pixel_rt
                        .decoded_cache
                        .get(&pg.cache_key)
                        .and_then(|r| r.pixel_payload.as_deref())
                        .or_else(|| {
                            app.image_cache
                                .get(&pg.cache_key)
                                .and_then(|r| r.pixel_payload.as_deref())
                        });
                    if let Some(payload) = payload {
                        if pg.protocol == TerminalImageProtocol::Kitty {
                            terminal
                                .backend_mut()
                                .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                            guard.kitty_used = true;
                        }
                        queue!(terminal.backend_mut(), MoveTo(pg.rect.x, pg.rect.y))?;
                        terminal.backend_mut().write_all(payload.as_bytes())?;
                        terminal.backend_mut().flush()?;
                        pixel_rt.set_active_pixel_graphic(
                            ui,
                            Some((pg.cache_key, pg.rect, pg.protocol)),
                        );
                    } else {
                        pixel_rt.set_active_pixel_graphic(ui, None);
                    }
                }
            } else {
                pixel_rt.set_active_pixel_graphic(ui, None);
            }
        }

        if let Some((cx, cy)) = cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }
        Ok(())
    })();

    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_res
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    pixel_rt: &mut PixelPreviewRuntime,
    dirty: &mut bool,
) -> bool {
    match ev {
        CrosstermEvent::Key(key) => {
            if let Some(act) = event::key_to_action(
                key,
                ui.active_tab,
                ui.focus,
                ui.has_overlay(),
                ui.accepts_text_input(),
                &ui.keymap,
            ) {
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return true;
                }
                pixel_rt.sync_overlay_state(app, ui);
            }
        }
        CrosstermEvent::Mouse(mouse) => {
            if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return true;
                }
                pixel_rt.sync_overlay_state(app, ui);
            }
        }
        CrosstermEvent::Paste(text) => {
            *dirty = true;
            if action::dispatch(Action::Paste(text), app, ui) {
                return true;
            }
            pixel_rt.sync_overlay_state(app, ui);
        }
        CrosstermEvent::Resize(w, h) => {
            ui.term_width = w;
            ui.term_height = h;
            pixel_rt.on_resize();
            *dirty = true;
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                let _ = action::dispatch(Action::LoadPreviewImage, app, ui);
                pixel_rt.sync_overlay_state(app, ui);
            }
        }
        _ => {}
    }
    false
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

    let (mut guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    // Probe terminal graphics protocol and cell pixel dimensions once on TUI startup.
    let graphics_caps = img_preview::detect_graphics_caps_auto();
    let mut pixel_rt = PixelPreviewRuntime::new(graphics_caps);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        if let Some((target, initial)) = ui.editor_request.take() {
            pixel_rt.clear_kitty_before_suspend(&mut terminal, &mut ui);
            guard.restore();
            let result = text_edit::external_edit(&initial);
            guard.resume()?;
            terminal.clear()?;
            dirty = true;

            match result {
                Ok(edited) => {
                    apply_external_edit_result(&mut ui, target, edited);
                    ui.success("Updated content from external editor");
                }
                Err(err) => {
                    ui.error(format!("Failed to launch external editor: {err}"));
                }
            }
        }

        if dirty {
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard, &mut pixel_rt)?;
            dirty = false;
        }

        let mut should_quit = false;
        tokio::select! {
            _ = tick.tick() => {
                let before = ui.toasts.len();
                ui.toasts.tick(std::time::Instant::now());
                if ui.toasts.len() != before {
                    dirty = true;
                }
            }
            maybe_core = core_rx.recv() => {
                match maybe_core {
                    Some(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                    }
                    None => should_quit = true,
                }
            }
            maybe_ui = ui_rx.recv() => {
                if let Some(msg) = maybe_ui {
                    app.handle_ui_msg(&mut ui, msg);
                    dirty = true;
                }
            }
            maybe_px = pixel_rt.pixel_rx.recv() => {
                if let Some(msg) = maybe_px {
                    pixel_rt.handle_pixel_msg(&mut app, &mut ui, msg);
                    dirty = true;
                }
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        if handle_terminal_event(ev, &mut app, &mut ui, &mut pixel_rt, &mut dirty) {
                            should_quit = true;
                        }
                    }
                    Some(Err(_)) | None => should_quit = true,
                }
            }
        }
        if should_quit {
            break;
        }

        // Coalesce burst events before redrawing to avoid flickering pixel overlays.
        let mut drained = 0usize;
        while drained < MAX_COALESCED_EVENTS && ui.editor_request.is_none() {
            let mut progressed = false;
            while drained < MAX_COALESCED_EVENTS {
                match core_rx.try_recv() {
                    Ok(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        should_quit = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            if should_quit {
                break;
            }
            while drained < MAX_COALESCED_EVENTS {
                match ui_rx.try_recv() {
                    Ok(msg) => {
                        app.handle_ui_msg(&mut ui, msg);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(_) => break,
                }
            }
            while drained < MAX_COALESCED_EVENTS {
                match pixel_rt.pixel_rx.try_recv() {
                    Ok(msg) => {
                        pixel_rt.handle_pixel_msg(&mut app, &mut ui, msg);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(_) => break,
                }
            }
            while drained < MAX_COALESCED_EVENTS && ui.editor_request.is_none() {
                match events.next().now_or_never() {
                    Some(Some(Ok(ev))) => {
                        drained += 1;
                        progressed = true;
                        if handle_terminal_event(ev, &mut app, &mut ui, &mut pixel_rt, &mut dirty) {
                            should_quit = true;
                            break;
                        }
                    }
                    Some(Some(Err(_))) | Some(None) => {
                        should_quit = true;
                        break;
                    }
                    None => break,
                }
            }
            if should_quit || !progressed {
                break;
            }
        }
        if should_quit {
            break;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AttachmentItem;
    use crate::ui::overlays::image_preview::ImagePreviewOverlay;

    fn make_test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new("test");
        let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Jira,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
        (app, ui)
    }

    #[tokio::test]
    async fn pixel_runtime_tracks_pending_overlay_and_syncs_eviction() {
        let (mut app, mut ui) = make_test_app_and_ui();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let mut rt = PixelPreviewRuntime::new(caps);
        let att = AttachmentItem {
            id: "att-9".into(),
            filename: "diagram.png".into(),
            mime_type: "image/png".into(),
            size: 1024,
            author_name: None,
            created_at: String::new(),
            content_url: String::new(),
            thumbnail_url: None,
        };
        let ov = ImagePreviewOverlay::from_attachments("ATL-9", &[att], None).unwrap();
        ui.push_overlay(Overlay::ImagePreview(ov));
        rt.sync_overlay_state(&mut app, &mut ui);

        let area = Rect::new(0, 0, 100, 30);
        let (cols, rows) = match ui.overlays.last() {
            Some(Overlay::ImagePreview(ov)) => {
                assert_eq!(ov.protocol, TerminalImageProtocol::Kitty);
                ov.target_dimensions(area.width, area.height)
            }
            _ => panic!("expected ImagePreview overlay"),
        };

        let cache_key =
            img_preview::preview_cache_key("att-9", cols, rows, TerminalImageProtocol::Kitty);
        rt.handle_pixel_msg(
            &mut app,
            &mut ui,
            PixelDecodeMsg {
                cache_key: cache_key.clone(),
                att_id: "att-9".into(),
                raw_bytes: Some(Arc::from(vec![1u8, 2, 3, 4])),
                rendered: Ok(RenderedTerminalImage {
                    width_px: 640,
                    height_px: 360,
                    cols: 40,
                    rows: 12,
                    format_label: "PNG".into(),
                    protocol: TerminalImageProtocol::Kitty,
                    lines: Vec::new(),
                    pixel_payload: Some("\x1b_Gf=100,a=T,q=2,i=4242,c=40,r=12,C=1,m=0;AA==\x1b\\".into()),
                }),
            },
        );

        assert!(rt.raw_cache.contains_key("att-9"));
        let pending = current_pending_pixel_graphic(area, &app, &ui)
            .expect("pending pixel graphic should resolve from cache");
        assert_eq!(pending.cache_key, cache_key);
        assert_eq!(pending.protocol, TerminalImageProtocol::Kitty);

        // Closing overlay clears pending pixel graphic
        ui.pop_overlay();
        assert!(current_pending_pixel_graphic(area, &app, &ui).is_none());

        let _ = app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
