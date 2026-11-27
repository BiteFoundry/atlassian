//! Native-grade Atlassian TUI entry shell (`jira` / `confluence` / `conf`):
//! terminal raw mode, mouse capture, keyboard enhancement, synchronized pixel graphics overlay loop,
//! event coalescing, and external editor handoff.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    io::{self, BufWriter, Stdout, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
#[cfg(not(windows))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::{
    cursor::MoveTo,
    event::{
        DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event as CrosstermEvent, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use tokio::sync::mpsc;

use crate::{
    atlassian::AtlassianClient,
    auth::AuthManager,
    config::{AppConfig, Persona},
    img_preview::{
        self, ActivePixelGraphic, DecodedImageCache, ImageProtocolPreference,
        RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
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
const EVENT_READER_POLL: Duration = Duration::from_millis(25);
const MAX_RAW_IMAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RAW_IMAGE_CACHE_ENTRIES: usize = 32;

static KITTY_USED_GLOBAL: AtomicBool = AtomicBool::new(false);

/// VT DECSET sequences for X10 + button-motion + SGR mouse reporting.
/// `crossterm 0.28` skips ANSI mouse codes on Windows, so writing these explicitly
/// ensures ConPTY / Windows Terminal / xterm.js report mouse wheel events cleanly.
pub(crate) const VT_ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(crate) const VT_DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

pub(crate) fn write_vt_mouse_enable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_ENABLE_MOUSE.as_bytes())?;
    w.flush()
}

pub(crate) fn write_vt_mouse_disable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_DISABLE_MOUSE.as_bytes())?;
    w.flush()
}

#[cfg(windows)]
mod win_console {
    use std::ptr::null_mut;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;

    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    pub const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

    /// Raw-mode + ConPTY mouse capture console input mode (`0x0098`):
    /// enables `ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS`,
    /// explicitly clearing `ENABLE_QUICK_EDIT_MODE`, `ENABLE_VIRTUAL_TERMINAL_INPUT`,
    /// `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT`, and `ENABLE_PROCESSED_INPUT`.
    pub const TUI_MOUSE_CAPTURE_MODE: u32 =
        ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
    }

    fn with_conin_handle<T>(f: impl FnOnce(isize) -> Option<T>) -> Option<T> {
        const CONIN_WIDE: [u16; 7] = [
            b'C' as u16,
            b'O' as u16,
            b'N' as u16,
            b'I' as u16,
            b'N' as u16,
            b'$' as u16,
            0,
        ];
        let conin = unsafe {
            CreateFileW(
                CONIN_WIDE.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if conin != 0 && conin != INVALID_HANDLE_VALUE {
            let res = f(conin);
            unsafe {
                CloseHandle(conin);
            }
            return res;
        }
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if std_in != 0 && std_in != INVALID_HANDLE_VALUE {
            f(std_in)
        } else {
            None
        }
    }

    pub fn snapshot_console_mode() -> Option<u32> {
        with_conin_handle(|h| {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                Some(mode)
            } else {
                None
            }
        })
    }

    pub fn enforce_windows_mouse_capture() {
        let _ = with_conin_handle(|h| {
            unsafe {
                SetConsoleMode(h, TUI_MOUSE_CAPTURE_MODE);
            }
            Some(())
        });
    }

    pub fn restore_console_mode(orig_mode: Option<u32>) {
        if let Some(mode) = orig_mode {
            let _ = with_conin_handle(|h| {
                unsafe {
                    SetConsoleMode(h, mode | ENABLE_EXTENDED_FLAGS);
                }
                Some(())
            });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PixelReconcilePlan {
    erase_and_redraw: bool,
    delete_kitty: bool,
    erase_rects: Vec<Rect>,
    emit_desired: bool,
}

#[derive(Debug, Default)]
struct PixelLayerTracker {
    active: Vec<ActivePixelGraphic>,
    last_term_size: (u16, u16),
}

impl PixelLayerTracker {
    fn plan_transition(
        &self,
        desired: &[ActivePixelGraphic],
        term_size: (u16, u16),
        kitty_used: bool,
    ) -> PixelReconcilePlan {
        let term_resized = self.last_term_size != term_size;
        if self.active == desired && !term_resized {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: false,
            };
        }

        // Avoid full `terminal.clear()` flicker when only the `#crop:...` viewport changed
        // on the exact same rect, cell size, and base image for Sixel.
        let sixel_crop_only_update = !term_resized
            && self.active.len() == 1
            && desired.len() == 1
            && self.active[0].protocol == TerminalImageProtocol::Sixel
            && desired[0].protocol == TerminalImageProtocol::Sixel
            && self.active[0].rect == desired[0].rect
            && self.active[0].cols == desired[0].cols
            && self.active[0].rows == desired[0].rows
            && parse_pixel_key(&self.active[0].cache_key).0
                == parse_pixel_key(&desired[0].cache_key).0;

        if sixel_crop_only_update {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: true,
            };
        }

        let had_active = !self.active.is_empty();
        let delete_kitty = had_active
            && (kitty_used
                || self
                    .active
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty));
        let erase_rects = self
            .active
            .iter()
            .map(|prev| {
                Rect::new(
                    prev.rect.x,
                    prev.rect.y,
                    prev.rect.width.max(prev.cols),
                    prev.rect.height.max(prev.rows),
                )
            })
            .collect();

        PixelReconcilePlan {
            erase_and_redraw: had_active,
            delete_kitty,
            erase_rects,
            emit_desired: !desired.is_empty(),
        }
    }

    fn commit(&mut self, desired: Vec<ActivePixelGraphic>, term_size: (u16, u16)) {
        self.active = desired;
        self.last_term_size = term_size;
    }

    fn clear(&mut self) {
        self.active.clear();
    }
}

/// Overlay Z-order guard: blocks inline pixel graphics whenever any modal overlay is active,
/// allows `ImagePreview` modal graphics when it is the topmost overlay, and allows `HtmlPreview`
/// inline graphics (`html:*`) when `HtmlPreview` is the topmost overlay.
fn pixel_graphics_allowed_for_overlays(overlays: &[Overlay], cache_key: &str) -> bool {
    if overlays.is_empty() {
        true
    } else if matches!(overlays.last(), Some(Overlay::ImagePreview(_))) {
        !cache_key.starts_with("inline:") && !cache_key.starts_with("html:")
    } else if matches!(overlays.last(), Some(Overlay::HtmlPreview(_))) {
        cache_key.starts_with("html:")
    } else {
        false
    }
}

fn parse_pixel_key(key: &str) -> (&str, Option<(u16, u16, u16, u16)>) {
    if let Some((base, crop_str)) = key.rsplit_once("#crop:") {
        let mut parts = crop_str.split(',');
        if let (Some(sx), Some(sy), Some(vw), Some(vh), None) = (
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next(),
        ) {
            return (base, Some((sx, sy, vw, vh)));
        }
    }
    (key, None)
}

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
    pixel_tracker: PixelLayerTracker,
    #[cfg(windows)]
    orig_conin_mode: Option<u32>,
}

type TerminalSession = (
    TerminalGuard,
    Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    TerminalGraphicsCaps,
);

type FetchedImageBytes = (Option<Arc<[u8]>>, Result<Arc<[u8]>, String>);

impl TerminalGuard {
    fn enter(image_pref: ImageProtocolPreference) -> Result<TerminalSession> {
        #[cfg(windows)]
        let orig_conin_mode = win_console::snapshot_console_mode();

        enable_raw_mode()?;
        // Probe graphics & cell pixel dimensions while raw mode is active, BEFORE entering
        // alternate screen / mouse capture so Windows VT probe never clobbers ConPTY mouse mode.
        let graphics_caps = img_preview::probe_terminal_graphics_caps(image_pref);

        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange,
            EnableBracketedPaste
        )?;
        #[cfg(windows)]
        {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableFocusChange
            )?;
            write_vt_mouse_enable(&mut stdout)?;
        }

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

        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
        }

        let backend = CrosstermBackend::new(BufWriter::with_capacity(64 * 1024, stdout));
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
                pixel_tracker: PixelLayerTracker::default(),
                #[cfg(windows)]
                orig_conin_mode,
            },
            terminal,
            graphics_caps,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used || KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(
                stdout,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(self.orig_conin_mode);
    }

    fn resume(&self) -> Result<()> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange,
            EnableBracketedPaste
        )?;
        #[cfg(windows)]
        {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableFocusChange
            )?;
            write_vt_mouse_enable(&mut stdout)?;
        }
        if self.keyboard_enhanced {
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            );
        }
        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool, orig_conin_mode: Option<u32>) {
    #[cfg(not(windows))]
    let _ = orig_conin_mode;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(
                stdout,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(orig_conin_mode);
        prev(info);
    }));
}

enum ReaderMsg {
    Event(CrosstermEvent),
    Eof,
}

/// Lock-free terminal event reader backed by a dedicated OS thread and Tokio unbounded channel.
///
/// Avoids `crossterm::event::EventStream::next().now_or_never()`, which on `crossterm 0.28`
/// poisons `EventStream`'s background waker with `noop_waker` (causing up to 200ms timer-tick
/// input stalls) and blocks the main thread on `INTERNAL_EVENT_READER` on Windows until the next
/// console event arrives.
struct TerminalEventReader {
    rx: mpsc::UnboundedReceiver<ReaderMsg>,
    tx: mpsc::UnboundedSender<ReaderMsg>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TerminalEventReader {
    fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut reader = Self {
            rx,
            tx,
            stop,
            handle: None,
        };
        reader.resume();
        reader
    }

    #[cfg(test)]
    fn from_channel(
        tx: mpsc::UnboundedSender<ReaderMsg>,
        rx: mpsc::UnboundedReceiver<ReaderMsg>,
    ) -> Self {
        Self {
            rx,
            tx,
            stop: Arc::new(AtomicBool::new(true)),
            handle: None,
        }
    }

    fn resume(&mut self) {
        if self.handle.is_some() {
            return;
        }
        self.stop.store(false, Ordering::Release);
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        self.handle = std::thread::Builder::new()
            .name("tui-event-reader".to_string())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match crossterm::event::poll(EVENT_READER_POLL) {
                        Ok(true) => match crossterm::event::read() {
                            Ok(ev) => {
                                if tx.send(ReaderMsg::Event(ev)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(ReaderMsg::Eof);
                                break;
                            }
                        },
                        Ok(false) => continue,
                        Err(_) => {
                            let _ = tx.send(ReaderMsg::Eof);
                            break;
                        }
                    }
                }
            })
            .ok();
    }

    fn pause(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    async fn recv(&mut self) -> Option<CrosstermEvent> {
        match self.rx.recv().await {
            Some(ReaderMsg::Event(ev)) => Some(ev),
            Some(ReaderMsg::Eof) | None => None,
        }
    }

    fn try_recv(&mut self) -> std::result::Result<CrosstermEvent, mpsc::error::TryRecvError> {
        match self.rx.try_recv() {
            Ok(ReaderMsg::Event(ev)) => Ok(ev),
            Ok(ReaderMsg::Eof) => Err(mpsc::error::TryRecvError::Disconnected),
            Err(err) => Err(err),
        }
    }
}

impl Drop for TerminalEventReader {
    fn drop(&mut self) {
        self.pause();
    }
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

        let fit = ov.fit_box(ui.term_width, ui.term_height);
        let att_id = ov.cache_id(&att);
        let cache_key = ov.preview_image_cache_key(&att, ui.term_width, ui.term_height);

        if let Some(cached) = self.decoded_cache.get(&cache_key) {
            app.pixel_image_cache
                .entry(cache_key)
                .or_insert_with(|| cached.clone());
            return;
        }

        if app.pixel_image_cache.contains_key(&cache_key)
            || self.in_flight.contains(&cache_key)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }

        self.in_flight.insert(cache_key.clone());
        let cached_raw = self.raw_cache.get(&att_id).cloned();
        let (_, client) = app.site_id_and_client_for(ov.site_id.as_deref());
        let tx = self.pixel_tx.clone();
        let ui_tx = app.ui_tx.clone();
        let caps = ov.graphics_caps();
        let bg_rgb = theme_preview_bg_rgb(&ui.theme);

        tokio::spawn(async move {
            let (raw_opt, bytes_res): FetchedImageBytes =
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

            if let Ok(ref bytes) = bytes_res {
                let mime = (!att.mime_type.trim().is_empty()).then_some(att.mime_type.as_str());
                let fname = (!att.filename.trim().is_empty()).then_some(att.filename.as_str());
                if bitefoundry_html::sniff_content_kind(mime, fname, Some(bytes))
                    == bitefoundry_html::ContentKind::Html
                {
                    let html = String::from_utf8_lossy(bytes).into_owned();
                    let _ = ui_tx.send(UiMsg::PreviewImageSniffedAsHtml {
                        title: att.filename.clone(),
                        source_url: att.content_url.clone(),
                        html,
                    });
                    return;
                }
            }

            let rendered = match bytes_res {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    img_preview::decode_and_render_for_protocol(&bytes, fit, bg_rgb, caps)
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
                    app.pixel_image_cache.remove(&old_key);
                }
                app.pixel_image_cache.insert(msg.cache_key, img);
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

    fn clear_graphics_before_suspend(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<BufWriter<Stdout>>>,
        ui: &mut UiState,
        guard: &mut TerminalGuard,
    ) {
        let plan = guard
            .pixel_tracker
            .plan_transition(&[], (ui.term_width, ui.term_height), guard.kitty_used);
        if plan.delete_kitty
            || self
                .active_pixel_graphic
                .as_ref()
                .is_some_and(|(_, _, proto)| *proto == TerminalImageProtocol::Kitty)
        {
            let _ = terminal
                .backend_mut()
                .write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = terminal
                .backend_mut()
                .write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            guard.kitty_used = false;
        }
        for rect in &plan.erase_rects {
            let erase = img_preview::erase_pixel_rect_sequence(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                ui.term_width,
                ui.term_height,
            );
            if !erase.is_empty() {
                let _ = terminal.backend_mut().write_all(erase.as_bytes());
            }
        }
        let _ = terminal.backend_mut().flush();
        guard.pixel_tracker.clear();
        self.set_active_pixel_graphic(ui, None);
    }
}

fn theme_preview_bg_rgb(theme: &ui::theme::Theme) -> (u8, u8, u8) {
    img_preview::color_to_rgb(theme.bg_elevated, img_preview::DEFAULT_PREVIEW_BG_RGB)
}

fn current_pending_pixel_graphic(
    frame_area: Rect,
    app: &AppState,
    ui: &UiState,
) -> Option<PendingPixelGraphic> {
    let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
        return None;
    };
    ov.pending_pixel_graphic(frame_area, app)
}

fn resolve_pixel_payload<'a>(
    cache_key: &str,
    pixel_rt: &'a PixelPreviewRuntime,
    app: &'a AppState,
) -> Option<(Cow<'a, str>, u16, u16)> {
    let (base_key, crop) = parse_pixel_key(cache_key);
    let rendered = pixel_rt
        .decoded_cache
        .get(base_key)
        .or_else(|| app.pixel_image_cache.get(base_key))?;
    if let Some((sx, sy, vw, vh)) = crop {
        rendered
            .viewport_pixel_payload(sx, sy, vw, vh)
            .map(|(p, c, r)| (Cow::Owned(p), c, r))
            .or_else(|| {
                if rendered.cols <= vw && rendered.rows <= vh {
                    rendered
                        .pixel_payload
                        .as_deref()
                        .map(|p| (Cow::Borrowed(p), rendered.cols, rendered.rows))
                } else {
                    None
                }
            })
    } else {
        rendered
            .pixel_payload
            .as_deref()
            .map(|p| (Cow::Borrowed(p), rendered.cols, rendered.rows))
    }
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
    pixel_rt: &mut PixelPreviewRuntime,
) -> Result<()> {
    pixel_rt.sync_overlay_state(app, ui);

    let _ = queue!(terminal.backend_mut(), BeginSynchronizedUpdate);

    let draw_res: Result<()> = (|| {
        let mut cursor_pos = None;
        let mut cursor_set_in_draw = false;
        let mut frame_area = Rect::default();
        terminal.draw(|frame| {
            frame_area = frame.area();
            cursor_pos = ui::draw(frame, app, ui);
            if ui.pending_pixel_graphics.is_empty()
                && current_pending_pixel_graphic(frame_area, app, ui).is_none()
            {
                if let Some((cx, cy)) = cursor_pos {
                    frame.set_cursor_position((cx, cy));
                    cursor_set_in_draw = true;
                }
            }
        })?;

        let mut desired: Vec<ActivePixelGraphic> = Vec::new();
        let mut desired_payloads: Vec<String> = Vec::new();

        let pending_list: Vec<PendingPixelGraphic> = if !ui.pending_pixel_graphics.is_empty() {
            ui.pending_pixel_graphics.clone()
        } else if let Some(pg) = current_pending_pixel_graphic(frame_area, app, ui) {
            vec![pg]
        } else {
            Vec::new()
        };

        let safe_bottom = ui.term_height.saturating_sub(1);
        for pg in pending_list {
            if !pixel_graphics_allowed_for_overlays(&ui.overlays, &pg.cache_key) {
                continue;
            }
            if pg.rect.y >= safe_bottom || pg.rect.x >= ui.term_width {
                continue;
            }
            let resolved = if let Some(inline_payload) = pg.payload {
                Some((Cow::Owned(inline_payload), pg.rect.width, pg.rect.height))
            } else {
                resolve_pixel_payload(&pg.cache_key, pixel_rt, app)
            };
            if let Some((payload, draw_cols, draw_rows)) = resolved {
                let clamped_w = draw_cols.min(ui.term_width.saturating_sub(pg.rect.x));
                let clamped_h = draw_rows.min(safe_bottom.saturating_sub(pg.rect.y));
                if clamped_w > 0 && clamped_h > 0 {
                    desired.push(ActivePixelGraphic {
                        cache_key: pg.cache_key,
                        rect: Rect::new(pg.rect.x, pg.rect.y, clamped_w, clamped_h),
                        protocol: pg.protocol,
                        cols: clamped_w,
                        rows: clamped_h,
                        payload_len: payload.len(),
                    });
                    desired_payloads.push(payload.into_owned());
                }
            }
        }

        let term_size = (ui.term_width, ui.term_height);
        let plan = guard
            .pixel_tracker
            .plan_transition(&desired, term_size, guard.kitty_used);

        if plan.erase_and_redraw {
            if plan.delete_kitty {
                terminal
                    .backend_mut()
                    .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                terminal
                    .backend_mut()
                    .write_all(img_preview::kitty_delete_all_sequence().as_bytes())?;
            }
            for rect in &plan.erase_rects {
                let erase = img_preview::erase_pixel_rect_sequence(
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    ui.term_width,
                    ui.term_height,
                );
                if !erase.is_empty() {
                    terminal.backend_mut().write_all(erase.as_bytes())?;
                }
            }
            terminal.backend_mut().flush()?;
            terminal.clear()?;
            pixel_rt.set_active_pixel_graphic(ui, None);
            terminal.draw(|frame| {
                frame_area = frame.area();
                cursor_pos = ui::draw(frame, app, ui);
            })?;
        }

        if plan.emit_desired {
            if !desired.is_empty() {
                let any_kitty = desired
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty);
                if any_kitty {
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_all_sequence().as_bytes())?;
                    guard.kitty_used = true;
                    KITTY_USED_GLOBAL.store(true, Ordering::Relaxed);
                }
                for (idx, (graphic, payload)) in
                    desired.iter().zip(desired_payloads.iter()).enumerate()
                {
                    queue!(
                        terminal.backend_mut(),
                        MoveTo(graphic.rect.x, graphic.rect.y)
                    )?;
                    if graphic.protocol == TerminalImageProtocol::Kitty && idx > 0 {
                        let unique_id =
                            img_preview::KITTY_PREVIEW_IMAGE_ID.saturating_add(idx as u32);
                        let patched = payload.replacen(
                            &format!(",i={},", img_preview::KITTY_PREVIEW_IMAGE_ID),
                            &format!(",i={unique_id},"),
                            1,
                        );
                        terminal.backend_mut().write_all(patched.as_bytes())?;
                    } else {
                        terminal.backend_mut().write_all(payload.as_bytes())?;
                    }
                }
                terminal.backend_mut().flush()?;
                if let Some(first) = desired.first() {
                    pixel_rt.set_active_pixel_graphic(
                        ui,
                        Some((first.cache_key.clone(), first.rect, first.protocol)),
                    );
                }
            } else {
                pixel_rt.set_active_pixel_graphic(ui, None);
            }
        } else if plan.delete_kitty {
            guard.kitty_used = false;
            pixel_rt.set_active_pixel_graphic(ui, None);
        } else if desired.is_empty() {
            pixel_rt.set_active_pixel_graphic(ui, None);
        }

        guard.pixel_tracker.commit(desired, term_size);

        if plan.erase_and_redraw || plan.emit_desired || !cursor_set_in_draw {
            if let Some((cx, cy)) = cursor_pos {
                let _ = terminal.show_cursor();
                let _ = terminal.set_cursor_position((cx, cy));
            } else {
                let _ = terminal.hide_cursor();
            }
        }
        Ok(())
    })();

    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_res
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermEventOutcome {
    Continue,
    /// Layout, tab, overlay, or mouse-click target changed; redraw immediately so
    /// `ui.hitmap` is rebuilt before any subsequent queued mouse event is processed.
    RedrawNow,
    /// Terminal dimensions changed; re-probe `cell_px` safely (pausing `TerminalEventReader` on Windows)
    /// and reload any active image preview before redrawing.
    Resized,
    Quit,
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    pixel_rt: &mut PixelPreviewRuntime,
    dirty: &mut bool,
) -> TermEventOutcome {
    let prev_tab = ui.active_tab;
    let prev_overlays = ui.overlays.len();
    let prev_nav_rail = ui.show_nav_rail;
    let prev_drawer = ui.board_ui.drawer_open;
    let prev_toc = ui.confluence_ui.toc_open;
    let mut force_redraw = false;

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
                    return TermEventOutcome::Quit;
                }
                pixel_rt.sync_overlay_state(app, ui);
            }
        }
        CrosstermEvent::Mouse(mouse) => {
            if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                if matches!(
                    act,
                    Action::MouseClick { .. } | Action::MouseRightClick { .. }
                ) {
                    force_redraw = true;
                }
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return TermEventOutcome::Quit;
                }
                pixel_rt.sync_overlay_state(app, ui);
            }
        }
        CrosstermEvent::Paste(text) => {
            *dirty = true;
            if action::dispatch(Action::Paste(text), app, ui) {
                return TermEventOutcome::Quit;
            }
            pixel_rt.sync_overlay_state(app, ui);
        }
        CrosstermEvent::Resize(w, h) => {
            ui.term_width = w;
            ui.term_height = h;
            *dirty = true;
            return TermEventOutcome::Resized;
        }
        CrosstermEvent::FocusGained => {
            app.handle_focus_gained(ui);
        }
        _ => {}
    }

    let layout_changed = ui.active_tab != prev_tab
        || ui.overlays.len() != prev_overlays
        || ui.show_nav_rail != prev_nav_rail
        || ui.board_ui.drawer_open != prev_drawer
        || ui.confluence_ui.toc_open != prev_toc;
    if force_redraw || layout_changed {
        TermEventOutcome::RedrawNow
    } else {
        TermEventOutcome::Continue
    }
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

    let sync_ctrl = match crate::atlassian::SiteClientPool::from_config(&config) {
        Ok(pool) if pool.enabled_clients().len() > 1 => {
            let targets = sync::sync_targets(&pool);
            sync::spawn_multi_site_sync_engine(
                config.clone(),
                targets,
                storage.clone(),
                sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                core_tx,
            )
        }
        _ => sync::spawn_sync_engine(config.clone(), client.clone(), storage.clone(), core_tx),
    };

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
    app.apply_default_selection(&mut ui);
    app.spawn_bootstrap(&mut ui);
    app.sync_open_item_watch(&ui);

    let img_pref = ui::app::config_protocol_to_img_pref(app.config.image_protocol);
    let (mut guard, mut terminal, graphics_caps) = TerminalGuard::enter(img_pref)?;
    #[cfg(windows)]
    let orig_conin_mode = guard.orig_conin_mode;
    #[cfg(not(windows))]
    let orig_conin_mode = None;
    install_panic_hook(guard.keyboard_enhanced, orig_conin_mode);

    ui.graphics_caps = graphics_caps;
    let mut pixel_rt = PixelPreviewRuntime::new(graphics_caps);

    let mut events = TerminalEventReader::start();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        if let Some((target, initial)) = ui.editor_request.take() {
            events.pause();
            pixel_rt.clear_graphics_before_suspend(&mut terminal, &mut ui, &mut guard);
            guard.restore();
            let result = text_edit::external_edit(&initial);
            guard.resume()?;
            terminal.clear()?;
            events.resume();
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
            app.sync_open_item_watch(&ui);
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard, &mut pixel_rt)?;
            dirty = false;
        }

        let mut should_quit = false;
        let mut needs_immediate_redraw = false;
        let mut needs_resize_probe = false;
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
            maybe_ev = events.recv() => {
                match maybe_ev {
                    Some(ev) => match handle_terminal_event(ev, &mut app, &mut ui, &mut pixel_rt, &mut dirty) {
                        TermEventOutcome::Quit => should_quit = true,
                        TermEventOutcome::RedrawNow => needs_immediate_redraw = true,
                        TermEventOutcome::Resized => {
                            needs_immediate_redraw = true;
                            needs_resize_probe = true;
                        }
                        TermEventOutcome::Continue => {}
                    },
                    None => should_quit = true,
                }
            }
        }
        if should_quit {
            break;
        }

        // Coalesce burst events before redrawing to avoid flickering pixel overlays,
        // but stop immediately after any mouse click or tab/overlay/layout change so
        // the next mouse event never queries a stale hitmap.
        let mut drained = 0usize;
        while drained < MAX_COALESCED_EVENTS
            && ui.editor_request.is_none()
            && !needs_immediate_redraw
        {
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
            while drained < MAX_COALESCED_EVENTS
                && ui.editor_request.is_none()
                && !needs_immediate_redraw
            {
                match events.try_recv() {
                    Ok(ev) => {
                        drained += 1;
                        progressed = true;
                        match handle_terminal_event(ev, &mut app, &mut ui, &mut pixel_rt, &mut dirty) {
                            TermEventOutcome::Quit => {
                                should_quit = true;
                                break;
                            }
                            TermEventOutcome::RedrawNow => {
                                needs_immediate_redraw = true;
                                break;
                            }
                            TermEventOutcome::Resized => {
                                needs_immediate_redraw = true;
                                needs_resize_probe = true;
                                break;
                            }
                            TermEventOutcome::Continue => {}
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        should_quit = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            if should_quit || needs_immediate_redraw || !progressed {
                break;
            }
        }
        if should_quit {
            break;
        }
        if needs_resize_probe {
            #[cfg(windows)]
            {
                events.pause();
                let _ = img_preview::refresh_cell_pixel_size(&mut pixel_rt.graphics_caps);
                win_console::enforce_windows_mouse_capture();
                let _ = write_vt_mouse_enable(terminal.backend_mut());
                events.resume();
            }
            #[cfg(not(windows))]
            {
                let _ = img_preview::refresh_cell_pixel_size(&mut pixel_rt.graphics_caps);
            }
            ui.graphics_caps = pixel_rt.graphics_caps;
            let has_img_preview = if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last_mut() {
                ov.apply_graphics_caps(pixel_rt.graphics_caps);
                true
            } else {
                false
            };
            if has_img_preview {
                let _ = action::dispatch(Action::LoadPreviewImage, &mut app, &mut ui);
                pixel_rt.sync_overlay_state(&mut app, &mut ui);
            }
        }
    }

    app.html_server.shutdown();
    let _ = sync_ctrl.request(SyncCommand::Shutdown);
    events.pause();
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
        let cache_key = match ui.overlays.last() {
            Some(Overlay::ImagePreview(ov)) => {
                assert_eq!(ov.protocol, TerminalImageProtocol::Kitty);
                let cur = ov.current().unwrap();
                ov.preview_image_cache_key(cur, area.width, area.height)
            }
            _ => panic!("expected ImagePreview overlay"),
        };
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
                    scroll_backing: None,
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

    #[cfg(windows)]
    #[test]
    fn test_windows_mouse_capture_mode_bits() {
        // 0x0098 = ENABLE_WINDOW_INPUT (0x0008) | ENABLE_MOUSE_INPUT (0x0010) | ENABLE_EXTENDED_FLAGS (0x0080)
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE, 0x0098);
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0040, 0); // QuickEdit cleared
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0200, 0); // VTInput cleared
    }

    #[test]
    fn test_vt_mouse_enable_and_disable_ansi_sequences() {
        let mut enable_buf = Vec::new();
        write_vt_mouse_enable(&mut enable_buf).unwrap();
        assert_eq!(
            String::from_utf8(enable_buf).unwrap(),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );

        let mut disable_buf = Vec::new();
        write_vt_mouse_disable(&mut disable_buf).unwrap();
        assert_eq!(
            String::from_utf8(disable_buf).unwrap(),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );
    }

    #[test]
    fn test_parse_pixel_key_and_pixel_layer_tracker_erase_transitions() {
        assert_eq!(
            parse_pixel_key("att-1:80x120:sixel#crop:2,10,60,20"),
            ("att-1:80x120:sixel", Some((2, 10, 60, 20)))
        );
        assert_eq!(
            parse_pixel_key("att-1:80x120:sixel"),
            ("att-1:80x120:sixel", None)
        );

        let mut tracker = PixelLayerTracker::default();
        let img_a = ActivePixelGraphic {
            cache_key: "att-1:80x120:sixel#crop:0,0,60,20".into(),
            rect: Rect::new(10, 4, 60, 20),
            protocol: TerminalImageProtocol::Sixel,
            cols: 60,
            rows: 20,
            payload_len: 4096,
        };
        tracker.commit(vec![img_a.clone()], (120, 36));

        // Same graphic & size -> no erase, no re-emit
        let same = tracker.plan_transition(std::slice::from_ref(&img_a), (120, 36), false);
        assert!(!same.erase_and_redraw);
        assert!(!same.emit_desired);

        // Sixel scroll crop on same rect -> re-emit without full clear flicker
        let scrolled = ActivePixelGraphic {
            cache_key: "att-1:80x120:sixel#crop:0,4,60,20".into(),
            ..img_a.clone()
        };
        let crop_plan = tracker.plan_transition(&[scrolled], (120, 36), false);
        assert!(!crop_plan.erase_and_redraw);
        assert!(crop_plan.emit_desired);

        // Closing preview / switching away -> erase_and_redraw with previous rect
        let close_plan = tracker.plan_transition(&[], (120, 36), false);
        assert!(close_plan.erase_and_redraw);
        assert_eq!(close_plan.erase_rects, vec![img_a.rect]);
        assert!(!close_plan.emit_desired);

        // Terminal resize -> erase_and_redraw + re-emit
        let resize_plan = tracker.plan_transition(&[img_a], (130, 36), false);
        assert!(resize_plan.erase_and_redraw);
        assert!(resize_plan.emit_desired);

        // Overlay Z-order guard: modal stacked above ImagePreview blocks pixels
        let att = AttachmentItem {
            id: "1".into(),
            filename: "a.png".into(),
            mime_type: "image/png".into(),
            size: 100,
            author_name: None,
            created_at: String::new(),
            content_url: String::new(),
            thumbnail_url: None,
        };
        let modal = ImagePreviewOverlay::from_attachments("ATL-1", &[att], None).unwrap();
        assert!(pixel_graphics_allowed_for_overlays(
            &[Overlay::ImagePreview(modal.clone())],
            "1:80x24:sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &[Overlay::ImagePreview(modal), Overlay::Help],
            "1:80x24:sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &[Overlay::command_palette()],
            "inline:1"
        ));
    }

    #[tokio::test]
    async fn terminal_event_reader_try_recv_is_non_blocking_and_wakes_immediately() {
        use std::time::Instant;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, rx) = mpsc::unbounded_channel();
        let mut reader = TerminalEventReader::from_channel(tx.clone(), rx);

        // Empty queue returns immediately without blocking.
        let t0 = Instant::now();
        assert!(matches!(
            reader.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(t0.elapsed() < Duration::from_millis(5));

        // Async recv wakes immediately on single keystroke without waiting for any tick.
        let tx_bg = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx_bg.send(ReaderMsg::Event(CrosstermEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            ))));
        });
        let t1 = Instant::now();
        let ev = tokio::time::timeout(Duration::from_millis(100), reader.recv())
            .await
            .expect("recv must wake immediately");
        assert!(matches!(
            ev,
            Some(CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Char('a'),
                ..
            }))
        ));
        assert!(t1.elapsed() < Duration::from_millis(80));

        // Subsequent try_recv does not poison future async recv wakers.
        assert!(matches!(
            reader.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let _ = tx.send(ReaderMsg::Event(CrosstermEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        ))));
        let ev2 = tokio::time::timeout(Duration::from_millis(50), reader.recv())
            .await
            .expect("second recv must wake immediately after try_recv");
        assert!(matches!(
            ev2,
            Some(CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Char('b'),
                ..
            }))
        ));

        // Explicit EOF reports clean termination even while sender handle exists.
        let _ = tx.send(ReaderMsg::Eof);
        assert!(reader.recv().await.is_none());
    }
}
