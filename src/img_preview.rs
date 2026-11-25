//! Inline terminal image preview decoder & renderer using `image` (PNG/JPEG/GIF/WebP) ->
//! Truecolor Unicode Half-Blocks (`▀` U+2580) for `ratatui` + Kitty / iTerm2 / Sixel / ANSI Half-Block rendering for TUI & CLI.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Cursor, IsTerminal, Write as IoWrite};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

const ENV_PROTOCOL_KEYS: &[&str] = &["ATLASSIAN_IMAGE_PROTOCOL"];
const ENV_CELL_PX_KEYS: &[&str] = &["ATLASSIAN_CELL_PIXEL_SIZE"];

/// Default dark terminal background (`#1E1F22`) used for alpha compositing in TUI half-blocks.
pub const DEFAULT_PREVIEW_BG_RGB: (u8, u8, u8) = (0x1E, 0x1F, 0x22);

/// Upper memory budget (8 MiB) for decoded terminal inline image caches.
pub const MAX_IMAGE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum entry count for [`DecodedImageCache`].
pub const DEFAULT_MAX_IMAGE_CACHE_ENTRIES: usize = 64;

/// Fixed Kitty graphics image ID used by the TUI preview overlay so prior placements can be cleanly deleted.
pub const KITTY_PREVIEW_IMAGE_ID: u32 = 4242;

/// HiDPI-aware fallback cell pixel dimensions when neither env nor terminal queries provide an exact value.
#[cfg(windows)]
pub const DEFAULT_FALLBACK_CELL_PX: (u16, u16) = (10, 20);
#[cfg(not(windows))]
pub const DEFAULT_FALLBACK_CELL_PX: (u16, u16) = (8, 16);

/// Memory safety bounds for rendered pixel images (supports 4K/HiDPI and up to 300% zoom).
pub const MAX_RENDER_PX_W: u32 = 4096;
pub const MAX_RENDER_PX_H: u32 = 8192;
/// Proportional shrink ceiling for Sixel raster buffers (4 megapixels).
pub const MAX_SIXEL_PIXELS: u64 = 4_000_000;
/// Bounded color-lookup cache ceiling during Sixel error-diffusion quantization.
pub const MAX_QUANT_LOOKUP_CACHE: usize = 16_384;
const MAX_ITERM2_RAW_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Explicit viewport, zoom, and scrollability sizing contract for terminal image rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FitBox {
    pub view_cols: u16,
    pub view_rows: u16,
    pub zoom_pct: u16,
    pub scrollable: bool,
}

impl FitBox {
    pub fn fit(view_cols: u16, view_rows: u16) -> Self {
        Self {
            view_cols,
            view_rows,
            zoom_pct: 100,
            scrollable: false,
        }
    }

    pub fn scrollable(view_cols: u16, view_rows: u16, zoom_pct: u16) -> Self {
        Self {
            view_cols,
            view_rows,
            zoom_pct,
            scrollable: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyFitAxis {
    Cols(u16),
    Rows(u16),
}

/// Converts a Ratatui [`Color`] to an `(r, g, b)` triple, falling back to `fallback` for `Color::Reset`.
pub fn color_to_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(idx) => xterm_256_to_rgb(idx),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (229, 229, 229),
        Color::DarkGray => (102, 102, 102),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (255, 255, 255),
        Color::Reset => fallback,
    }
}

fn xterm_256_to_rgb(idx: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if idx < 16 {
        ANSI16[idx as usize]
    } else if idx < 232 {
        let v = idx - 16;
        let r = CUBE[(v / 36) as usize];
        let g = CUBE[((v % 36) / 6) as usize];
        let b = CUBE[(v % 6) as usize];
        (r, g, b)
    } else {
        let gray = 8 + (idx - 232) * 10;
        (gray, gray, gray)
    }
}

/// User or configuration preference for terminal inline image protocol selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }
}

/// Active terminal graphics protocol used for rendering inline images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalImageProtocol {
    Kitty,
    ITerm2,
    Sixel,
    Halfblock,
}

impl TerminalImageProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::ITerm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Kitty => "Kitty",
            Self::ITerm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }

    pub fn is_pixel(self) -> bool {
        !matches!(self, Self::Halfblock)
    }

    pub fn next_manual(self) -> Self {
        match self {
            Self::Kitty => Self::ITerm2,
            Self::ITerm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Kitty,
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::ITerm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }
}

/// Legacy stdout graphics protocol enum retained for backwards compatibility with existing callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalGraphicsProtocol {
    Kitty,
    ITerm2,
    Sixel,
    HalfBlocks,
}

impl From<TerminalImageProtocol> for TerminalGraphicsProtocol {
    fn from(proto: TerminalImageProtocol) -> Self {
        match proto {
            TerminalImageProtocol::Kitty => Self::Kitty,
            TerminalImageProtocol::ITerm2 => Self::ITerm2,
            TerminalImageProtocol::Sixel => Self::Sixel,
            TerminalImageProtocol::Halfblock => Self::HalfBlocks,
        }
    }
}

impl From<TerminalGraphicsProtocol> for TerminalImageProtocol {
    fn from(proto: TerminalGraphicsProtocol) -> Self {
        match proto {
            TerminalGraphicsProtocol::Kitty => Self::Kitty,
            TerminalGraphicsProtocol::ITerm2 => Self::ITerm2,
            TerminalGraphicsProtocol::Sixel => Self::Sixel,
            TerminalGraphicsProtocol::HalfBlocks => Self::Halfblock,
        }
    }
}

/// Probed terminal graphics capabilities (resolved protocol and cell pixel dimensions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGraphicsCaps {
    pub protocol: TerminalImageProtocol,
    pub cell_px: (u16, u16),
    pub cell_px_exact: bool,
}

impl Default for TerminalGraphicsCaps {
    fn default() -> Self {
        Self {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: DEFAULT_FALLBACK_CELL_PX,
            cell_px_exact: false,
        }
    }
}

/// Re-probes terminal cell pixel dimensions on resize while preserving the active protocol.
/// Returns `true` if `caps.cell_px` or `caps.cell_px_exact` changed.
///
/// - On Unix, queries `TIOCGWINSZ` synchronously via `crossterm::terminal::window_size()`.
/// - On Windows, falls back to a bounded `CSI 16t/14t + DA1` terminal probe when `window_size()`
///   returns `0x0`; callers with an active `EventStream` must drop it first and restore mouse
///   capture afterwards.
pub fn refresh_cell_pixel_size(caps: &mut TerminalGraphicsCaps) -> bool {
    if let Some(parsed) = resolve_cell_pixel_size_with(|k| std::env::var(k).ok()) {
        let changed = caps.cell_px != parsed || !caps.cell_px_exact;
        caps.cell_px = parsed;
        caps.cell_px_exact = true;
        return changed;
    }

    if let Ok(ws) = crossterm::terminal::window_size() {
        if let Some(new_px) = compute_rounded_cell_px(ws.width, ws.height, ws.columns, ws.rows) {
            let changed = caps.cell_px != new_px || !caps.cell_px_exact;
            caps.cell_px = new_px;
            caps.cell_px_exact = true;
            return changed;
        }
    }

    #[cfg(windows)]
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let reply = write_and_read_terminal_probe("\x1b[16t\x1b[14t\x1b[c", PROBE_TIMEOUT);
        if !reply.is_empty() {
            let reply_bytes = reply.as_bytes();
            let probed = parse_csi_cell_size_reply(reply_bytes).or_else(|| {
                let (win_w, win_h) = parse_csi_win_size_reply(reply_bytes)?;
                let (cols, rows) = crossterm::terminal::size().ok()?;
                compute_rounded_cell_px(win_w, win_h, cols, rows)
            });
            if let Some(new_px) = probed {
                let changed = caps.cell_px != new_px || !caps.cell_px_exact;
                caps.cell_px = new_px;
                caps.cell_px_exact = true;
                return changed;
            }
        }
    }

    false
}

#[derive(Debug, Clone)]
pub(crate) enum PixelScrollBacking {
    SixelIndexed {
        width: u32,
        height: u32,
        cell_px: (u16, u16),
        palette: Vec<(u8, u8, u8)>,
        indexed: Arc<Vec<u8>>,
    },
    Source {
        image: Arc<DynamicImage>,
        target_px_w: u32,
        target_px_h: u32,
        cell_px: (u16, u16),
    },
}

impl PixelScrollBacking {
    fn approx_heap_bytes(&self) -> usize {
        match self {
            Self::SixelIndexed {
                palette, indexed, ..
            } => palette.capacity() * std::mem::size_of::<(u8, u8, u8)>() + indexed.capacity(),
            Self::Source { image, .. } => (image.width() as usize)
                .saturating_mul(image.height() as usize)
                .saturating_mul(image.color().bytes_per_pixel() as usize),
        }
    }
}

/// Decoded & protocol-encoded terminal image ready for `ratatui` or synchronized pixel emission.
#[derive(Debug, Clone)]
pub struct RenderedTerminalImage {
    pub width_px: u32,
    pub height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub format_label: String,
    pub protocol: TerminalImageProtocol,
    pub lines: Vec<Line<'static>>,
    /// Pre-encoded escape sequence payload when `protocol.is_pixel()` is true (positioned by caller in TUI).
    pub pixel_payload: Option<String>,
    /// Optional scrollable pixel backing used to generate viewport-cropped slices on scroll.
    pub(crate) scroll_backing: Option<PixelScrollBacking>,
}

impl RenderedTerminalImage {
    /// Approximate heap footprint of this rendered terminal image in bytes.
    pub fn approx_heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>()
            + self.format_label.capacity()
            + self.lines.capacity() * std::mem::size_of::<Line<'static>>()
            + self.pixel_payload.as_ref().map_or(0, |s| s.capacity())
            + self
                .scroll_backing
                .as_ref()
                .map_or(0, PixelScrollBacking::approx_heap_bytes);
        for line in &self.lines {
            total += line.spans.capacity() * std::mem::size_of::<Span<'static>>();
            for span in &line.spans {
                total += span.content.len();
            }
        }
        total
    }

    /// Returns a viewport-cropped pixel protocol payload `(payload, draw_cols, draw_rows)` guaranteed
    /// to fit within `(view_cols, view_rows)` cells starting at cell offset `(scroll_x, scroll_y)`.
    pub fn viewport_pixel_payload(
        &self,
        scroll_x: u16,
        scroll_y: u16,
        view_cols: u16,
        view_rows: u16,
    ) -> Option<(String, u16, u16)> {
        if !self.protocol.is_pixel() || view_cols == 0 || view_rows == 0 {
            return None;
        }
        if scroll_x == 0 && scroll_y == 0 && self.cols <= view_cols && self.rows <= view_rows {
            return Some((self.pixel_payload.clone()?, self.cols, self.rows));
        }

        let backing = self.scroll_backing.as_ref()?;
        match backing {
            PixelScrollBacking::SixelIndexed {
                width,
                height,
                cell_px,
                palette,
                indexed,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *width || y0 >= *height {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let raw_max_h = (view_rows as u32).saturating_mul(cell_h);
                let sixel_safe_h = compute_sixel_safe_pixel_height(view_rows, cell_px.1.max(1));
                let max_h = if sixel_safe_h >= 6 {
                    sixel_safe_h.min(raw_max_h)
                } else {
                    raw_max_h
                };
                let crop_w = (*width - x0).min(max_w);
                let mut crop_h = (*height - y0).min(max_h);
                // Sixel emits 6-pixel vertical bands; keep band-rounded height <= max_h
                // so terminals that paint full 6px bands never spill past view_rows.
                if crop_h.div_ceil(6) * 6 > max_h && max_h >= 6 {
                    crop_h = max_h - (max_h % 6);
                }
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = (crop_h.div_ceil(6) * 6)
                    .div_ceil(cell_h)
                    .clamp(1, view_rows as u32) as u16;
                let payload = encode_sixel_indexed_slice(
                    palette,
                    indexed,
                    *width as usize,
                    x0 as usize,
                    y0 as usize,
                    crop_w as usize,
                    crop_h as usize,
                );
                Some((payload, draw_cols, draw_rows))
            }
            PixelScrollBacking::Source {
                image,
                target_px_w,
                target_px_h,
                cell_px,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *target_px_w
                    || y0 >= *target_px_h
                    || *target_px_w == 0
                    || *target_px_h == 0
                {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let max_h = (view_rows as u32).saturating_mul(cell_h);
                let crop_w = (*target_px_w - x0).min(max_w);
                let crop_h = (*target_px_h - y0).min(max_h);
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = crop_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;

                let src_w = image.width();
                let src_h = image.height();
                if src_w == 0 || src_h == 0 {
                    return None;
                }
                let src_x0 = ((x0 as u64) * (src_w as u64) / (*target_px_w as u64))
                    .min(src_w.saturating_sub(1) as u64) as u32;
                let src_y0 = ((y0 as u64) * (src_h as u64) / (*target_px_h as u64))
                    .min(src_h.saturating_sub(1) as u64) as u32;
                let src_crop_w = ((crop_w as u64) * (src_w as u64))
                    .div_ceil(*target_px_w as u64)
                    .clamp(1, (src_w - src_x0) as u64) as u32;
                let src_crop_h = ((crop_h as u64) * (src_h as u64))
                    .div_ceil(*target_px_h as u64)
                    .clamp(1, (src_h - src_y0) as u64) as u32;

                let sub_img = image.crop_imm(src_x0, src_y0, src_crop_w, src_crop_h);
                let png_bytes = encode_png(&sub_img).ok()?;
                let payload = match self.protocol {
                    TerminalImageProtocol::Kitty => {
                        let axis = kitty_fit_axis(
                            src_crop_w,
                            src_crop_h,
                            draw_cols,
                            draw_rows,
                            *cell_px,
                        );
                        encode_kitty_tui_payload(&png_bytes, axis)
                    }
                    TerminalImageProtocol::ITerm2 => {
                        let b64 = B64_STANDARD.encode(&png_bytes);
                        format!(
                            "\x1b]1337;File=inline=1;width={draw_cols};height={draw_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
                        )
                    }
                    TerminalImageProtocol::Sixel | TerminalImageProtocol::Halfblock => {
                        return None;
                    }
                };
                Some((payload, draw_cols, draw_rows))
            }
        }
    }
}

/// Tracks an active terminal pixel graphic placement (Kitty / iTerm2 / Sixel) painted onto the screen
/// so frame transitions can deterministically erase stale pixels before Ratatui redraws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    pub cols: u16,
    pub rows: u16,
    pub payload_len: usize,
}

/// Memory-budgeted FIFO cache for decoded [`RenderedTerminalImage`] entries (default 8 MiB budget).
#[derive(Debug, Clone)]
pub struct DecodedImageCache {
    entries: HashMap<String, RenderedTerminalImage>,
    order: VecDeque<String>,
    total_bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for DecodedImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodedImageCache {
    /// Creates a decoded image cache with the default 8 MiB memory budget ([`MAX_IMAGE_CACHE_BYTES`]).
    pub fn new() -> Self {
        Self::with_limits(MAX_IMAGE_CACHE_BYTES, DEFAULT_MAX_IMAGE_CACHE_ENTRIES)
    }

    /// Creates a decoded image cache with explicit byte and entry limits.
    pub fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_bytes: 0,
            max_bytes: max_bytes.max(64),
            max_entries: max_entries.max(1),
        }
    }

    fn entry_bytes(img: &RenderedTerminalImage) -> usize {
        img.approx_heap_bytes().max(64)
    }

    /// Inserts or replaces an entry, evicting oldest entries until within byte and entry budgets.
    /// Returns the list of evicted cache keys.
    pub fn insert(&mut self, key: String, rendered: RenderedTerminalImage) -> Vec<String> {
        let mut evicted_keys = Vec::new();
        if let Some(prev) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(Self::entry_bytes(&prev));
            self.order.retain(|k| k != &key);
        }

        let entry_bytes = Self::entry_bytes(&rendered);
        self.entries.insert(key.clone(), rendered);
        self.order.push_back(key);
        self.total_bytes = self.total_bytes.saturating_add(entry_bytes);

        while self.order.len() > self.max_entries {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&evicted) {
                self.total_bytes = self.total_bytes.saturating_sub(Self::entry_bytes(&removed));
                evicted_keys.push(evicted);
            }
        }

        while self.total_bytes > self.max_bytes && self.order.len() > 1 {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&evicted) {
                self.total_bytes = self.total_bytes.saturating_sub(Self::entry_bytes(&removed));
                evicted_keys.push(evicted);
            }
        }

        evicted_keys
    }

    pub fn get(&self, key: &str) -> Option<&RenderedTerminalImage> {
        self.entries.get(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<RenderedTerminalImage> {
        let removed = self.entries.remove(key)?;
        self.total_bytes = self.total_bytes.saturating_sub(Self::entry_bytes(&removed));
        self.order.retain(|k| k != key);
        Some(removed)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_bytes = 0;
    }
}

/// Builds a viewport-, zoom-, cell-pixel-, and protocol-scoped preview cache key
/// (`"{att_id}:{view_cols}x{view_rows}@{zoom_pct}%:{cw}x{ch}:{protocol}"`).
pub fn preview_cache_key(
    att_id: &str,
    view_cols: u16,
    view_rows: u16,
    zoom_pct: u16,
    cell_px: (u16, u16),
    protocol: TerminalImageProtocol,
) -> String {
    format!(
        "{att_id}:{view_cols}x{view_rows}@{zoom_pct}%:{}x{}:{}",
        cell_px.0,
        cell_px.1,
        protocol.as_str()
    )
}

/// Returns the Kitty graphics delete sequence for the fixed TUI preview placement (`q=2` suppresses terminal responses).
pub fn kitty_delete_preview_sequence() -> &'static str {
    "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
}

/// Deletes all visible Kitty graphics placements and frees unused image data (`d=A`, `q=2` quiet).
pub fn kitty_delete_all_sequence() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// Builds an ANSI sequence (`\x1b[0m` + row cursor moves + space runs) that overwrites `rect`
/// (expanded by +2 cols / +2 rows and clamped to `term_cols x term_rows`) so Sixel and iTerm2
/// cell-bound pixel tiles are deterministically erased before Ratatui redraws.
pub fn erase_pixel_rect_sequence(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    term_cols: u16,
    term_rows: u16,
) -> String {
    if width == 0 || height == 0 || x >= term_cols || y >= term_rows {
        return String::new();
    }
    let x_end = x.saturating_add(width).saturating_add(2).min(term_cols);
    let y_end = y.saturating_add(height).saturating_add(2).min(term_rows);
    if x >= x_end || y >= y_end {
        return String::new();
    }
    let span_w = (x_end - x) as usize;
    let rows = (y_end - y) as usize;
    let spaces = " ".repeat(span_w);
    let mut out = String::with_capacity(4 + rows * (12 + span_w));
    out.push_str("\x1b[0m");
    for row in y..y_end {
        let _ = write!(out, "\x1b[{};{}H{spaces}", row + 1, x + 1);
    }
    out
}

/// Floors the maximum vertical pixel budget for Sixel encoding to a multiple of 6
/// (`<= rows * cell_px_h`) so the final 6-pixel band never overhangs into the row below `rect`.
pub fn compute_sixel_safe_pixel_height(rows: u16, cell_px_h: u16) -> u32 {
    let budget = (rows as u32).saturating_mul(cell_px_h as u32);
    (budget / 6) * 6
}

/// Resolves terminal graphics protocol and cell pixel dimensions without writing queries to stdin/stdout.
pub fn detect_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let protocol = resolve_protocol_with(pref, |k| std::env::var(k).ok());
    let (cell_px, cell_px_exact) = detect_cell_pixel_size();
    TerminalGraphicsCaps {
        protocol,
        cell_px,
        cell_px_exact,
    }
}

/// Convenience helper calling [`detect_graphics_caps`] with [`ImageProtocolPreference::Auto`].
pub fn detect_graphics_caps_auto() -> TerminalGraphicsCaps {
    detect_graphics_caps(ImageProtocolPreference::Auto)
}

/// Pure protocol resolver taking an environment variable map (for deterministic testing and diagnostics).
pub fn resolve_protocol_from_env_map(
    pref: ImageProtocolPreference,
    env: &HashMap<&str, &str>,
) -> TerminalImageProtocol {
    resolve_protocol_with(pref, |k| env.get(k).map(|v| (*v).to_string()))
}

fn is_stackbite_env<F>(get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    non_empty_env(get_env, "STACKBITE_ROOM_ID").is_some()
        || non_empty_env(get_env, "STACKBITE_HOME").is_some()
        || non_empty_env(get_env, "STACKBITE_PTY_BACKEND").is_some()
}

fn first_non_empty_env<F>(get_env: &F, keys: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|k| non_empty_env(get_env, k))
}

fn resolve_protocol_with<F>(pref: ImageProtocolPreference, get_env: F) -> TerminalImageProtocol
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = first_non_empty_env(&get_env, ENV_PROTOCOL_KEYS) {
        if let Some(parsed) = ImageProtocolPreference::parse_str(&raw) {
            if parsed != ImageProtocolPreference::Auto {
                return protocol_from_pref(parsed);
            }
        }
    }

    if pref != ImageProtocolPreference::Auto {
        return protocol_from_pref(pref);
    }

    // Multiplexer guard: avoid raw pixel escape sequences inside tmux/screen without DCS passthrough.
    if non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some() {
        return TerminalImageProtocol::Halfblock;
    }

    // StackBite PTY guard: suppress inherited host-terminal env vars (e.g. WT_SESSION) on static
    // paths; TUI startup will still run live Kitty/DA1 probing and trust only actual terminal replies.
    if is_stackbite_env(&get_env) {
        return TerminalImageProtocol::Halfblock;
    }

    let term_program = lower_env(&get_env, "TERM_PROGRAM");
    let term = lower_env(&get_env, "TERM");

    // Kitty Graphics Protocol (Kitty, Ghostty, and WezTerm all support Kitty graphics natively).
    if non_empty_env(&get_env, "KITTY_WINDOW_ID").is_some()
        || non_empty_env(&get_env, "GHOSTTY_RESOURCES_DIR").is_some()
        || non_empty_env(&get_env, "WEZTERM_EXECUTABLE").is_some()
        || non_empty_env(&get_env, "WEZTERM_PANE").is_some()
        || matches!(term_program.as_str(), "kitty" | "ghostty" | "wezterm")
        || term.contains("xterm-kitty")
        || term.contains("ghostty")
    {
        return TerminalImageProtocol::Kitty;
    }

    // iTerm2 Inline Images (`OSC 1337`).
    if matches!(
        term_program.as_str(),
        "iterm.app" | "rio" | "mintty" | "hyper"
    ) {
        return TerminalImageProtocol::ITerm2;
    }

    // Sixel (`DCS q`) for terminals known to support Sixel unconditionally.
    if matches!(
        term.as_str(),
        "foot" | "foot-extra" | "mlterm" | "yaft-256color"
    ) || matches!(term_program.as_str(), "foot" | "contour")
    {
        return TerminalImageProtocol::Sixel;
    }

    // `WT_SESSION` is intentionally left as `Halfblock` here so `Auto` mode always verifies
    // live Kitty `a=q` / DA1 `;4` support instead of assuming Sixel on WT < 1.22 or inherited hosts.
    TerminalImageProtocol::Halfblock
}

fn protocol_from_pref(pref: ImageProtocolPreference) -> TerminalImageProtocol {
    match pref {
        ImageProtocolPreference::Auto | ImageProtocolPreference::Halfblock => {
            TerminalImageProtocol::Halfblock
        }
        ImageProtocolPreference::Kitty => TerminalImageProtocol::Kitty,
        ImageProtocolPreference::Iterm2 => TerminalImageProtocol::ITerm2,
        ImageProtocolPreference::Sixel => TerminalImageProtocol::Sixel,
    }
}

fn non_empty_env<F>(get_env: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key).and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn lower_env<F>(get_env: &F, key: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Computes terminal cell pixel dimensions `(cell_w, cell_h)` from window pixel and cell counts
/// using rounded division (avoiding truncation bias).
pub fn compute_rounded_cell_px(
    win_w: u16,
    win_h: u16,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16)> {
    if win_w == 0 || win_h == 0 || cols == 0 || rows == 0 {
        return None;
    }
    let cw = ((win_w as u32 + cols as u32 / 2) / cols as u32).clamp(4, 64) as u16;
    let ch = ((win_h as u32 + rows as u32 / 2) / rows as u32).clamp(8, 128) as u16;
    Some((cw, ch))
}

fn detect_cell_pixel_size() -> ((u16, u16), bool) {
    if let Some(parsed) = resolve_cell_pixel_size_with(|k| std::env::var(k).ok()) {
        return (parsed, true);
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if let Some(cell_px) = compute_rounded_cell_px(ws.width, ws.height, ws.columns, ws.rows) {
            return (cell_px, true);
        }
    }
    (DEFAULT_FALLBACK_CELL_PX, false)
}

/// Parses a `CSI 16 t` reply (`\x1b[6;<height>;<width>t`) into `(cell_w, cell_h)` in pixels.
pub fn parse_csi_cell_size_reply(buf: &[u8]) -> Option<(u16, u16)> {
    parse_csi_t_pair(buf, b'6').map(|(h, w)| (w.clamp(4, 64), h.clamp(8, 128)))
}

/// Parses a `CSI 14 t` reply (`\x1b[4;<height>;<width>t`) into `(win_width_px, win_height_px)`.
pub fn parse_csi_win_size_reply(buf: &[u8]) -> Option<(u16, u16)> {
    parse_csi_t_pair(buf, b'4').map(|(h, w)| (w, h))
}

fn parse_csi_t_pair(buf: &[u8], code: u8) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 5 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == code && buf[i + 3] == b';' {
            let mut j = i + 4;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && buf[j] == b't' {
                if let Ok(params) = std::str::from_utf8(&buf[i + 4..j]) {
                    let mut parts = params.split(';');
                    if let (Some(h_s), Some(w_s), None) = (parts.next(), parts.next(), parts.next())
                    {
                        if let (Ok(h), Ok(w)) = (h_s.parse::<u16>(), w_s.parse::<u16>()) {
                            if h > 0 && w > 0 {
                                return Some((h, w));
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Parses a Primary Device Attributes (`DA1`, `\x1b[?...c`) reply and returns `true` if attribute `;4` (Sixel) is advertised.
pub fn parse_da1_has_sixel(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && buf[j] == b'c' {
                if let Ok(params) = std::str::from_utf8(&buf[i + 3..j]) {
                    if params.split(';').skip(1).any(|attr| attr == "4") {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// Parses a Kitty Graphics Protocol query reply (`\x1b_G...;OK\x1b\\` or `\x1b_G...;OK\x07`).
pub fn parse_kitty_query_ok(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'_' && buf[i + 2] == b'G' {
            let start = i + 3;
            let mut end = start;
            let mut found_term = false;
            while end < buf.len() {
                if buf[end] == 0x07 {
                    found_term = true;
                    break;
                }
                if buf[end] == 0x1b && end + 1 < buf.len() && buf[end + 1] == b'\\' {
                    found_term = true;
                    break;
                }
                end += 1;
            }
            if found_term {
                if let Ok(body) = std::str::from_utf8(&buf[start..end]) {
                    if let Some((_, status)) = body.split_once(';') {
                        if status.trim() == "OK" {
                            return true;
                        }
                    }
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn contains_da1_reply(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j > i + 3 && j < buf.len() && buf[j] == b'c' {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn apply_probe_reply_to_caps(
    caps: &mut TerminalGraphicsCaps,
    reply: &str,
    need_cell_px: bool,
    need_protocol: bool,
    term_size: Option<(u16, u16)>,
) {
    let reply_bytes = reply.as_bytes();
    if reply_bytes.is_empty() {
        return;
    }

    if need_cell_px {
        if let Some(cell_px) = parse_csi_cell_size_reply(reply_bytes) {
            caps.cell_px = cell_px;
            caps.cell_px_exact = true;
        } else if let Some((win_w, win_h)) = parse_csi_win_size_reply(reply_bytes) {
            if let Some((cols, rows)) = term_size {
                if let Some(cell_px) = compute_rounded_cell_px(win_w, win_h, cols, rows) {
                    caps.cell_px = cell_px;
                    caps.cell_px_exact = true;
                }
            }
        }
    }

    if need_protocol {
        if parse_kitty_query_ok(reply_bytes) {
            caps.protocol = TerminalImageProtocol::Kitty;
        } else if parse_da1_has_sixel(reply_bytes) {
            caps.protocol = TerminalImageProtocol::Sixel;
        }
    }
}

/// Performs non-blocking, timeout-bounded (150ms) active terminal graphics and cell-pixel probing.
/// Must be called once on TUI startup after raw mode is enabled and before spawning `EventStream`.
pub fn probe_terminal_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let mut caps = detect_graphics_caps(pref);
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return caps;
    }

    let get_env = |k: &str| std::env::var(k).ok();
    let in_mux =
        non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some();
    let env_override = first_non_empty_env(&get_env, ENV_PROTOCOL_KEYS)
        .and_then(|v| ImageProtocolPreference::parse_str(&v))
        .is_some_and(|p| p != ImageProtocolPreference::Auto);

    let need_cell_px = !caps.cell_px_exact;
    let need_protocol = pref == ImageProtocolPreference::Auto
        && !env_override
        && !in_mux
        && caps.protocol == TerminalImageProtocol::Halfblock;

    if !need_cell_px && !need_protocol {
        return caps;
    }

    let mut query = String::new();
    if need_cell_px {
        query.push_str("\x1b[16t\x1b[14t");
    }
    if need_protocol {
        query.push_str("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    }
    // Primary Device Attributes (DA1) acts as both Sixel capability query and synchronization sentinel.
    query.push_str("\x1b[c");

    let reply = write_and_read_terminal_probe(&query, PROBE_TIMEOUT);
    apply_probe_reply_to_caps(
        &mut caps,
        &reply,
        need_cell_px,
        need_protocol,
        crossterm::terminal::size().ok(),
    );

    caps
}

#[cfg(any(windows, test))]
pub const WIN_ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
#[cfg(any(windows, test))]
pub const WIN_ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
#[cfg(any(windows, test))]
pub const WIN_ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// Restores Windows console input mode while always setting `ENABLE_EXTENDED_FLAGS` (`0x0080`)
/// and clearing `ENABLE_QUICK_EDIT_MODE` (`0x0040`) so mouse capture is never clobbered by QuickEdit.
#[cfg(any(windows, test))]
pub fn restore_windows_console_mode_flags(prev_mode: u32) -> u32 {
    (prev_mode | WIN_ENABLE_EXTENDED_FLAGS) & !WIN_ENABLE_QUICK_EDIT_MODE
}

#[cfg(any(windows, test))]
struct ModeRestore<F: FnMut(u32)> {
    prev_mode: u32,
    active: bool,
    set_mode: F,
}

#[cfg(any(windows, test))]
impl<F: FnMut(u32)> Drop for ModeRestore<F> {
    fn drop(&mut self) {
        if self.active {
            // Always include ENABLE_EXTENDED_FLAGS (0x0080) and clear ENABLE_QUICK_EDIT_MODE (0x0040)
            // so SetConsoleMode never re-enables QuickEdit when GetConsoleMode omitted 0x0080.
            (self.set_mode)(restore_windows_console_mode_flags(self.prev_mode));
        }
    }
}

/// Core Windows probe sequencing helper: enables `ENABLE_VIRTUAL_TERMINAL_INPUT` via RAII
/// [`ModeRestore`] **before** writing and flushing `query`, then reads the reply and restores mode on drop.
#[cfg(any(windows, test))]
fn run_windows_probe_with_hooks<W, SetMode, ReadFn>(
    prev_mode: u32,
    query: &str,
    writer: &mut W,
    mut set_mode: SetMode,
    read_reply: ReadFn,
) -> String
where
    W: IoWrite,
    SetMode: FnMut(u32),
    ReadFn: FnOnce() -> Vec<u8>,
{
    let probe_mode =
        restore_windows_console_mode_flags(prev_mode) | WIN_ENABLE_VIRTUAL_TERMINAL_INPUT;
    let active = prev_mode != probe_mode;
    if active {
        set_mode(probe_mode);
    }
    let _restore = ModeRestore {
        prev_mode,
        active,
        set_mode,
    };

    if writer.write_all(query.as_bytes()).is_err() || writer.flush().is_err() {
        return String::new();
    }

    let bytes = read_reply();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(windows)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // -10i32 as u32
    const WAIT_OBJECT_0: u32 = 0x0000_0000;
    const KEY_EVENT: u16 = 0x0001;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        b_key_down: i32,
        w_repeat_count: u16,
        w_virtual_key_code: u16,
        w_virtual_scan_code: u16,
        u_char: u16,
        dw_control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union InputEventUnion {
        key_event: KeyEventRecord,
        _pad: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _padding: u16,
        event: InputEventUnion,
    }

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
        fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
        fn GetNumberOfConsoleInputEvents(
            h_console_input: isize,
            lpc_number_of_events: *mut u32,
        ) -> i32;
        fn PeekConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
        fn ReadConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
    }

    struct OwnedHandle(Option<isize>);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if let Some(h) = self.0 {
                unsafe {
                    CloseHandle(h);
                }
            }
        }
    }

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
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        )
    };
    let (handle, _owned_handle) = if conin != 0 && conin != INVALID_HANDLE_VALUE {
        (conin, OwnedHandle(Some(conin)))
    } else {
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        (std_in, OwnedHandle(None))
    };
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return String::new();
    }

    let mut prev_mode = 0u32;
    if unsafe { GetConsoleMode(handle, &mut prev_mode) } == 0 {
        return String::new();
    }

    let mut stdout = std::io::stdout();
    // `run_windows_probe_with_hooks` enables VT input BEFORE `stdout.write_all(query)` + `flush()`,
    // and drops `ModeRestore` BEFORE `_owned_handle` closes `handle`.
    run_windows_probe_with_hooks(
        prev_mode,
        query,
        &mut stdout,
        |mode| unsafe {
            let _ = SetConsoleMode(handle, mode);
        },
        || {
            let deadline = Instant::now() + timeout;
            let mut out = Vec::with_capacity(256);
            let mut records = [InputRecord {
                event_type: 0,
                _padding: 0,
                event: InputEventUnion { _pad: [0; 4] },
            }; 64];

            while out.len() < 4096 {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let wait_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
                if wait_ms == 0 {
                    break;
                }
                let wait_res = unsafe { WaitForSingleObject(handle, wait_ms) };
                if wait_res != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_read = (avail as usize).min(records.len()) as u32;
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), to_read, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let ke = unsafe { rec.event.key_event };
                        if ke.b_key_down != 0 && ke.u_char != 0 && ke.u_char <= 0x7f {
                            out.push(ke.u_char as u8);
                        }
                    }
                }
                if contains_da1_reply(&out) {
                    break;
                }
            }

            // Non-blocking drain of any trailing KEY_EVENT bytes from the probe burst so nothing leaks into EventStream.
            for _ in 0..8 {
                if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_peek = (avail as usize).min(records.len()) as u32;
                let mut peeked = 0u32;
                if unsafe {
                    PeekConsoleInputW(handle, records.as_mut_ptr(), to_peek, &mut peeked)
                } == 0
                    || peeked == 0
                {
                    break;
                }
                let key_prefix = records[..peeked as usize]
                    .iter()
                    .take_while(|rec| rec.event_type == KEY_EVENT)
                    .count() as u32;
                if key_prefix == 0 {
                    break;
                }
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), key_prefix, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let ke = unsafe { rec.event.key_event };
                        if ke.b_key_down != 0
                            && ke.u_char != 0
                            && ke.u_char <= 0x7f
                            && out.len() < 4096
                        {
                            out.push(ke.u_char as u8);
                        }
                    }
                }
            }

            out
        },
    )
}

#[cfg(unix)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    use std::os::unix::io::AsRawFd;

    const POLLIN: i16 = 0x0001;

    #[cfg(target_os = "macos")]
    type Nfds = u32;
    #[cfg(not(target_os = "macos"))]
    type Nfds = usize;

    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }

    extern "C" {
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: i32) -> i32;
        fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    }

    let mut stdout = std::io::stdout();
    if stdout.write_all(query.as_bytes()).is_err() || stdout.flush().is_err() {
        return String::new();
    }

    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut out = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];

    while Instant::now() < deadline {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(15) as i32;
        let mut pfd = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        let ready = unsafe { poll(&mut pfd, 1 as Nfds, remaining.max(1)) };
        if ready <= 0 {
            if contains_da1_reply(&out) {
                break;
            }
            continue;
        }
        if (pfd.revents & POLLIN) != 0 {
            let n = unsafe { read(fd, chunk.as_mut_ptr(), chunk.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n as usize]);
            if contains_da1_reply(&out) {
                break;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(not(any(windows, unix)))]
fn write_and_read_terminal_probe(_query: &str, _timeout: Duration) -> String {
    String::new()
}

fn resolve_cell_pixel_size_with<F>(get_env: F) -> Option<(u16, u16)>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = first_non_empty_env(&get_env, ENV_CELL_PX_KEYS) {
        if let Some(parsed) = parse_cell_pixel_size(&raw) {
            return Some(parsed);
        }
    }
    None
}

fn parse_cell_pixel_size(raw: &str) -> Option<(u16, u16)> {
    let (w_str, h_str) = raw.trim().split_once(['x', 'X'])?;
    let w = w_str.trim().parse::<u16>().ok()?;
    let h = h_str.trim().parse::<u16>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.clamp(4, 64), h.clamp(8, 128)))
}

fn trim_bom_and_ascii_ws(mut bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes = &bytes[3..];
    }
    while let Some((&first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

fn has_tag_boundary(rest: &[u8]) -> bool {
    match rest.first() {
        None => true,
        Some(&b) => b.is_ascii_whitespace() || b == b'>' || b == b'/',
    }
}

/// Returns `true` if `bytes` look like an HTML/XHTML document rather than a raster image
/// (e.g., SSO login redirects or HTML error pages returned by attachment URLs).
pub fn looks_like_html_bytes(bytes: &[u8]) -> bool {
    let trimmed = trim_bom_and_ascii_ws(bytes);
    if trimmed.is_empty() || trimmed[0] != b'<' {
        return false;
    }
    let sample_len = trimmed.len().min(1024);
    let lower = String::from_utf8_lossy(&trimmed[..sample_len]).to_ascii_lowercase();
    let html_prefixes = [
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<meta",
        "<title",
        "<script",
    ];
    for pfx in html_prefixes {
        if let Some(rest) = lower.strip_prefix(pfx) {
            if pfx == "<!doctype html" || has_tag_boundary(rest.as_bytes()) {
                return true;
            }
        }
    }
    if lower.starts_with("<?xml") || lower.starts_with("<!--") {
        if lower.contains("<!doctype html") || lower.contains("<html") {
            return true;
        }
    }
    false
}

/// Returns `true` if `bytes` look like an SVG vector document (`<svg ...>`), which cannot be
/// raster-decoded by `image`.
pub fn looks_like_svg_bytes(bytes: &[u8]) -> bool {
    let trimmed = trim_bom_and_ascii_ws(bytes);
    if trimmed.is_empty() || trimmed[0] != b'<' || looks_like_html_bytes(trimmed) {
        return false;
    }
    let sample_len = trimmed.len().min(1024);
    let lower = String::from_utf8_lossy(&trimmed[..sample_len]).to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("<svg") {
        return has_tag_boundary(rest.as_bytes());
    }
    if lower.starts_with("<?xml")
        || lower.starts_with("<!doctype svg")
        || lower.starts_with("<!--")
    {
        if let Some(idx) = lower.find("<svg") {
            let after = &lower.as_bytes()[idx + 4..];
            return has_tag_boundary(after);
        }
    }
    false
}

fn reject_non_raster_markup_bytes(bytes: &[u8]) -> Result<()> {
    if looks_like_html_bytes(bytes) {
        bail!("Received HTML document instead of raster image");
    }
    Ok(())
}

/// Returns `true` if a MIME type or filename represents a supported image (`png`, `jpeg`, `jpg`, `gif`, `webp`, `svg`).
/// Never returns `true` for HTML/XHTML (`text/html`, `application/xhtml+xml`, `.html`, `.htm`, `.xhtml`).
pub fn is_image_attachment(mime_type: &str, filename: &str) -> bool {
    let m_base = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(m_base.as_str(), "text/html" | "application/xhtml+xml") || m_base.contains("html") {
        return false;
    }

    let f = filename.trim().to_ascii_lowercase();
    if f.ends_with(".html") || f.ends_with(".htm") || f.ends_with(".xhtml") {
        return false;
    }

    if m_base == "image/svg+xml" || m_base.starts_with("image/") {
        return true;
    }
    f.ends_with(".png")
        || f.ends_with(".jpg")
        || f.ends_with(".jpeg")
        || f.ends_with(".gif")
        || f.ends_with(".webp")
        || f.ends_with(".svg")
        || f.ends_with(".svgz")
}

/// Returns `true` if a MIME type or optional filename looks like a supported raster image.
pub fn is_image_mime_or_name(content_type: &str, file_name: Option<&str>) -> bool {
    is_image_attachment(content_type, file_name.unwrap_or_default())
}

/// Detects the preferred stdout inline image protocol from environment variables.
pub fn detect_stdout_protocol() -> TerminalGraphicsProtocol {
    detect_graphics_caps_auto().protocol.into()
}

/// Decodes raster image bytes and renders aspect-ratio-preserving Truecolor Half-Blocks (`▀`)
/// alpha-composited against `#1E1F22` for `ratatui`.
pub fn render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
) -> Result<RenderedTerminalImage> {
    decode_and_render_halfblocks(bytes, max_cols, max_rows, DEFAULT_PREVIEW_BG_RGB)
}

/// Decodes raster image bytes and renders Truecolor Half-Blocks (`▀`) composited over `bg_rgb`.
pub fn render_halfblocks_with_bg(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    decode_and_render_halfblocks(bytes, max_cols, max_rows, bg_rgb)
}

/// Decodes an image buffer and renders aspect-ratio-preserving Truecolor Unicode Half-Blocks (`▀`) for `ratatui`.
pub fn decode_and_render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    decode_and_render_halfblocks_with_cell_px(
        bytes,
        FitBox::fit(max_cols, max_rows),
        bg_rgb,
        DEFAULT_FALLBACK_CELL_PX,
    )
}

fn svg_raster_bounds(fit: FitBox, cell_px: (u16, u16)) -> (u32, u32) {
    let zoom = fit.zoom_pct.clamp(25, 300) as u32;
    let max_px_w = ((fit.view_cols.max(1) as u32)
        .saturating_mul(cell_px.0.max(1) as u32)
        .saturating_mul(zoom)
        / 100)
        .clamp(64, MAX_RENDER_PX_W.min(2048));
    let max_px_h = if fit.scrollable {
        MAX_RENDER_PX_H.min(2048)
    } else {
        ((fit.view_rows.max(1) as u32)
            .saturating_mul(cell_px.1.max(1) as u32)
            .saturating_mul(zoom)
            / 100)
            .clamp(64, MAX_RENDER_PX_H.min(2048))
    };
    (max_px_w, max_px_h)
}

/// Decodes an image buffer and renders Truecolor Unicode Half-Blocks (`▀`) compensating for the
/// terminal's actual cell pixel aspect ratio (`cell_px`) and compositing alpha over `bg_rgb`.
pub fn decode_and_render_halfblocks_with_cell_px(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    cell_px: (u16, u16),
) -> Result<RenderedTerminalImage> {
    reject_non_raster_markup_bytes(bytes)?;
    let svg_png;
    let (decode_bytes, format_label): (&[u8], String) = if looks_like_svg_bytes(bytes) {
        let (max_px_w, max_px_h) = svg_raster_bounds(fit, cell_px);
        let rasterized = bitefoundry_html::graphics::rasterize_svg_to_png(
            bytes, max_px_w, max_px_h, None,
        )
        .context("Failed to rasterize SVG image")?;
        svg_png = rasterized.png_bytes;
        (&svg_png[..], "SVG".to_string())
    } else {
        let guessed = image::guess_format(bytes).ok();
        (bytes, format_label_for(guessed))
    };
    let dyn_img = image::load_from_memory(decode_bytes).context("Failed to decode image")?;
    let rgba = dyn_img.into_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_w, target_h) = compute_halfblock_dims(width_px, height_px, fit, cell_px);

    let resized = image::imageops::resize(&rgba, target_w, target_h, FilterType::Lanczos3);
    let mut lines = Vec::with_capacity((target_h / 2) as usize);

    for y in (0..target_h).step_by(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_pair: Option<((u8, u8, u8), (u8, u8, u8))> = None;
        let mut run = String::new();

        for x in 0..target_w {
            let top = blend_over_bg(*resized.get_pixel(x, y), bg_rgb);
            let bot = blend_over_bg(*resized.get_pixel(x, y + 1), bg_rgb);
            let pair = (top, bot);

            if current_pair == Some(pair) {
                run.push('▀');
            } else {
                if let Some((prev_top, prev_bot)) = current_pair {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        Style::default()
                            .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                            .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
                    ));
                }
                current_pair = Some(pair);
                run.push('▀');
            }
        }

        if let Some((prev_top, prev_bot)) = current_pair {
            spans.push(Span::styled(
                run,
                Style::default()
                    .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                    .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
            ));
        }

        lines.push(Line::from(spans));
    }

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: target_w as u16,
        rows: (target_h / 2) as u16,
        format_label,
        protocol: TerminalImageProtocol::Halfblock,
        lines,
        pixel_payload: None,
        scroll_backing: None,
    })
}

/// Decodes and renders an image using the requested terminal graphics protocol (`Kitty`, `ITerm2`, `Sixel`, or `Halfblock`).
pub fn decode_and_render_for_protocol(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    caps: TerminalGraphicsCaps,
) -> Result<RenderedTerminalImage> {
    reject_non_raster_markup_bytes(bytes)?;
    if caps.protocol == TerminalImageProtocol::Halfblock {
        return decode_and_render_halfblocks_with_cell_px(bytes, fit, bg_rgb, caps.cell_px);
    }

    let svg_png;
    let (decode_bytes, guessed, format_label): (&[u8], Option<ImageFormat>, String) =
        if looks_like_svg_bytes(bytes) {
            let (max_px_w, max_px_h) = svg_raster_bounds(fit, caps.cell_px);
            let rasterized = bitefoundry_html::graphics::rasterize_svg_to_png(
                bytes, max_px_w, max_px_h, None,
            )
            .context("Failed to rasterize SVG image")?;
            svg_png = rasterized.png_bytes;
            (&svg_png[..], Some(ImageFormat::Png), "SVG".to_string())
        } else {
            let guessed = image::guess_format(bytes).ok();
            (bytes, guessed, format_label_for(guessed))
        };
    let dyn_img = image::load_from_memory(decode_bytes).context("Failed to decode image")?;
    let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_px_w, target_px_h, occ_cols, occ_rows) = compute_pixel_fit_for_protocol(
        width_px,
        height_px,
        fit,
        caps.cell_px,
        caps.protocol,
    );

    let (payload, scroll_backing) = match caps.protocol {
        TerminalImageProtocol::Kitty => {
            let source_img = if width_px > MAX_RENDER_PX_W || height_px > MAX_RENDER_PX_H {
                let (fit_w, fit_h) =
                    fit_within_max_bounds(width_px, height_px, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3)
            } else {
                dyn_img
            };
            let src_w = source_img.width();
            let src_h = source_img.height();
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                decode_bytes
            } else {
                png_owned = encode_png(&source_img)?;
                &png_owned
            };
            let axis = kitty_fit_axis(src_w, src_h, occ_cols, occ_rows, caps.cell_px);
            let backing = PixelScrollBacking::Source {
                image: Arc::new(source_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            };
            (encode_kitty_tui_payload(png_bytes, axis), Some(backing))
        }
        TerminalImageProtocol::ITerm2 => {
            let source_img = if width_px > MAX_RENDER_PX_W || height_px > MAX_RENDER_PX_H {
                let (fit_w, fit_h) =
                    fit_within_max_bounds(width_px, height_px, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3)
            } else {
                dyn_img
            };
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && decode_bytes.len() <= MAX_ITERM2_RAW_BYTES
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                decode_bytes
            } else {
                encoded_owned = encode_png(&source_img)?;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            let payload = format!(
                "\x1b]1337;File=inline=1;width={occ_cols};height={occ_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
            );
            let backing = PixelScrollBacking::Source {
                image: Arc::new(source_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            };
            (payload, Some(backing))
        }
        TerminalImageProtocol::Sixel => {
            let rgba = dyn_img.to_rgba8();
            let resized = if width_px == target_px_w && height_px == target_px_h {
                rgba
            } else {
                image::imageops::resize(&rgba, target_px_w, target_px_h, FilterType::Lanczos3)
            };
            let mut flat_rgba = Vec::with_capacity((target_px_w * target_px_h * 4) as usize);
            for px in resized.pixels() {
                let (r, g, b) = blend_over_bg(*px, bg_rgb);
                flat_rgba.extend_from_slice(&[r, g, b, 255]);
            }
            let (palette, indexed) =
                quantize_rgba_to_256(&flat_rgba, target_px_w as usize, target_px_h as usize);
            let payload = encode_sixel_indexed_slice(
                &palette,
                &indexed,
                target_px_w as usize,
                0,
                0,
                target_px_w as usize,
                target_px_h as usize,
            );
            let backing = PixelScrollBacking::SixelIndexed {
                width: target_px_w,
                height: target_px_h,
                cell_px: caps.cell_px,
                palette,
                indexed: Arc::new(indexed),
            };
            (payload, Some(backing))
        }
        TerminalImageProtocol::Halfblock => unreachable!("handled above"),
    };

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: occ_cols,
        rows: occ_rows,
        format_label,
        protocol: caps.protocol,
        lines: Vec::new(),
        pixel_payload: Some(payload),
        scroll_backing,
    })
}

fn kitty_fit_axis(
    src_w: u32,
    src_h: u32,
    occ_cols: u16,
    occ_rows: u16,
    cell_px: (u16, u16),
) -> KittyFitAxis {
    let cell_w = cell_px.0.max(1) as u64;
    let cell_h = cell_px.1.max(1) as u64;
    let lhs = (src_w as u64) * (occ_rows.max(1) as u64 * cell_h);
    let rhs = (src_h as u64) * (occ_cols.max(1) as u64 * cell_w);
    if lhs > rhs || (lhs == rhs && src_w >= src_h) {
        KittyFitAxis::Cols(occ_cols.max(1))
    } else {
        KittyFitAxis::Rows(occ_rows.max(1))
    }
}

fn encode_kitty_tui_payload(png_bytes: &[u8], axis: KittyFitAxis) -> String {
    let b64 = B64_STANDARD.encode(png_bytes);
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
        .collect();
    let total = chunks.len();
    let mut out = String::with_capacity(b64.len() + total * 48);
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let more = if idx + 1 < total { 1 } else { 0 };
        if idx == 0 {
            match axis {
                KittyFitAxis::Cols(c) => {
                    let _ = write!(
                        out,
                        "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={c},C=1,m={more};{chunk}\x1b\\"
                    );
                }
                KittyFitAxis::Rows(r) => {
                    let _ = write!(
                        out,
                        "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},r={r},C=1,m={more};{chunk}\x1b\\"
                    );
                }
            }
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};{chunk}\x1b\\");
        }
    }
    out
}

fn is_tall_scrollable_image(
    width_px: u32,
    height_px: u32,
    vp_w: u32,
    vp_h: u32,
    scrollable: bool,
) -> bool {
    scrollable
        && (height_px as u128) * 4 > (width_px as u128) * 5
        && (height_px as u128) * (vp_w as u128) * 2 > (width_px as u128) * (vp_h as u128) * 3
}

pub(crate) fn compute_halfblock_dims(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
) -> (u32, u32) {
    let cell_w = cell_px.0.max(1);
    let cell_h = cell_px.1.max(1);
    let view_cols = fit.view_cols.max(1);
    let view_rows = fit.view_rows.max(1);
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    let vp_px_w = (view_cols as u32).saturating_mul(cell_w as u32).max(1);
    let vp_px_h = (view_rows as u32).saturating_mul(cell_h as u32).max(1);
    let subpx_aspect = (cell_h as f64) / (2.0 * cell_w as f64);
    let eff_src_w = width_px as f64;
    let eff_src_h = (height_px as f64) / subpx_aspect;
    let vp_sub_w = view_cols as f64;
    let vp_sub_h = (view_rows as f64) * 2.0;

    let base_scale = if is_tall_scrollable_image(width_px, height_px, vp_px_w, vp_px_h, fit.scrollable) {
        (vp_sub_w / eff_src_w).min(1.0)
    } else {
        (vp_sub_w / eff_src_w).min(vp_sub_h / eff_src_h).min(1.0)
    };

    const MAX_HALFBLOCK_COLS: f64 = 1024.0;
    const MAX_HALFBLOCK_SUB_H: f64 = 2048.0;
    let scale = (base_scale * (zoom_pct as f64 / 100.0))
        .min(MAX_HALFBLOCK_COLS / eff_src_w)
        .min(MAX_HALFBLOCK_SUB_H / eff_src_h);

    let mut target_w = ((eff_src_w * scale).round() as u32).max(1);
    let mut target_h = ((eff_src_h * scale).round() as u32).max(2);

    if !fit.scrollable && zoom_pct <= 100 {
        target_w = target_w.min(view_cols as u32);
        target_h = target_h.min((view_rows as u32) * 2).max(2);
    }

    if target_h % 2 != 0 {
        if !fit.scrollable && zoom_pct <= 100 && target_h + 1 > (view_rows as u32) * 2 {
            target_h = target_h.saturating_sub(1).max(2);
        } else {
            target_h += 1;
        }
    }

    (target_w, target_h)
}

pub(crate) fn compute_pixel_fit_for_protocol(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
    protocol: TerminalImageProtocol,
) -> (u32, u32, u16, u16) {
    let cell_w = cell_px.0.max(1);
    let cell_h = cell_px.1.max(1);
    let view_cols = fit.view_cols.max(1);
    let view_rows = fit.view_rows.max(1);
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    let vp_w = (view_cols as u32).saturating_mul(cell_w as u32).max(1);
    let raw_vp_h = (view_rows as u32).saturating_mul(cell_h as u32).max(1);
    let vp_h = if protocol == TerminalImageProtocol::Sixel && !fit.scrollable {
        let sixel_safe_h = compute_sixel_safe_pixel_height(view_rows, cell_h);
        if sixel_safe_h >= 6 {
            sixel_safe_h.min(raw_vp_h)
        } else {
            raw_vp_h
        }
    } else {
        raw_vp_h
    };

    let base_scale = if is_tall_scrollable_image(width_px, height_px, vp_w, vp_h, fit.scrollable) {
        (vp_w as f64 / width_px as f64).min(1.0)
    } else {
        (vp_w as f64 / width_px as f64)
            .min(vp_h as f64 / height_px as f64)
            .min(1.0)
    };

    let max_scale_w = MAX_RENDER_PX_W as f64 / width_px as f64;
    let max_scale_h = MAX_RENDER_PX_H as f64 / height_px as f64;
    let scale = (base_scale * (zoom_pct as f64 / 100.0))
        .min(max_scale_w)
        .min(max_scale_h);

    let mut target_px_w = ((width_px as f64 * scale).round() as u32).clamp(1, MAX_RENDER_PX_W);
    let mut target_px_h = ((height_px as f64 * scale).round() as u32).clamp(1, MAX_RENDER_PX_H);

    if protocol == TerminalImageProtocol::Sixel {
        let pixels = (target_px_w as u64).saturating_mul(target_px_h as u64);
        if pixels > MAX_SIXEL_PIXELS {
            let shrink = (MAX_SIXEL_PIXELS as f64 / pixels as f64).sqrt();
            target_px_w = ((target_px_w as f64 * shrink).floor() as u32).max(1);
            target_px_h = ((target_px_h as f64 * shrink).floor() as u32).max(1);
        }
        if !fit.scrollable {
            if target_px_h > vp_h {
                target_px_h = vp_h;
            }
            if target_px_h.div_ceil(6) * 6 > vp_h && vp_h >= 6 {
                target_px_h = vp_h - (vp_h % 6);
            }
        }
    }

    let occ_cols = target_px_w
        .div_ceil(cell_w as u32)
        .clamp(1, u16::MAX as u32) as u16;
    let occ_rows = if protocol == TerminalImageProtocol::Sixel && !fit.scrollable {
        let band_px_h = target_px_h.div_ceil(6) * 6;
        band_px_h.div_ceil(cell_h as u32).clamp(1, view_rows as u32) as u16
    } else if !fit.scrollable && zoom_pct <= 100 {
        target_px_h.div_ceil(cell_h as u32).clamp(1, view_rows as u32) as u16
    } else {
        target_px_h
            .div_ceil(cell_h as u32)
            .clamp(1, u16::MAX as u32) as u16
    };

    (target_px_w, target_px_h, occ_cols, occ_rows)
}

fn fit_within_max_bounds(width_px: u32, height_px: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width_px <= max_w && height_px <= max_h {
        return (width_px, height_px);
    }
    let scale = (max_w as f64 / width_px as f64).min(max_h as f64 / height_px as f64);
    let w = ((width_px as f64 * scale).round() as u32).clamp(1, max_w);
    let h = ((height_px as f64 * scale).round() as u32).clamp(1, max_h);
    (w, h)
}

/// Encodes opaque RGBA pixel data (`4 * width * height` bytes) into a standard DEC VT340 / Windows Terminal Sixel stream.
pub fn encode_sixel_rgba(flat_rgba: &[u8], width: u32, height: u32) -> String {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || flat_rgba.len() < w * h * 4 {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let (palette, indexed) = quantize_rgba_to_256(flat_rgba, w, h);
    encode_sixel_indexed_slice(&palette, &indexed, w, 0, 0, w, h)
}

fn encode_sixel_indexed_slice(
    palette: &[(u8, u8, u8)],
    indexed: &[u8],
    full_w: usize,
    x0: usize,
    y0: usize,
    crop_w: usize,
    crop_h: usize,
) -> String {
    if crop_w == 0 || crop_h == 0 || full_w == 0 || palette.is_empty() {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let mut out = String::with_capacity(crop_w * crop_h / 2 + palette.len() * 16 + 32);
    let _ = write!(out, "\x1bP0;1;q\"1;1;{crop_w};{crop_h}");

    for (idx, &(r, g, b)) in palette.iter().enumerate() {
        let r_pct = (r as u32 * 100 + 127) / 255;
        let g_pct = (g as u32 * 100 + 127) / 255;
        let b_pct = (b as u32 * 100 + 127) / 255;
        let _ = write!(out, "#{idx};2;{r_pct};{g_pct};{b_pct}");
    }

    let mut color_seen = [false; 256];
    let mut band_colors: Vec<u8> = Vec::with_capacity(palette.len().min(64));
    let mut band_masks = vec![0u8; palette.len().max(1) * crop_w];

    for band_y in (0..crop_h).step_by(6) {
        band_colors.clear();
        let band_h = (crop_h - band_y).min(6);

        for dy in 0..band_h {
            let bit = 1u8 << dy;
            let row_offset = (y0 + band_y + dy) * full_w + x0;
            for x in 0..crop_w {
                let c = indexed[row_offset + x];
                if !color_seen[c as usize] {
                    color_seen[c as usize] = true;
                    band_colors.push(c);
                }
                band_masks[c as usize * crop_w + x] |= bit;
            }
        }

        for (ci, &color_idx) in band_colors.iter().enumerate() {
            if ci > 0 {
                out.push('$');
            }
            let _ = write!(out, "#{color_idx}");

            let mask_start = color_idx as usize * crop_w;
            let row_slice = &band_masks[mask_start..mask_start + crop_w];
            let end = row_slice
                .iter()
                .rposition(|&bits| bits != 0)
                .map_or(0, |idx| idx + 1);

            write_sixel_rle(&row_slice[..end], &mut out);
        }

        for &c in &band_colors {
            color_seen[c as usize] = false;
            let start = c as usize * crop_w;
            band_masks[start..start + crop_w].fill(0);
        }

        if band_y + 6 < crop_h {
            out.push('-');
        }
    }

    out.push_str("\x1b\\");
    out
}

fn write_sixel_rle(masks: &[u8], out: &mut String) {
    let mut i = 0;
    while i < masks.len() {
        let bits = masks[i];
        let mut run_len = 1usize;
        while i + run_len < masks.len() && masks[i + run_len] == bits {
            run_len += 1;
        }
        let ch = (bits + 63) as char;
        if run_len >= 4 {
            let _ = write!(out, "!{run_len}{ch}");
        } else {
            for _ in 0..run_len {
                out.push(ch);
            }
        }
        i += run_len;
    }
}

#[derive(Clone, Copy)]
struct ColorBucket {
    r_avg: u8,
    g_avg: u8,
    b_avg: u8,
    count: u32,
    sum_r: u64,
    sum_g: u64,
    sum_b: u64,
}

/// Integer Redmean perceptual RGB color distance squared (green & luminance weighted).
pub(crate) fn redmean_dist_sq(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let r_mean = (r1 as u32 + r2 as u32) / 2;
    let dr = r1.abs_diff(r2) as u32;
    let dg = g1.abs_diff(g2) as u32;
    let db = b1.abs_diff(b2) as u32;
    (((512 + r_mean) * dr * dr) >> 8) + 4 * dg * dg + (((767 - r_mean) * db * db) >> 8)
}

fn isqrt_ceil(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let r = (n as f64).sqrt().ceil().max(1.0) as u64;
    if (r - 1).saturating_mul(r - 1) >= n {
        r - 1
    } else if r.saturating_mul(r) < n {
        r + 1
    } else {
        r
    }
}

fn quantize_rgba_to_256(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let (palette, indexed, _) = quantize_rgba_to_256_with_stats(flat_rgba, width, height);
    (palette, indexed)
}

fn quantize_rgba_to_256_with_stats(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>, usize) {
    let pixel_count = width.saturating_mul(height);
    // Fast lossless path when the image uses <= 256 distinct RGB colors.
    let mut exact_map: HashMap<u32, u8> = HashMap::with_capacity(256);
    let mut exact_palette: Vec<(u8, u8, u8)> = Vec::with_capacity(256);
    let mut indexed = Vec::with_capacity(pixel_count);
    let mut overflow = false;

    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
        let key = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32);
        if let Some(&idx) = exact_map.get(&key) {
            indexed.push(idx);
        } else if exact_palette.len() < 256 {
            let idx = exact_palette.len() as u8;
            exact_map.insert(key, idx);
            exact_palette.push((px[0], px[1], px[2]));
            indexed.push(idx);
        } else {
            overflow = true;
            break;
        }
    }

    if !overflow {
        return (exact_palette, indexed, exact_map.len());
    }

    // Perceptually-weighted median-cut over 5-bit-per-channel RGB buckets for > 256 distinct colors.
    let mut hist: HashMap<u16, (u32, u64, u64, u64)> = HashMap::new();
    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
        let qkey = (((px[0] as u16) >> 3) << 10)
            | (((px[1] as u16) >> 3) << 5)
            | ((px[2] as u16) >> 3);
        let entry = hist.entry(qkey).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += px[0] as u64;
        entry.2 += px[1] as u64;
        entry.3 += px[2] as u64;
    }

    let mut buckets: Vec<ColorBucket> = hist
        .into_values()
        .map(|(count, sum_r, sum_g, sum_b)| {
            let c = count as u64;
            ColorBucket {
                r_avg: (sum_r / c) as u8,
                g_avg: (sum_g / c) as u8,
                b_avg: (sum_b / c) as u8,
                count,
                sum_r,
                sum_g,
                sum_b,
            }
        })
        .collect();

    let total_buckets = buckets.len();
    let mut boxes: Vec<(usize, usize)> = vec![(0, total_buckets)];

    while boxes.len() < 256 {
        let mut best_box_idx = None;
        let mut best_score = 0u64;
        let mut best_channel = 0u8;

        for (bi, &(start, end)) in boxes.iter().enumerate() {
            if end - start <= 1 {
                continue;
            }
            let (mut min_r, mut max_r) = (u8::MAX, u8::MIN);
            let (mut min_g, mut max_g) = (u8::MAX, u8::MIN);
            let (mut min_b, mut max_b) = (u8::MAX, u8::MIN);
            let mut pop = 0u64;

            for b in &buckets[start..end] {
                min_r = min_r.min(b.r_avg);
                max_r = max_r.max(b.r_avg);
                min_g = min_g.min(b.g_avg);
                max_g = max_g.max(b.g_avg);
                min_b = min_b.min(b.b_avg);
                max_b = max_b.max(b.b_avg);
                pop += b.count as u64;
            }

            let w_span_r = (max_r - min_r) as u64 * 2;
            let w_span_g = (max_g - min_g) as u64 * 4;
            let w_span_b = (max_b - min_b) as u64 * 3;
            let (channel, weighted_span) = if w_span_g >= w_span_r && w_span_g >= w_span_b {
                (1u8, w_span_g)
            } else if w_span_b >= w_span_r {
                (2u8, w_span_b)
            } else {
                (0u8, w_span_r)
            };

            let score = weighted_span.saturating_mul(isqrt_ceil(pop));
            if score > best_score {
                best_score = score;
                best_box_idx = Some(bi);
                best_channel = channel;
            }
        }

        let Some(bi) = best_box_idx else {
            break;
        };
        let (start, end) = boxes[bi];
        let slice = &mut buckets[start..end];
        match best_channel {
            0 => slice.sort_unstable_by_key(|b| b.r_avg),
            1 => slice.sort_unstable_by_key(|b| b.g_avg),
            _ => slice.sort_unstable_by_key(|b| b.b_avg),
        }

        let total_pop: u64 = slice.iter().map(|b| b.count as u64).sum();
        let half_pop = total_pop / 2;
        let mut accum = 0u64;
        let mut split_rel = (slice.len() / 2).max(1);
        for (idx, b) in slice.iter().enumerate() {
            accum += b.count as u64;
            if accum >= half_pop {
                split_rel = if idx + 1 < slice.len() {
                    (idx + 1).max(1)
                } else {
                    idx.max(1)
                };
                break;
            }
        }
        let mid = start + split_rel;
        boxes[bi] = (start, mid);
        boxes.push((mid, end));
    }

    let raw_palette: Vec<(u8, u8, u8)> = boxes
        .iter()
        .map(|&(start, end)| {
            let mut cnt = 0u64;
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            for b in &buckets[start..end] {
                cnt += b.count as u64;
                sr += b.sum_r;
                sg += b.sum_g;
                sb += b.sum_b;
            }
            if cnt == 0 {
                (0, 0, 0)
            } else {
                ((sr / cnt) as u8, (sg / cnt) as u8, (sb / cnt) as u8)
            }
        })
        .collect();

    indexed.clear();
    indexed.resize(pixel_count, 0);
    let mut lookup_cache: HashMap<u32, u8> = HashMap::with_capacity(4096);
    let mut used = vec![false; raw_palette.len()];

    // Serpentine Floyd-Steinberg error diffusion with [-48, 48] error clamp and flat-region guard.
    let mut work: Vec<[f32; 3]> = flat_rgba
        .chunks_exact(4)
        .take(pixel_count)
        .map(|px| [px[0] as f32, px[1] as f32, px[2] as f32])
        .collect();

    let orig_rgb = |p: usize| -> (u8, u8, u8) {
        let o = p * 4;
        (flat_rgba[o], flat_rgba[o + 1], flat_rgba[o + 2])
    };

    for y in 0..height {
        let row_base = y * width;
        let reverse = (y & 1) == 1;
        for step in 0..width {
            let x = if reverse { width - 1 - step } else { step };
            let pos = row_base + x;
            let [rf, gf, bf] = work[pos];
            let r = rf.round().clamp(0.0, 255.0) as u8;
            let g = gf.round().clamp(0.0, 255.0) as u8;
            let b = bf.round().clamp(0.0, 255.0) as u8;

            let key = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let best_idx = if let Some(&cached) = lookup_cache.get(&key) {
                cached
            } else {
                let mut best_i = 0u8;
                let mut best_dist = u32::MAX;
                for (pi, &(pr, pg, pb)) in raw_palette.iter().enumerate() {
                    let dist = redmean_dist_sq(r, g, b, pr, pg, pb);
                    if dist < best_dist {
                        best_dist = dist;
                        best_i = pi as u8;
                        if dist == 0 {
                            break;
                        }
                    }
                }
                if lookup_cache.len() < MAX_QUANT_LOOKUP_CACHE {
                    lookup_cache.insert(key, best_i);
                }
                best_i
            };

            used[best_idx as usize] = true;
            indexed[pos] = best_idx;

            let (pr, pg, pb) = raw_palette[best_idx as usize];
            let er = (r as f32 - pr as f32).clamp(-48.0, 48.0);
            let eg = (g as f32 - pg as f32).clamp(-48.0, 48.0);
            let eb = (b as f32 - pb as f32).clamp(-48.0, 48.0);

            if er.abs() <= 3.0 && eg.abs() <= 3.0 && eb.abs() <= 3.0 {
                continue;
            }
            let cur_orig = orig_rgb(pos);
            let flat_neighbor = (x > 0 && orig_rgb(pos - 1) == cur_orig)
                || (y > 0 && orig_rgb(pos - width) == cur_orig)
                || (x == 0 && x + 1 < width && orig_rgb(pos + 1) == cur_orig)
                || (y == 0 && y + 1 < height && orig_rgb(pos + width) == cur_orig);
            if flat_neighbor && redmean_dist_sq(r, g, b, pr, pg, pb) <= 64 {
                continue;
            }

            let diffuse = |buf: &mut [[f32; 3]], idx: usize, weight: f32| {
                let p = &mut buf[idx];
                p[0] = (p[0] + er * weight).clamp(0.0, 255.0);
                p[1] = (p[1] + eg * weight).clamp(0.0, 255.0);
                p[2] = (p[2] + eb * weight).clamp(0.0, 255.0);
            };

            if !reverse {
                if x + 1 < width {
                    diffuse(&mut work, pos + 1, 7.0 / 16.0);
                }
                if y + 1 < height {
                    let next_row = pos + width;
                    if x > 0 {
                        diffuse(&mut work, next_row - 1, 3.0 / 16.0);
                    }
                    diffuse(&mut work, next_row, 5.0 / 16.0);
                    if x + 1 < width {
                        diffuse(&mut work, next_row + 1, 1.0 / 16.0);
                    }
                }
            } else {
                if x > 0 {
                    diffuse(&mut work, pos - 1, 7.0 / 16.0);
                }
                if y + 1 < height {
                    let next_row = pos + width;
                    if x + 1 < width {
                        diffuse(&mut work, next_row + 1, 3.0 / 16.0);
                    }
                    diffuse(&mut work, next_row, 5.0 / 16.0);
                    if x > 0 {
                        diffuse(&mut work, next_row - 1, 1.0 / 16.0);
                    }
                }
            }
        }
    }

    let lookup_cache_len = lookup_cache.len();

    // Compact palette so every emitted `#idx;2;...` entry is actually referenced.
    let mut remap = vec![0u8; raw_palette.len()];
    let mut compact_palette = Vec::with_capacity(raw_palette.len());
    for (old_idx, &color) in raw_palette.iter().enumerate() {
        if used[old_idx] {
            remap[old_idx] = compact_palette.len() as u8;
            compact_palette.push(color);
        }
    }
    for idx in &mut indexed {
        *idx = remap[*idx as usize];
    }

    (compact_palette, indexed, lookup_cache_len)
}

/// Renders an image for CLI stdout, auto-detecting Kitty / iTerm2 / Sixel / ANSI 24-bit Half-Blocks.
pub fn render_for_stdout(bytes: &[u8], max_cols: u16, max_rows: u16) -> Result<String> {
    render_ansi_stdout(bytes, max_cols, max_rows)
}

/// Renders an image for CLI stdout using an explicit legacy graphics protocol.
pub fn render_for_stdout_with_protocol(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    protocol: TerminalGraphicsProtocol,
) -> Result<String> {
    let caps = TerminalGraphicsCaps {
        protocol: protocol.into(),
        ..Default::default()
    };
    render_ansi_stdout_with_caps(bytes, max_cols, max_rows, caps)
}

/// Renders an image for direct CLI terminal stdout (auto-detecting Kitty / iTerm2 / Sixel / 24-bit ANSI Half-Blocks).
pub fn render_ansi_stdout(bytes: &[u8], max_cols: u16, max_rows: u16) -> Result<String> {
    render_ansi_stdout_with_pref(bytes, max_cols, max_rows, ImageProtocolPreference::Auto)
}

/// Renders an image for direct CLI terminal stdout honoring an explicit or auto-detected protocol preference.
pub fn render_ansi_stdout_with_pref(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    pref: ImageProtocolPreference,
) -> Result<String> {
    let caps = detect_graphics_caps(pref);
    render_ansi_stdout_with_caps(bytes, max_cols, max_rows, caps)
}

/// Renders an image for direct CLI terminal stdout using pre-probed terminal graphics capabilities.
pub fn render_ansi_stdout_with_caps(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    caps: TerminalGraphicsCaps,
) -> Result<String> {
    reject_non_raster_markup_bytes(bytes)?;
    let fit = FitBox::fit(max_cols, max_rows);
    match caps.protocol {
        TerminalImageProtocol::ITerm2 => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let (cols, _) = compute_halfblock_dims(width_px, height_px, fit, caps.cell_px);
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && bytes.len() <= MAX_ITERM2_RAW_BYTES
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                bytes
            } else {
                let (fit_w, fit_h) =
                    fit_within_max_bounds(width_px, height_px, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                encoded_owned = if fit_w == width_px && fit_h == height_px {
                    encode_png(&dyn_img)?
                } else {
                    let resized = dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3);
                    encode_png(&resized)?
                };
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            Ok(format!(
                "\x1b]1337;File=inline=1;width={cols};preserveAspectRatio=1:{b64}\x07\n"
            ))
        }
        TerminalImageProtocol::Kitty => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                bytes
            } else if width_px <= MAX_RENDER_PX_W && height_px <= MAX_RENDER_PX_H {
                png_owned = encode_png(&dyn_img)?;
                &png_owned
            } else {
                let (fit_w, fit_h) =
                    fit_within_max_bounds(width_px, height_px, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                let resized = dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3);
                png_owned = encode_png(&resized)?;
                &png_owned
            };
            let b64 = B64_STANDARD.encode(png_bytes);
            let chunks: Vec<&str> = b64
                .as_bytes()
                .chunks(4096)
                .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
                .collect();
            let total = chunks.len();
            let mut out = String::new();
            for (idx, chunk) in chunks.into_iter().enumerate() {
                let more = if idx + 1 < total { 1 } else { 0 };
                if idx == 0 {
                    let _ = write!(out, "\x1b_Gf=100,a=T,m={more};{chunk}\x1b\\");
                } else {
                    let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
                }
            }
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Sixel => {
            let rendered = decode_and_render_for_protocol(
                bytes,
                fit,
                DEFAULT_PREVIEW_BG_RGB,
                caps,
            )?;
            let mut out = rendered.pixel_payload.unwrap_or_default();
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Halfblock => {
            let rendered = decode_and_render_halfblocks_with_cell_px(
                bytes,
                fit,
                DEFAULT_PREVIEW_BG_RGB,
                caps.cell_px,
            )?;
            let mut out = String::new();
            for line in &rendered.lines {
                for span in &line.spans {
                    let (tr, tg, tb) = match span.style.fg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (255, 255, 255),
                    };
                    let (br, bg_c, bb) = match span.style.bg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => DEFAULT_PREVIEW_BG_RGB,
                    };
                    let _ = write!(
                        out,
                        "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_c};{bb}m{}",
                        span.content
                    );
                }
                out.push_str("\x1b[0m\n");
            }
            Ok(out)
        }
    }
}

fn format_label_for(fmt: Option<ImageFormat>) -> String {
    match fmt {
        Some(ImageFormat::Png) => "PNG".to_string(),
        Some(ImageFormat::Jpeg) => "JPEG".to_string(),
        Some(ImageFormat::Gif) => "GIF".to_string(),
        Some(ImageFormat::WebP) => "WEBP".to_string(),
        Some(other) => format!("{other:?}").to_ascii_uppercase(),
        None => "IMAGE".to_string(),
    }
}

fn blend_over_bg(px: Rgba<u8>, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let a = px[3] as u16;
    if a == 255 {
        return (px[0], px[1], px[2]);
    }
    if a == 0 {
        return bg;
    }
    let inv = 255 - a;
    let r = ((px[0] as u16 * a + bg.0 as u16 * inv + 127) / 255) as u8;
    let g = ((px[1] as u16 * a + bg.1 as u16 * inv + 127) / 255) as u8;
    let b = ((px[2] as u16 * a + bg.2 as u16 * inv + 127) / 255) as u8;
    (r, g, b)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)
        .context("Failed to encode PNG")?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, RgbaImage};

    fn make_test_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            if y < h / 2 {
                Rgba([(x * 16) as u8, 120, 40, 255])
            } else {
                Rgba([20, 40, 200, 128])
            }
        });
        encode_png(&DynamicImage::ImageRgba8(img)).unwrap()
    }

    fn make_dummy_rendered(payload_len: usize, proto: TerminalImageProtocol) -> RenderedTerminalImage {
        RenderedTerminalImage {
            width_px: 100,
            height_px: 50,
            cols: 20,
            rows: 10,
            format_label: "PNG".to_string(),
            protocol: proto,
            lines: Vec::new(),
            pixel_payload: Some("a".repeat(payload_len)),
            scroll_backing: None,
        }
    }

    #[test]
    fn test_is_image_attachment() {
        assert!(is_image_attachment("image/png", "shot.bin"));
        assert!(is_image_attachment("application/octet-stream", "photo.WEBP"));
        assert!(is_image_mime_or_name("image/jpeg", None));
        assert!(is_image_mime_or_name("application/octet-stream", Some("pic.GIF")));
        assert!(is_image_attachment("image/svg+xml", "diagram.svg"));
        assert!(is_image_mime_or_name("image/svg+xml", Some("diagram.svg")));
        assert!(is_image_mime_or_name("application/octet-stream", Some("vector.SVG")));
        assert!(!is_image_attachment("application/pdf", "spec.pdf"));
    }

    #[test]
    fn test_render_halfblocks_aspect_coalescing_and_alpha() {
        let png = make_test_png(16, 8);
        let rendered = render_halfblocks(&png, 20, 10).unwrap();
        assert_eq!(rendered.width_px, 16);
        assert_eq!(rendered.height_px, 8);
        assert_eq!(rendered.cols, 16);
        assert_eq!(rendered.rows, 4);
        assert_eq!(rendered.format_label, "PNG");
        assert_eq!(rendered.protocol, TerminalImageProtocol::Halfblock);
        assert!(rendered.pixel_payload.is_none());
        assert_eq!(rendered.lines.len(), 4);

        for line in &rendered.lines {
            let char_count: usize = line
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(char_count, 16);
            assert!(line.spans.iter().all(|s| s.content.chars().all(|c| c == '▀')));
        }

        // Transparent pixel composites onto #1E1F22 (30, 31, 34)
        let transparent: RgbaImage = ImageBuffer::from_pixel(10, 4, Rgba([0, 0, 0, 0]));
        let transparent_png = encode_png(&DynamicImage::ImageRgba8(transparent)).unwrap();
        let tr_rendered = render_halfblocks(&transparent_png, 20, 10).unwrap();
        assert_eq!(tr_rendered.lines.len(), 2);
        assert_eq!(tr_rendered.lines[0].spans.len(), 1);
        assert_eq!(tr_rendered.lines[0].spans[0].content, "▀▀▀▀▀▀▀▀▀▀");
        assert_eq!(
            tr_rendered.lines[0].spans[0].style.fg,
            Some(Color::Rgb(0x1E, 0x1F, 0x22))
        );
        assert_eq!(
            tr_rendered.lines[0].spans[0].style.bg,
            Some(Color::Rgb(0x1E, 0x1F, 0x22))
        );
    }

    #[test]
    fn test_protocol_auto_detection_matrix() {
        let empty = HashMap::new();
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &empty),
            TerminalImageProtocol::Kitty
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Iterm2, &empty),
            TerminalImageProtocol::ITerm2
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Sixel, &empty),
            TerminalImageProtocol::Sixel
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Halfblock, &empty),
            TerminalImageProtocol::Halfblock
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &empty),
            TerminalImageProtocol::Halfblock
        );

        // ATLASSIAN_IMAGE_PROTOCOL overrides everything including TMUX and explicit pref
        let mut env_override = HashMap::new();
        env_override.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        env_override.insert("ATLASSIAN_IMAGE_PROTOCOL", "sixel");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::Sixel
        );
        env_override.insert("ATLASSIAN_IMAGE_PROTOCOL", "halfblocks");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &env_override),
            TerminalImageProtocol::Halfblock
        );
        env_override.insert("ATLASSIAN_IMAGE_PROTOCOL", "iterm");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::ITerm2
        );

        // TMUX / STY guard forces Halfblock in Auto mode even when KITTY_WINDOW_ID / WT_SESSION are set
        let mut tmux_env = HashMap::new();
        tmux_env.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        tmux_env.insert("KITTY_WINDOW_ID", "1");
        tmux_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &tmux_env),
            TerminalImageProtocol::Halfblock
        );

        let mut screen_env = HashMap::new();
        screen_env.insert("STY", "1234.pts-0.host");
        screen_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &screen_env),
            TerminalImageProtocol::Halfblock
        );

        // Kitty & Ghostty detection
        let mut kitty_env = HashMap::new();
        kitty_env.insert("KITTY_WINDOW_ID", "42");
        kitty_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &kitty_env),
            TerminalImageProtocol::Kitty
        );

        let mut ghostty_env = HashMap::new();
        ghostty_env.insert("TERM_PROGRAM", "Ghostty");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &ghostty_env),
            TerminalImageProtocol::Kitty
        );

        let mut ghostty_dir = HashMap::new();
        ghostty_dir.insert("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &ghostty_dir),
            TerminalImageProtocol::Kitty
        );

        // WezTerm + WT_SESSION -> Kitty wins over iTerm2 and WT_SESSION
        let mut wez_wt_env = HashMap::new();
        wez_wt_env.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        wez_wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_wt_env),
            TerminalImageProtocol::Kitty
        );

        let mut iterm_env = HashMap::new();
        iterm_env.insert("TERM_PROGRAM", "iTerm.app");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &iterm_env),
            TerminalImageProtocol::ITerm2
        );

        // WT_SESSION alone -> Halfblock (requires live DA1 `;4` probe to confirm Sixel)
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Halfblock
        );

        // WT_SESSION inside VS Code / Cursor / Windsurf integrated terminal -> Halfblock
        let mut wt_vscode = HashMap::new();
        wt_vscode.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        wt_vscode.insert("TERM_PROGRAM", "vscode");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_vscode),
            TerminalImageProtocol::Halfblock
        );

        let mut wt_vscode_pid = HashMap::new();
        wt_vscode_pid.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        wt_vscode_pid.insert("VSCODE_PID", "9876");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_vscode_pid),
            TerminalImageProtocol::Halfblock
        );

        // Linux Sixel terminals
        let mut foot_env = HashMap::new();
        foot_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &foot_env),
            TerminalImageProtocol::Sixel
        );
    }

    #[test]
    fn test_sixel_encoder_solid_and_multiband_and_gradient() {
        let solid: RgbaImage = ImageBuffer::from_pixel(10, 8, Rgba([255, 0, 0, 255]));
        let solid_png = encode_png(&DynamicImage::ImageRgba8(solid)).unwrap();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let rendered =
            decode_and_render_for_protocol(&solid_png, FitBox::fit(40, 20), (0, 0, 0), caps)
                .unwrap();
        assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
        assert!(rendered.lines.is_empty());
        let payload = rendered.pixel_payload.as_deref().unwrap();
        assert!(payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(payload.contains("#0;2;100;0;0"));
        assert!(payload.contains('!'));
        assert!(payload.contains('-'));
        assert!(payload.ends_with("\x1b\\"));
        assert!(!payload.ends_with('\n'));
        assert!(rendered.approx_heap_bytes() > std::mem::size_of::<RenderedTerminalImage>());

        // Multi-color (>256 distinct RGB colors) gradient exercises median-cut quantization
        let grad: RgbaImage = ImageBuffer::from_fn(32, 16, |x, y| {
            Rgba([
                ((x * 8) % 256) as u8,
                ((y * 16) % 256) as u8,
                (((x + y) * 5) % 256) as u8,
                if y < 8 { 255 } else { 128 },
            ])
        });
        let grad_png = encode_png(&DynamicImage::ImageRgba8(grad)).unwrap();
        let grad_rendered =
            decode_and_render_for_protocol(&grad_png, FitBox::fit(20, 10), (12, 24, 36), caps)
                .unwrap();
        let grad_payload = grad_rendered.pixel_payload.as_deref().unwrap();
        assert!(grad_payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(grad_payload.contains(";2;"));
        assert!(grad_payload.contains('-'));
        assert!(grad_payload.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_and_iterm2_tui_vs_cli_stdout_payloads() {
        let png = make_test_png(16, 8);

        // TUI Kitty payload: fixed image id 4242, q=2, single constraining axis (c=... without r=), C=1, no trailing newline
        let kitty_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let kitty_tui =
            decode_and_render_for_protocol(&png, FitBox::fit(20, 10), (0, 0, 0), kitty_caps)
                .unwrap();
        let kitty_payload = kitty_tui.pixel_payload.as_deref().unwrap();
        assert!(kitty_payload.starts_with(&format!(
            "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={},C=1,m=",
            kitty_tui.cols
        )));
        assert!(!kitty_payload.contains(",r="));
        assert!(kitty_payload.ends_with("\x1b\\"));
        assert!(!kitty_payload.ends_with('\n'));
        assert_eq!(
            kitty_delete_preview_sequence(),
            "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
        );

        // TUI iTerm2 payload: doNotMoveCursor=1, no trailing newline
        let iterm_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::ITerm2,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let iterm_tui =
            decode_and_render_for_protocol(&png, FitBox::fit(20, 10), (0, 0, 0), iterm_caps)
                .unwrap();
        let iterm_payload = iterm_tui.pixel_payload.as_deref().unwrap();
        assert!(iterm_payload.starts_with(&format!(
            "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:",
            iterm_tui.cols, iterm_tui.rows
        )));
        assert!(iterm_payload.ends_with('\x07'));
        assert!(!iterm_payload.ends_with('\n'));

        // CLI stdout payloads: omit C=1 and doNotMoveCursor=1, end with '\n'
        let kitty_cli = render_ansi_stdout_with_caps(&png, 16, 8, kitty_caps).unwrap();
        assert!(kitty_cli.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(!kitty_cli.contains("C=1"));
        assert!(kitty_cli.ends_with("\x1b\\\n"));

        let iterm_cli = render_ansi_stdout_with_caps(&png, 16, 8, iterm_caps).unwrap();
        assert!(iterm_cli.starts_with("\x1b]1337;File=inline=1;"));
        assert!(!iterm_cli.contains("doNotMoveCursor=1"));
        assert!(iterm_cli.ends_with("\x07\n"));

        let sixel_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let sixel_cli = render_ansi_stdout_with_caps(&png, 16, 8, sixel_caps).unwrap();
        assert!(sixel_cli.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(sixel_cli.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_render_for_stdout_protocols() {
        let png = make_test_png(8, 4);

        let half = render_for_stdout_with_protocol(
            &png,
            16,
            8,
            TerminalGraphicsProtocol::HalfBlocks,
        )
        .unwrap();
        assert!(half.contains("\x1b[38;2;"));
        assert!(half.contains("\x1b[48;2;"));
        assert!(half.contains('▀'));
        assert!(half.ends_with("\x1b[0m\n"));

        let iterm = render_for_stdout_with_protocol(
            &png,
            16,
            8,
            TerminalGraphicsProtocol::ITerm2,
        )
        .unwrap();
        assert!(iterm.starts_with("\x1b]1337;File=inline=1;width=8;preserveAspectRatio=1:"));
        assert!(iterm.ends_with("\x07\n"));

        let kitty = render_for_stdout_with_protocol(
            &png,
            16,
            8,
            TerminalGraphicsProtocol::Kitty,
        )
        .unwrap();
        assert!(kitty.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(kitty.ends_with("\x1b\\\n"));

        let sixel = render_for_stdout_with_protocol(
            &png,
            16,
            8,
            TerminalGraphicsProtocol::Sixel,
        )
        .unwrap();
        assert!(sixel.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(sixel.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_decoded_image_cache_eviction_by_byte_budget_and_entry_limit() {
        assert_eq!(MAX_IMAGE_CACHE_BYTES, 8 * 1024 * 1024);

        let sample = make_dummy_rendered(1024, TerminalImageProtocol::Sixel);
        let one_size = sample.approx_heap_bytes();
        // Budget fits two entries comfortably, but not three.
        let mut cache = DecodedImageCache::with_limits(one_size * 2 + 16, 16);

        assert!(cache.insert("k1".into(), sample.clone()).is_empty());
        assert!(cache.insert("k2".into(), sample.clone()).is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_bytes(), one_size * 2);

        // Inserting k3 evicts oldest key k1.
        let evicted = cache.insert("k3".into(), sample.clone());
        assert_eq!(evicted, vec!["k1".to_string()]);
        assert!(!cache.contains_key("k1"));
        assert!(cache.contains_key("k2"));
        assert!(cache.contains_key("k3"));
        assert!(cache.total_bytes() <= cache.max_bytes());

        // Replacing k2 updates order so k3 becomes oldest.
        assert!(cache.insert("k2".into(), sample.clone()).is_empty());
        let evicted2 = cache.insert("k4".into(), sample);
        assert_eq!(evicted2, vec!["k3".to_string()]);
        assert!(!cache.contains_key("k3"));
        assert!(cache.contains_key("k2"));
        assert!(cache.contains_key("k4"));

        // Oversized single entry evicts all older entries but keeps itself.
        let huge = make_dummy_rendered(one_size * 4, TerminalImageProtocol::Kitty);
        let evicted_huge = cache.insert("oversized".into(), huge);
        assert_eq!(evicted_huge, vec!["k2".to_string(), "k4".to_string()]);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key("oversized"));

        // Entry-count limit enforcement.
        let mut count_cache = DecodedImageCache::with_limits(MAX_IMAGE_CACHE_BYTES, 2);
        let tiny = make_dummy_rendered(32, TerminalImageProtocol::ITerm2);
        assert!(count_cache.insert("a".into(), tiny.clone()).is_empty());
        assert!(count_cache.insert("b".into(), tiny.clone()).is_empty());
        assert_eq!(count_cache.insert("c".into(), tiny), vec!["a".to_string()]);
        assert_eq!(count_cache.len(), 2);

        count_cache.clear();
        assert!(count_cache.is_empty());
        assert_eq!(count_cache.total_bytes(), 0);
    }

    #[test]
    fn test_decode_rejects_invalid_bytes_all_protocols() {
        assert!(render_halfblocks(b"not-an-image", 20, 10).is_err());
        assert!(render_for_stdout(b"not-an-image", 20, 10).is_err());
        for proto in [
            TerminalImageProtocol::Halfblock,
            TerminalImageProtocol::ITerm2,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::Sixel,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                ..Default::default()
            };
            assert!(render_ansi_stdout_with_caps(b"bad-bytes", 16, 8, caps).is_err());
            assert!(
                decode_and_render_for_protocol(b"bad-bytes", FitBox::fit(16, 8), (0, 0, 0), caps)
                    .is_err()
            );
        }
    }

    #[test]
    fn test_protocol_helpers_and_cell_size_parser() {
        assert_eq!(TerminalImageProtocol::Kitty.as_str(), "kitty");
        assert_eq!(TerminalImageProtocol::ITerm2.as_str(), "iterm2");
        assert_eq!(TerminalImageProtocol::Sixel.as_str(), "sixel");
        assert_eq!(TerminalImageProtocol::Halfblock.as_str(), "halfblock");

        assert_eq!(TerminalImageProtocol::Kitty.label(), "Kitty");
        assert_eq!(TerminalImageProtocol::ITerm2.label(), "iTerm2");
        assert_eq!(TerminalImageProtocol::Sixel.label(), "Sixel");
        assert_eq!(TerminalImageProtocol::Halfblock.label(), "Halfblock");

        assert_eq!(
            TerminalImageProtocol::Kitty.next_manual(),
            TerminalImageProtocol::ITerm2
        );
        assert_eq!(
            TerminalImageProtocol::ITerm2.next_manual(),
            TerminalImageProtocol::Sixel
        );
        assert_eq!(
            TerminalImageProtocol::Sixel.next_manual(),
            TerminalImageProtocol::Halfblock
        );
        assert_eq!(
            TerminalImageProtocol::Halfblock.next_manual(),
            TerminalImageProtocol::Kitty
        );

        assert!(TerminalImageProtocol::Kitty.is_pixel());
        assert!(TerminalImageProtocol::ITerm2.is_pixel());
        assert!(TerminalImageProtocol::Sixel.is_pixel());
        assert!(!TerminalImageProtocol::Halfblock.is_pixel());

        assert_eq!(parse_cell_pixel_size("10x20"), Some((10, 20)));
        assert_eq!(parse_cell_pixel_size(" 9X18 "), Some((9, 18)));
        assert_eq!(parse_cell_pixel_size("0x16"), None);
        assert_eq!(parse_cell_pixel_size("bad"), None);

        let mut env = HashMap::new();
        env.insert("ATLASSIAN_CELL_PIXEL_SIZE", "12x24");
        assert_eq!(
            resolve_cell_pixel_size_with(|k| env.get(k).map(|v| (*v).to_string())),
            Some((12, 24))
        );
    }

    #[test]
    fn test_terminal_probe_reply_parsers() {
        // CSI 16 t reply: \x1b[6;<height>;<width>t -> (width, height)
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[6;20;10t"), Some((10, 20)));
        assert_eq!(
            parse_csi_cell_size_reply(b"noise\x1b[6;24;12t\x1b[?62;4c"),
            Some((12, 24))
        );
        // Clamping extreme values & rejecting zeros/malformed
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[6;200;100t"), Some((64, 128)));
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[6;0;10t"), None);
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[6;20;0t"), None);
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[4;800;1200t"), None);

        // CSI 14 t reply: \x1b[4;<height>;<width>t -> (win_w, win_h)
        assert_eq!(
            parse_csi_win_size_reply(b"\x1b[4;1080;1920t"),
            Some((1920, 1080))
        );
        assert_eq!(
            parse_csi_win_size_reply(b"\x1b[6;20;10t\x1b[4;960;1600t\x1b[?64;4c"),
            Some((1600, 960))
        );
        assert_eq!(parse_csi_win_size_reply(b"\x1b[4;0;1920t"), None);
        assert_eq!(parse_csi_win_size_reply(b"no-escape"), None);

        // DA1 Sixel attribute (;4 after conformance class)
        assert!(parse_da1_has_sixel(b"\x1b[?62;1;2;4;6;9;15;22c"));
        assert!(parse_da1_has_sixel(b"\x1b[6;20;10t\x1b[?64;4;21c"));
        assert!(!parse_da1_has_sixel(b"\x1b[?62;1;2;6;9;15;22c"));
        assert!(!parse_da1_has_sixel(b"\x1b[?4;1;2c"));
        assert!(!parse_da1_has_sixel(b"\x1b[?62;14;40c"));

        // Kitty query response
        assert!(parse_kitty_query_ok(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(parse_kitty_query_ok(b"\x1b[6;20;10t\x1b_Gi=31;OK\x1b\\\x1b[?62;4c"));
        assert!(parse_kitty_query_ok(b"\x1b_Gi=31;OK\x07"));
        assert!(!parse_kitty_query_ok(b"\x1b_Gi=31;ENOTSUP:bad\x1b\\"));
        assert!(!parse_kitty_query_ok(b"\x1b[?62;4c"));
    }

    #[test]
    fn test_kitty_preserves_full_resolution_for_non_png_within_limits() {
        // Encode a 640x400 JPEG and render into a small 20x10 cell box with Kitty.
        // Kitty must preserve full 640x400 crisp resolution instead of downscaling to ~160x160.
        let rgb: image::RgbImage = image::ImageBuffer::from_fn(640, 400, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut jpeg_buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut jpeg_buf, ImageFormat::Jpeg)
            .unwrap();
        let jpeg_bytes = jpeg_buf.into_inner();

        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let rendered =
            decode_and_render_for_protocol(&jpeg_bytes, FitBox::fit(20, 10), (0, 0, 0), caps)
                .unwrap();
        let payload = rendered.pixel_payload.as_deref().unwrap();

        // Reassemble base64 chunks from the Kitty APC payload and decode embedded PNG dimensions.
        let mut b64 = String::new();
        for chunk in payload.split("\x1b\\") {
            if let Some((_, data)) = chunk.split_once(';') {
                b64.push_str(data);
            }
        }
        let png_bytes = B64_STANDARD.decode(b64).unwrap();
        let decoded = image::load_from_memory(&png_bytes).unwrap();
        assert_eq!(decoded.width(), 640);
        assert_eq!(decoded.height(), 400);
    }

    #[test]
    fn test_sixel_floyd_steinberg_dithering_handles_gradients_and_edge_dims() {
        // Smooth 2D gradient with >256 distinct RGB colors exercises median-cut + Floyd-Steinberg dithering.
        let w = 48usize;
        let h = 24usize;
        let mut flat = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let r = ((x * 255) / (w - 1)) as u8;
                let g = ((y * 255) / (h - 1)) as u8;
                let b = (((x + y) * 255) / (w + h - 2)) as u8;
                flat.extend_from_slice(&[r, g, b, 255]);
            }
        }
        let (palette, indexed) = quantize_rgba_to_256(&flat, w, h);
        assert!(!palette.is_empty() && palette.len() <= 256);
        assert_eq!(indexed.len(), w * h);
        assert!(indexed.iter().all(|&i| (i as usize) < palette.len()));

        // Verify average reconstructed luminance closely matches source luminance (no clipping drift).
        let src_avg_r: f64 = flat.chunks_exact(4).map(|p| p[0] as f64).sum::<f64>() / (w * h) as f64;
        let dst_avg_r: f64 = indexed
            .iter()
            .map(|&i| palette[i as usize].0 as f64)
            .sum::<f64>()
            / (w * h) as f64;
        assert!(
            (src_avg_r - dst_avg_r).abs() < 6.0,
            "dithered red mean drifted: src={src_avg_r:.2}, dst={dst_avg_r:.2}"
        );

        // Edge dimensions: 1x1, 1x300, 300x1 (exercises boundary clamping in error diffusion).
        for (ew, eh) in [(1usize, 1usize), (1, 300), (300, 1)] {
            let mut edge = Vec::with_capacity(ew * eh * 4);
            for i in 0..(ew * eh) {
                edge.extend_from_slice(&[
                    (i % 256) as u8,
                    ((i * 3) % 256) as u8,
                    ((i * 7) % 256) as u8,
                    255,
                ]);
            }
            let (ep, ei) = quantize_rgba_to_256(&edge, ew, eh);
            assert!(!ep.is_empty() && ep.len() <= 256);
            assert_eq!(ei.len(), ew * eh);
        }
    }

    #[test]
    fn test_windows_console_probe_restore_flags_always_set_extended_and_clear_quick_edit() {
        // QuickEdit set without ENABLE_EXTENDED_FLAGS (common GetConsoleMode quirk)
        let restored = restore_windows_console_mode_flags(0x0040 | 0x0018);
        assert_ne!(restored & WIN_ENABLE_EXTENDED_FLAGS, 0);
        assert_eq!(restored & WIN_ENABLE_QUICK_EDIT_MODE, 0);
        assert_eq!(restored & 0x0018, 0x0018);

        // Zero previous mode still enables extended flags and keeps QuickEdit cleared
        let from_zero = restore_windows_console_mode_flags(0);
        assert_eq!(from_zero, WIN_ENABLE_EXTENDED_FLAGS);
    }

    #[test]
    fn test_html_and_svg_sniff_and_mime_rejection() {
        // MIME & filename classification must reject HTML/XHTML while natively accepting SVG
        assert!(!is_image_mime_or_name("text/html", Some("photo.png")));
        assert!(!is_image_mime_or_name("text/html; charset=utf-8", None));
        assert!(!is_image_mime_or_name("application/xhtml+xml", Some("x.png")));
        assert!(is_image_mime_or_name("image/svg+xml", Some("icon.svg")));
        assert!(is_image_mime_or_name("image/svg+xml; charset=utf-8", Some("logo.png")));
        assert!(!is_image_mime_or_name("image/png", Some("login.html")));
        assert!(!is_image_mime_or_name("image/png", Some("page.HTM")));
        assert!(is_image_mime_or_name("application/octet-stream", Some("vector.SVG")));
        assert!(!is_image_attachment("text/html", "attachment.png"));
        assert!(is_image_attachment("image/svg+xml", "diagram.svg"));

        // Byte sniffing: HTML-spoofed bytes remain strictly rejected across all protocols
        let html_samples: &[&[u8]] = &[
            b"<!DOCTYPE html><html><body>Login</body></html>",
            b"  \r\n<html><head><title>SSO</title></head></html>",
            b"\xEF\xBB\xBF<!doctype html>\n<html><body><svg></svg></body></html>",
            b"<!-- comment --><html lang=\"en\"></html>",
        ];
        for sample in html_samples {
            assert!(looks_like_html_bytes(sample), "expected HTML sniff for {:?}", sample);
            assert!(!looks_like_svg_bytes(sample), "HTML should not sniff as SVG");

            let err_hb = decode_and_render_halfblocks(sample, 40, 20, (0, 0, 0))
                .unwrap_err()
                .to_string();
            assert!(
                err_hb.contains("Received HTML document instead of raster image"),
                "unexpected halfblock HTML error: {err_hb}"
            );
            for proto in [
                TerminalImageProtocol::Sixel,
                TerminalImageProtocol::Kitty,
                TerminalImageProtocol::ITerm2,
                TerminalImageProtocol::Halfblock,
            ] {
                let caps = TerminalGraphicsCaps {
                    protocol: proto,
                    ..Default::default()
                };
                let err = decode_and_render_for_protocol(
                    sample,
                    FitBox::fit(40, 20),
                    (0, 0, 0),
                    caps,
                )
                .unwrap_err()
                .to_string();
                assert!(
                    err.contains("Received HTML document instead of raster image"),
                    "unexpected {proto:?} HTML error: {err}"
                );
            }
        }

        // Standalone SVG vector documents rasterize natively via bitefoundry-html/resvg
        let svg_samples: &[&[u8]] = &[
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><rect width="80" height="40" fill="#e11d48"/></svg>"##,
            br##"<?xml version="1.0"?><svg viewBox="0 0 60 30"><circle cx="30" cy="15" r="12" fill="#2563eb"/></svg>"##,
            br##"<!-- icon --><svg width="64" height="32"><rect width="64" height="32" fill="#16a34a"/></svg>"##,
        ];
        for sample in svg_samples {
            assert!(looks_like_svg_bytes(sample), "expected SVG sniff for {:?}", sample);
            assert!(!looks_like_html_bytes(sample), "SVG should not sniff as HTML");

            let hb = decode_and_render_halfblocks(sample, 40, 20, (0, 0, 0))
                .expect("standalone SVG should render as halfblocks");
            assert_eq!(hb.format_label, "SVG");
            assert_eq!(hb.protocol, TerminalImageProtocol::Halfblock);
            assert!(!hb.lines.is_empty());
            assert!(hb.cols > 0 && hb.rows > 0);
            assert!(
                hb.lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .any(|s| s.content.contains('▀')),
                "SVG halfblock output must contain halfblock glyphs"
            );

            for proto in [
                TerminalImageProtocol::Sixel,
                TerminalImageProtocol::Kitty,
                TerminalImageProtocol::ITerm2,
            ] {
                let caps = TerminalGraphicsCaps {
                    protocol: proto,
                    ..Default::default()
                };
                let px = decode_and_render_for_protocol(
                    sample,
                    FitBox::fit(40, 20),
                    (0, 0, 0),
                    caps,
                )
                .unwrap_or_else(|e| panic!("SVG should render under {proto:?}: {e}"));
                assert_eq!(px.format_label, "SVG");
                assert_eq!(px.protocol, proto);
                assert!(
                    px.pixel_payload.as_deref().is_some_and(|p| !p.is_empty()),
                    "SVG {proto:?} payload should be non-empty"
                );
            }
        }

        // Valid PNG must not be misclassified as HTML or SVG
        let png = make_test_png(8, 8);
        assert!(!looks_like_html_bytes(&png));
        assert!(!looks_like_svg_bytes(&png));
    }

    #[test]
    fn test_erase_pixel_rect_sequence_and_sixel_safe_height() {
        assert_eq!(kitty_delete_all_sequence(), "\x1b_Ga=d,d=A,q=2\x1b\\");

        // Floors rows * cell_h to multiple of 6
        assert_eq!(compute_sixel_safe_pixel_height(10, 20), 198);
        assert_eq!(compute_sixel_safe_pixel_height(5, 16), 78);
        assert_eq!(compute_sixel_safe_pixel_height(3, 18), 54);
        assert_eq!(compute_sixel_safe_pixel_height(0, 16), 0);

        // Empty / out-of-bounds rects yield empty erase sequence
        assert!(erase_pixel_rect_sequence(0, 0, 0, 5, 80, 24).is_empty());
        assert!(erase_pixel_rect_sequence(0, 0, 5, 0, 80, 24).is_empty());
        assert!(erase_pixel_rect_sequence(80, 0, 5, 5, 80, 24).is_empty());

        // Normal rect expands by +2 cols / +2 rows and clamps to terminal dimensions
        let seq = erase_pixel_rect_sequence(10, 4, 20, 6, 80, 24);
        assert!(seq.starts_with("\x1b[0m"));
        // 1-indexed row 5..=12 (6 rows + 2 overhang rows = 8 rows), col 11, 22 spaces
        assert!(seq.contains("\x1b[5;11H"));
        assert!(seq.contains("\x1b[12;11H"));
        assert!(!seq.contains("\x1b[13;11H"));
        assert!(seq.contains(&" ".repeat(22)));

        // Near bottom-right corner clamps tightly to term bounds
        let clamped = erase_pixel_rect_sequence(75, 21, 5, 3, 80, 24);
        assert!(clamped.contains("\x1b[22;76H"));
        assert!(clamped.contains("\x1b[24;76H"));
        assert!(!clamped.contains("\x1b[25;76H"));
    }

    #[test]
    fn test_tall_image_viewport_pixel_payload_scrolls_for_sixel_and_kitty() {
        let tall: RgbaImage = ImageBuffer::from_fn(320, 960, |x, y| {
            let band = (y / 160) as u8;
            Rgba([
                band.wrapping_mul(45),
                ((x * 255) / 320) as u8,
                255u8.saturating_sub(band.wrapping_mul(40)),
                255,
            ])
        });
        let png = encode_png(&DynamicImage::ImageRgba8(tall)).unwrap();

        for proto in [
            TerminalImageProtocol::Sixel,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::ITerm2,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let rendered = decode_and_render_for_protocol(
                &png,
                FitBox::scrollable(80, 24, 100),
                (20, 20, 20),
                caps,
            )
            .unwrap();
            assert!(
                rendered.rows > 20,
                "tall {proto:?} image should exceed modal viewport height, got {} rows",
                rendered.rows
            );

            let (top_payload, top_w, top_h) = rendered
                .viewport_pixel_payload(0, 0, 60, 15)
                .expect("top viewport slice");
            let (scrolled_payload, sc_w, sc_h) = rendered
                .viewport_pixel_payload(0, 10, 60, 15)
                .expect("scrolled viewport slice");
            assert!(top_w <= 60 && top_h <= 15);
            assert!(sc_w <= 60 && sc_h <= 15);
            assert_ne!(
                top_payload, scrolled_payload,
                "scrolling tall {proto:?} image must produce distinct viewport slice"
            );
        }
    }

    fn decode_kitty_embedded_png(payload: &str) -> DynamicImage {
        let mut b64 = String::new();
        for chunk in payload.split("\x1b\\") {
            if let Some((_, data)) = chunk.split_once(';') {
                b64.push_str(data);
            }
        }
        let png_bytes = B64_STANDARD.decode(b64).expect("valid base64");
        image::load_from_memory(&png_bytes).expect("valid embedded PNG")
    }

    #[test]
    fn test_probe_vt_mode_enabled_before_query_write_and_restored() {
        use std::cell::RefCell;
        use std::rc::Rc;

        #[derive(Debug, PartialEq, Eq)]
        enum Event {
            SetMode(u32),
            Write(String),
            Flush,
            Read,
        }

        struct RecordingWriter(Rc<RefCell<Vec<Event>>>);
        impl IoWrite for RecordingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .borrow_mut()
                    .push(Event::Write(String::from_utf8_lossy(buf).into_owned()));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.borrow_mut().push(Event::Flush);
                Ok(())
            }
        }

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut writer = RecordingWriter(log.clone());
        let prev_mode = 0x0018 | WIN_ENABLE_QUICK_EDIT_MODE;
        let query = "\x1b[16t\x1b[14t\x1b[c";

        let reply = run_windows_probe_with_hooks(
            prev_mode,
            query,
            &mut writer,
            |mode| log.borrow_mut().push(Event::SetMode(mode)),
            || {
                log.borrow_mut().push(Event::Read);
                b"\x1b[6;20;10t\x1b[?61;4c".to_vec()
            },
        );

        assert_eq!(reply, "\x1b[6;20;10t\x1b[?61;4c");
        let events = log.borrow();
        assert_eq!(events.len(), 5, "unexpected probe event sequence: {events:?}");

        // 1. VT input mode MUST be enabled BEFORE writing the probe query.
        match events[0] {
            Event::SetMode(m) => {
                assert_ne!(m & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
                assert_ne!(m & WIN_ENABLE_EXTENDED_FLAGS, 0);
                assert_eq!(m & WIN_ENABLE_QUICK_EDIT_MODE, 0);
            }
            ref other => panic!("expected initial SetMode before write, got {other:?}"),
        }
        assert_eq!(events[1], Event::Write(query.to_string()));
        assert_eq!(events[2], Event::Flush);
        assert_eq!(events[3], Event::Read);

        // 2. ModeRestore drop MUST restore mode with QuickEdit cleared & ExtendedFlags set.
        match events[4] {
            Event::SetMode(restored) => {
                assert_eq!(restored & WIN_ENABLE_QUICK_EDIT_MODE, 0);
                assert_ne!(restored & WIN_ENABLE_EXTENDED_FLAGS, 0);
                assert_eq!(restored & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
                assert_eq!(restored & 0x0018, 0x0018);
            }
            ref other => panic!("expected final SetMode restore on drop, got {other:?}"),
        }
    }

    #[test]
    fn test_cell_px_rounded_division_avoids_truncation() {
        assert_eq!(compute_rounded_cell_px(1590, 995, 100, 50), Some((16, 20)));

        let mut caps = TerminalGraphicsCaps::default();
        apply_probe_reply_to_caps(
            &mut caps,
            "\x1b[4;995;1590t\x1b[?61;4c",
            true,
            false,
            Some((100, 50)),
        );
        assert_eq!(caps.cell_px, (16, 20));
        assert!(caps.cell_px_exact);
    }

    #[test]
    fn test_preview_cache_key_includes_cell_px() {
        let k_a = preview_cache_key("att-1", 100, 30, 100, (10, 20), TerminalImageProtocol::Sixel);
        let k_b = preview_cache_key("att-1", 100, 30, 100, (16, 32), TerminalImageProtocol::Sixel);
        let k_zoom =
            preview_cache_key("att-1", 100, 30, 150, (10, 20), TerminalImageProtocol::Sixel);
        assert_ne!(k_a, k_b);
        assert!(k_a.contains("10x20"));
        assert!(k_b.contains("16x32"));
        assert_ne!(k_a, k_zoom);
        assert!(k_zoom.contains("150%"));
    }

    #[test]
    fn test_protocol_detection_wt_requires_da1_stackbite_suppressed_wezterm_kitty() {
        // 1. WT_SESSION alone without DA1 -> Halfblock (never force Sixel)
        let mut wt_env = HashMap::new();
        wt_env.insert("WT_SESSION", "11111111-2222-3333-4444-555555555555");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_env),
            TerminalImageProtocol::Halfblock
        );

        // 2. WT_SESSION + DA1 `;4` -> Sixel; DA1 without `;4` -> Halfblock
        let mut caps_sixel = TerminalGraphicsCaps {
            protocol: resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_env),
            ..Default::default()
        };
        apply_probe_reply_to_caps(&mut caps_sixel, "\x1b[?61;4c", false, true, None);
        assert_eq!(caps_sixel.protocol, TerminalImageProtocol::Sixel);

        let mut caps_no_sixel = TerminalGraphicsCaps {
            protocol: resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_env),
            ..Default::default()
        };
        apply_probe_reply_to_caps(&mut caps_no_sixel, "\x1b[?61;1c", false, true, None);
        assert_eq!(caps_no_sixel.protocol, TerminalImageProtocol::Halfblock);

        // 3. WT_SESSION + STACKBITE_* -> static path returns Halfblock; live probe decides
        for sb_key in ["STACKBITE_ROOM_ID", "STACKBITE_HOME", "STACKBITE_PTY_BACKEND"] {
            let mut sb_env = HashMap::new();
            sb_env.insert("WT_SESSION", "11111111-2222-3333-4444-555555555555");
            sb_env.insert("WEZTERM_EXECUTABLE", "wezterm-gui.exe");
            sb_env.insert(sb_key, "active");
            assert_eq!(
                resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &sb_env),
                TerminalImageProtocol::Halfblock,
                "expected {sb_key} to suppress static protocol env inference"
            );
        }

        // 4. WezTerm -> Kitty (promoted ahead of iTerm2)
        let mut wez_exec = HashMap::new();
        wez_exec.insert("WEZTERM_EXECUTABLE", "/usr/bin/wezterm-gui");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_exec),
            TerminalImageProtocol::Kitty
        );

        let mut wez_tp = HashMap::new();
        wez_tp.insert("TERM_PROGRAM", "WezTerm");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_tp),
            TerminalImageProtocol::Kitty
        );
    }

    #[test]
    fn test_kitty_single_axis_emits_only_constraining_dimension() {
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };

        // Width-constrained: 1600x900 in 80x40 cells -> emits c= only, no ,r=
        let wide_png = make_test_png(1600, 900);
        let wide_rendered =
            decode_and_render_for_protocol(&wide_png, FitBox::fit(80, 40), (0, 0, 0), caps)
                .unwrap();
        let wide_payload = wide_rendered.pixel_payload.as_deref().unwrap();
        assert!(wide_payload.contains(",c="), "expected c= in wide Kitty payload");
        assert!(
            !wide_payload.contains(",r="),
            "width-constrained Kitty payload must omit r=: {wide_payload}"
        );

        // Height-constrained: 400x1200 in 80x20 cells -> emits r= only, no ,c=
        let tall_png = make_test_png(400, 1200);
        let tall_rendered =
            decode_and_render_for_protocol(&tall_png, FitBox::fit(80, 20), (0, 0, 0), caps)
                .unwrap();
        let tall_payload = tall_rendered.pixel_payload.as_deref().unwrap();
        assert!(tall_payload.contains(",r="), "expected r= in tall Kitty payload");
        assert!(
            !tall_payload.contains(",c="),
            "height-constrained Kitty payload must omit c=: {tall_payload}"
        );
    }

    #[test]
    fn test_pixel_scroll_backing_crops_native_source_without_double_scaling() {
        let src: RgbaImage = ImageBuffer::from_fn(800, 2400, |x, y| {
            Rgba([(x % 256) as u8, ((y / 10) % 256) as u8, 90, 255])
        });
        let png = encode_png(&DynamicImage::ImageRgba8(src)).unwrap();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };

        // Virtual width is 40*10 = 400px (0.5x of 800px native width).
        let rendered = decode_and_render_for_protocol(
            &png,
            FitBox::scrollable(40, 20, 100),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        let (slice_payload, draw_cols, draw_rows) = rendered
            .viewport_pixel_payload(0, 10, 40, 20)
            .expect("native source scroll slice");
        assert_eq!(draw_cols, 40);
        assert_eq!(draw_rows, 20);

        let decoded = decode_kitty_embedded_png(&slice_payload);
        assert_eq!(
            decoded.width(),
            800,
            "scroll slice must keep native 800px width instead of 400px downscaled buffer"
        );
        assert_eq!(decoded.height(), 800);
    }

    #[test]
    fn test_fit_box_no_forced_crop_on_wide_low_terminal_at_100pct() {
        let fit = FitBox::scrollable(176, 25, 100);
        for cell_px in [(10, 20), (8, 16)] {
            for proto in [
                TerminalImageProtocol::Kitty,
                TerminalImageProtocol::ITerm2,
                TerminalImageProtocol::Sixel,
            ] {
                let (_, _, occ_cols, occ_rows) =
                    compute_pixel_fit_for_protocol(1920, 1080, fit, cell_px, proto);
                assert!(
                    occ_rows <= 25 && occ_cols <= 176,
                    "1920x1080 at 100% must fit inside 176x25 without forced crop ({proto:?}, {cell_px:?}): got {occ_cols}x{occ_rows}"
                );
            }
        }
    }

    #[test]
    fn test_tall_screenshot_uses_full_width_past_92_cols_and_zoom_300pct_scales() {
        let cell_px = (10, 20);
        let (w100, h100, c100, r100) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 100),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        let (w200, h200, c200, r200) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 200),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        let (w300, h300, c300, r300) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 300),
            cell_px,
            TerminalImageProtocol::Kitty,
        );

        assert!(c100 > 92, "tall screenshot must not be capped at 92 cols, got {c100}");
        assert!(w100 < w200 && w200 < w300, "width must scale monotonically: {w100} < {w200} < {w300}");
        assert!(h100 < h200 && h200 < h300, "height must scale monotonically: {h100} < {h200} < {h300}");
        assert!(c100 < c200 && c200 < c300);
        assert!(r100 < r200 && r200 < r300);
        assert!(
            w200 > 1920 && w300 > 1920,
            "zoomed widths must exceed legacy 1920px cap: w200={w200}, w300={w300}"
        );
    }

    #[test]
    fn test_sixel_preserves_ui_accent_suppresses_flat_dither_and_bounds_cache() {
        // Synthetic 200x100 UI screenshot: ~98% solid gray (128,128,128) background,
        // vivid blue (0,100,255) & red (240,30,30) accent text, plus >256 gradient strip to trigger median-cut.
        let w = 200usize;
        let h = 100usize;
        let mut flat = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if y < 2 && x < 180 {
                    // 360 distinct colors so total distinct > 256
                    (((x * 255) / 180) as u8, ((y * 120 + x) % 256) as u8, (((x * 3) % 256)) as u8)
                } else if y == 10 && (10..50).contains(&x) {
                    (0, 100, 255) // vivid UI accent blue
                } else if y == 12 && (10..50).contains(&x) {
                    (240, 30, 30) // vivid UI accent red
                } else {
                    (128, 128, 128) // 98% solid gray panel
                };
                let o = (y * w + x) * 4;
                flat[o] = r;
                flat[o + 1] = g;
                flat[o + 2] = b;
                flat[o + 3] = 255;
            }
        }

        let (palette, indexed, _) = quantize_rgba_to_256_with_stats(&flat, w, h);
        let min_blue_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(0, 100, 255, r, g, b))
            .min()
            .unwrap();
        let min_red_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(240, 30, 30, r, g, b))
            .min()
            .unwrap();
        assert!(
            min_blue_dist < 600,
            "UI blue accent lost from palette (min redmean dist^2 = {min_blue_dist})"
        );
        assert!(
            min_red_dist < 600,
            "UI red accent lost from palette (min redmean dist^2 = {min_red_dist})"
        );

        // Flat background region (y in 25..95, x in 5..195) must map to a single palette index (0 dither noise).
        let bg_idx = indexed[25 * w + 5];
        for y in 25..95 {
            for x in 5..195 {
                assert_eq!(
                    indexed[y * w + x],
                    bg_idx,
                    "dither noise detected in flat gray region at ({x}, {y})"
                );
            }
        }

        // Large rich-color gradient must keep lookup_cache strictly bounded to MAX_QUANT_LOOKUP_CACHE.
        let gw = 200usize;
        let gh = 200usize;
        let mut huge_grad = Vec::with_capacity(gw * gh * 4);
        for y in 0..gh {
            for x in 0..gw {
                huge_grad.extend_from_slice(&[
                    ((x * 255) / (gw - 1)) as u8,
                    ((y * 255) / (gh - 1)) as u8,
                    (((x * 7 + y * 13) % 256)) as u8,
                    255,
                ]);
            }
        }
        let (_, _, cache_len) = quantize_rgba_to_256_with_stats(&huge_grad, gw, gh);
        assert!(
            cache_len <= MAX_QUANT_LOOKUP_CACHE,
            "lookup_cache exceeded cap: {cache_len} > {MAX_QUANT_LOOKUP_CACHE}"
        );
    }

    #[test]
    fn test_halfblock_aspect_ratio_matches_actual_cell_px() {
        let sq: RgbaImage = ImageBuffer::from_pixel(200, 200, Rgba([60, 140, 220, 255]));
        let png = encode_png(&DynamicImage::ImageRgba8(sq)).unwrap();
        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::fit(80, 40),
            DEFAULT_PREVIEW_BG_RGB,
            (9, 22),
        )
        .unwrap();
        let phys_ratio = (rendered.cols as f64 * 9.0) / (rendered.rows as f64 * 22.0);
        assert!(
            (0.90..=1.10).contains(&phys_ratio),
            "halfblock physical aspect ratio distorted for 9x22 cells: cols={}, rows={}, ratio={phys_ratio:.3}",
            rendered.cols,
            rendered.rows
        );
    }

    #[test]
    fn test_inline_halfblock_uses_light_theme_bg_for_transparent_png() {
        let transparent: RgbaImage = ImageBuffer::from_pixel(16, 8, Rgba([0, 0, 0, 0]));
        let png = encode_png(&DynamicImage::ImageRgba8(transparent)).unwrap();
        let light_bg = color_to_rgb(Color::Rgb(245, 247, 250), DEFAULT_PREVIEW_BG_RGB);
        assert_eq!(light_bg, (245, 247, 250));

        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::fit(40, 20),
            light_bg,
            (10, 20),
        )
        .unwrap();
        assert!(!rendered.lines.is_empty());
        for line in &rendered.lines {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::Rgb(245, 247, 250)));
                assert_eq!(span.style.bg, Some(Color::Rgb(245, 247, 250)));
            }
        }
    }
}
