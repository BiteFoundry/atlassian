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

const MAX_PIXEL_WIDTH: u32 = 1920;
const MAX_PIXEL_HEIGHT: u32 = 1200;
const MAX_SCROLLABLE_PIXEL_HEIGHT: u32 = 3600;
const MAX_ITERM2_RAW_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT_MS: u64 = 80;

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

impl TerminalGraphicsCaps {
    /// Re-probes terminal cell pixel dimensions while preserving the currently selected protocol
    /// and any previously probed exact cell dimensions when `window_size()` returns 0x0.
    pub fn refresh_cell_pixel_size(mut self) -> Self {
        let (cell_px, cell_px_exact) = detect_cell_pixel_size();
        if cell_px_exact || !self.cell_px_exact {
            self.cell_px = cell_px;
            self.cell_px_exact = cell_px_exact;
        }
        self
    }
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
    Rgba {
        width: u32,
        height: u32,
        cell_px: (u16, u16),
        rgba: Arc<Vec<u8>>,
    },
}

impl PixelScrollBacking {
    fn approx_heap_bytes(&self) -> usize {
        match self {
            Self::SixelIndexed {
                palette, indexed, ..
            } => palette.capacity() * std::mem::size_of::<(u8, u8, u8)>() + indexed.capacity(),
            Self::Rgba { rgba, .. } => rgba.capacity(),
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
            PixelScrollBacking::Rgba {
                width,
                height,
                cell_px,
                rgba,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *width || y0 >= *height {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let max_h = (view_rows as u32).saturating_mul(cell_h);
                let crop_w = (*width - x0).min(max_w);
                let crop_h = (*height - y0).min(max_h);
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = crop_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;

                let full_stride = (*width as usize) * 4;
                let row_bytes = (crop_w as usize) * 4;
                let x_byte_off = (x0 as usize) * 4;
                let mut sub = Vec::with_capacity(row_bytes * crop_h as usize);
                for dy in 0..(crop_h as usize) {
                    let start = (y0 as usize + dy) * full_stride + x_byte_off;
                    sub.extend_from_slice(&rgba[start..start + row_bytes]);
                }
                let sub_img = image::RgbaImage::from_raw(crop_w, crop_h, sub)?;
                let png_bytes = encode_png(&DynamicImage::ImageRgba8(sub_img)).ok()?;
                let payload = match self.protocol {
                    TerminalImageProtocol::Kitty => {
                        encode_kitty_tui_payload(&png_bytes, draw_cols, draw_rows)
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

/// Builds a protocol-scoped preview cache key (`"{att_id}:{cols}x{rows}:{protocol}"`).
pub fn preview_cache_key(
    att_id: &str,
    cols: u16,
    rows: u16,
    protocol: TerminalImageProtocol,
) -> String {
    format!("{att_id}:{cols}x{rows}:{}", protocol.as_str())
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

fn resolve_protocol_with<F>(pref: ImageProtocolPreference, get_env: F) -> TerminalImageProtocol
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = non_empty_env(&get_env, "ATLASSIAN_IMAGE_PROTOCOL") {
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

    let term_program = lower_env(&get_env, "TERM_PROGRAM");
    let term = lower_env(&get_env, "TERM");

    // Kitty Graphics Protocol (checked first so Kitty/Ghostty launched from another shell always wins).
    if non_empty_env(&get_env, "KITTY_WINDOW_ID").is_some()
        || non_empty_env(&get_env, "GHOSTTY_RESOURCES_DIR").is_some()
        || matches!(term_program.as_str(), "kitty" | "ghostty")
        || term.contains("xterm-kitty")
        || term.contains("ghostty")
    {
        return TerminalImageProtocol::Kitty;
    }

    // iTerm2 Inline Images (`OSC 1337`), checked before `WT_SESSION` so WezTerm/Rio/Mintty win under Windows Terminal.
    if non_empty_env(&get_env, "WEZTERM_EXECUTABLE").is_some()
        || non_empty_env(&get_env, "WEZTERM_PANE").is_some()
        || matches!(
            term_program.as_str(),
            "wezterm" | "iterm.app" | "rio" | "mintty" | "hyper"
        )
    {
        return TerminalImageProtocol::ITerm2;
    }

    // Sixel (`DCS q`)
    if matches!(
        term.as_str(),
        "foot" | "foot-extra" | "mlterm" | "yaft-256color"
    ) || matches!(term_program.as_str(), "foot" | "contour")
    {
        return TerminalImageProtocol::Sixel;
    }

    if non_empty_env(&get_env, "WT_SESSION").is_some()
        && !matches!(term_program.as_str(), "vscode" | "cursor" | "windsurf")
        && non_empty_env(&get_env, "VSCODE_PID").is_none()
        && non_empty_env(&get_env, "VSCODE_INJECTION").is_none()
    {
        return TerminalImageProtocol::Sixel;
    }

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

fn detect_cell_pixel_size() -> ((u16, u16), bool) {
    if let Some(parsed) = resolve_cell_pixel_size_with(|k| std::env::var(k).ok()) {
        return (parsed, true);
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
            let cw = ((ws.width as u32 + ws.columns as u32 / 2) / ws.columns as u32).clamp(4, 64)
                as u16;
            let ch =
                ((ws.height as u32 + ws.rows as u32 / 2) / ws.rows as u32).clamp(8, 128) as u16;
            return ((cw, ch), true);
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

/// Performs non-blocking, timeout-bounded (~80ms) active terminal graphics and cell-pixel probing.
/// Must be called once on TUI startup after raw mode is enabled and before spawning `EventStream`.
pub fn probe_terminal_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let mut caps = detect_graphics_caps(pref);
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return caps;
    }

    let get_env = |k: &str| std::env::var(k).ok();
    let in_mux =
        non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some();
    let env_override = non_empty_env(&get_env, "ATLASSIAN_IMAGE_PROTOCOL")
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

    let reply = write_and_read_terminal_probe(query.as_bytes(), Duration::from_millis(PROBE_TIMEOUT_MS));
    if reply.is_empty() {
        return caps;
    }

    if need_cell_px {
        if let Some(cell_px) = parse_csi_cell_size_reply(&reply) {
            caps.cell_px = cell_px;
            caps.cell_px_exact = true;
        } else if let Some((win_w, win_h)) = parse_csi_win_size_reply(&reply) {
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                if cols > 0 && rows > 0 {
                    let cw = ((win_w as u32 + cols as u32 / 2) / cols as u32).clamp(4, 64) as u16;
                    let ch = ((win_h as u32 + rows as u32 / 2) / rows as u32).clamp(8, 128) as u16;
                    caps.cell_px = (cw, ch);
                    caps.cell_px_exact = true;
                }
            }
        }
    }

    if need_protocol {
        if parse_kitty_query_ok(&reply) {
            caps.protocol = TerminalImageProtocol::Kitty;
        } else if parse_da1_has_sixel(&reply) {
            caps.protocol = TerminalImageProtocol::Sixel;
        }
    }

    caps
}

#[cfg(any(windows, test))]
pub const WIN_ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
#[cfg(any(windows, test))]
pub const WIN_ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

/// Restores Windows console input mode while always setting `ENABLE_EXTENDED_FLAGS` (`0x0080`)
/// and clearing `ENABLE_QUICK_EDIT_MODE` (`0x0040`) so mouse capture is never clobbered by QuickEdit.
#[cfg(any(windows, test))]
pub fn restore_windows_console_mode_flags(prev_mode: u32) -> u32 {
    (prev_mode | WIN_ENABLE_EXTENDED_FLAGS) & !WIN_ENABLE_QUICK_EDIT_MODE
}

#[cfg(windows)]
fn write_and_read_terminal_probe(query: &[u8], timeout: Duration) -> Vec<u8> {
    let mut stdout = std::io::stdout();
    if stdout.write_all(query).is_err() || stdout.flush().is_err() {
        return Vec::new();
    }
    read_terminal_probe_reply(timeout)
}

#[cfg(windows)]
fn read_terminal_probe_reply(timeout: Duration) -> Vec<u8> {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // -10i32 as u32
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
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

    struct ModeRestore {
        handle: isize,
        prev_mode: u32,
    }
    impl Drop for ModeRestore {
        fn drop(&mut self) {
            unsafe {
                // Always include ENABLE_EXTENDED_FLAGS (0x0080) and clear ENABLE_QUICK_EDIT_MODE (0x0040)
                // so SetConsoleMode never re-enables QuickEdit when GetConsoleMode omitted 0x0080.
                SetConsoleMode(
                    self.handle,
                    restore_windows_console_mode_flags(self.prev_mode),
                );
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
        return Vec::new();
    }

    let mut prev_mode = 0u32;
    if unsafe { GetConsoleMode(handle, &mut prev_mode) } == 0 {
        return Vec::new();
    }

    // Declared after `_owned_handle` so `ModeRestore::drop` executes BEFORE `CloseHandle`.
    let probe_mode = restore_windows_console_mode_flags(prev_mode) | ENABLE_VIRTUAL_TERMINAL_INPUT;
    let _restore = if prev_mode != probe_mode {
        let _ = unsafe { SetConsoleMode(handle, probe_mode) };
        Some(ModeRestore { handle, prev_mode })
    } else {
        None
    };

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
        if unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), to_read, &mut read_count) } == 0
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
        if unsafe { PeekConsoleInputW(handle, records.as_mut_ptr(), to_peek, &mut peeked) } == 0
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
        if unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), key_prefix, &mut read_count) }
            == 0
            || read_count == 0
        {
            break;
        }
        for rec in &records[..read_count as usize] {
            if rec.event_type == KEY_EVENT {
                let ke = unsafe { rec.event.key_event };
                if ke.b_key_down != 0 && ke.u_char != 0 && ke.u_char <= 0x7f && out.len() < 4096 {
                    out.push(ke.u_char as u8);
                }
            }
        }
    }

    out
}

#[cfg(unix)]
fn write_and_read_terminal_probe(query: &[u8], timeout: Duration) -> Vec<u8> {
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
    if stdout.write_all(query).is_err() || stdout.flush().is_err() {
        return Vec::new();
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

    out
}

#[cfg(not(any(windows, unix)))]
fn write_and_read_terminal_probe(_query: &[u8], _timeout: Duration) -> Vec<u8> {
    Vec::new()
}

fn resolve_cell_pixel_size_with<F>(get_env: F) -> Option<(u16, u16)>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = non_empty_env(&get_env, "ATLASSIAN_CELL_PIXEL_SIZE") {
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
    if looks_like_svg_bytes(bytes) {
        bail!("SVG vector image cannot be raster-decoded");
    }
    Ok(())
}

/// Returns `true` if a MIME type or filename represents a supported raster image (`png`, `jpeg`, `jpg`, `gif`, `webp`).
/// Never returns `true` for HTML/XHTML or SVG (`text/html`, `application/xhtml+xml`, `image/svg+xml`, `.html`, `.htm`, `.svg`).
pub fn is_image_attachment(mime_type: &str, filename: &str) -> bool {
    let m_base = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        m_base.as_str(),
        "text/html" | "application/xhtml+xml" | "image/svg+xml"
    ) || m_base.contains("svg")
        || m_base.contains("html")
    {
        return false;
    }

    let f = filename.trim().to_ascii_lowercase();
    if f.ends_with(".html")
        || f.ends_with(".htm")
        || f.ends_with(".xhtml")
        || f.ends_with(".svg")
        || f.ends_with(".svgz")
    {
        return false;
    }

    if m_base.starts_with("image/") {
        return true;
    }
    f.ends_with(".png")
        || f.ends_with(".jpg")
        || f.ends_with(".jpeg")
        || f.ends_with(".gif")
        || f.ends_with(".webp")
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
    reject_non_raster_markup_bytes(bytes)?;
    let guessed = image::guess_format(bytes).ok();
    let format_label = format_label_for(guessed);
    let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
    let rgba = dyn_img.to_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_w, target_h) = compute_target_dims(width_px, height_px, max_cols, max_rows);

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
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
    caps: TerminalGraphicsCaps,
) -> Result<RenderedTerminalImage> {
    reject_non_raster_markup_bytes(bytes)?;
    if caps.protocol == TerminalImageProtocol::Halfblock {
        return decode_and_render_halfblocks(bytes, max_cols, max_rows, bg_rgb);
    }

    let guessed = image::guess_format(bytes).ok();
    let format_label = format_label_for(guessed);
    let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
    let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_px_w, target_px_h, occ_cols, occ_rows) = compute_pixel_fit_for_protocol(
        width_px,
        height_px,
        max_cols,
        max_rows,
        caps.cell_px,
        caps.protocol,
    );
    let max_h_bound = if max_rows >= max_cols {
        MAX_SCROLLABLE_PIXEL_HEIGHT
    } else {
        MAX_PIXEL_HEIGHT
    };

    let (payload, scroll_backing) = match caps.protocol {
        TerminalImageProtocol::Kitty => {
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_PIXEL_WIDTH
                && height_px <= max_h_bound
            {
                bytes
            } else if width_px <= MAX_PIXEL_WIDTH && height_px <= max_h_bound {
                // Preserve full crisp source resolution and let the terminal GPU scale.
                png_owned = encode_png(&dyn_img)?;
                &png_owned
            } else {
                let (fit_w, fit_h) = fit_within_max_bounds(width_px, height_px, MAX_PIXEL_WIDTH, max_h_bound);
                let resized = dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3);
                png_owned = encode_png(&resized)?;
                &png_owned
            };
            let rgba = dyn_img.to_rgba8();
            let resized = if width_px == target_px_w && height_px == target_px_h {
                rgba
            } else {
                image::imageops::resize(&rgba, target_px_w, target_px_h, FilterType::Lanczos3)
            };
            let backing = PixelScrollBacking::Rgba {
                width: target_px_w,
                height: target_px_h,
                cell_px: caps.cell_px,
                rgba: Arc::new(resized.into_raw()),
            };
            (encode_kitty_tui_payload(png_bytes, occ_cols, occ_rows), Some(backing))
        }
        TerminalImageProtocol::ITerm2 => {
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
            {
                bytes
            } else {
                let (fit_w, fit_h) = fit_within_max_bounds(width_px, height_px, MAX_PIXEL_WIDTH, max_h_bound);
                let encoded = if fit_w == width_px && fit_h == height_px {
                    encode_png(&dyn_img)?
                } else {
                    let resized = dyn_img.resize_exact(fit_w, fit_h, FilterType::Lanczos3);
                    encode_png(&resized)?
                };
                encoded_owned = encoded;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            let payload = format!(
                "\x1b]1337;File=inline=1;width={occ_cols};height={occ_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
            );
            let rgba = dyn_img.to_rgba8();
            let resized = if width_px == target_px_w && height_px == target_px_h {
                rgba
            } else {
                image::imageops::resize(&rgba, target_px_w, target_px_h, FilterType::Lanczos3)
            };
            let backing = PixelScrollBacking::Rgba {
                width: target_px_w,
                height: target_px_h,
                cell_px: caps.cell_px,
                rgba: Arc::new(resized.into_raw()),
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

fn encode_kitty_tui_payload(png_bytes: &[u8], occ_cols: u16, occ_rows: u16) -> String {
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
            let _ = write!(
                out,
                "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={occ_cols},r={occ_rows},C=1,m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};{chunk}\x1b\\");
        }
    }
    out
}

fn compute_target_dims(width_px: u32, height_px: u32, max_cols: u16, max_rows: u16) -> (u32, u32) {
    let clamped_cols = max_cols.max(4);
    let clamped_rows = max_rows.max(2);
    let is_scrollable = clamped_rows >= clamped_cols;
    let is_tall = (height_px as u64) * 4 > (width_px as u64) * 5;
    let (eff_cols, eff_rows) = if is_scrollable {
        if is_tall {
            let fit_cols = if clamped_rows > 140 {
                clamped_cols
            } else {
                clamped_cols.min(92)
            };
            (fit_cols, clamped_rows)
        } else {
            (clamped_cols, (clamped_rows / 4).max(4))
        }
    } else {
        (clamped_cols, clamped_rows)
    };

    let max_px_w = eff_cols as u32;
    let max_px_h = (eff_rows as u32) * 2;

    let scale = (max_px_w as f64 / width_px as f64)
        .min(max_px_h as f64 / height_px as f64)
        .min(1.0);
    let target_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_px_w);
    let mut target_h = ((height_px as f64 * scale).round() as u32).clamp(2, max_px_h);
    if target_h % 2 != 0 {
        if target_h + 1 <= max_px_h {
            target_h += 1;
        } else {
            target_h = target_h.saturating_sub(1).max(2);
        }
    }
    (target_w, target_h)
}

fn compute_pixel_fit_for_protocol(
    width_px: u32,
    height_px: u32,
    max_cols: u16,
    max_rows: u16,
    cell_px: (u16, u16),
    protocol: TerminalImageProtocol,
) -> (u32, u32, u16, u16) {
    let cell_w = cell_px.0.max(1);
    let cell_h = cell_px.1.max(1);
    let clamped_cols = max_cols.max(4);
    let clamped_rows = if protocol == TerminalImageProtocol::Sixel {
        max_rows.max(1)
    } else {
        max_rows.max(2)
    };

    let is_scrollable = clamped_rows >= clamped_cols;
    let is_tall = (height_px as u64) * 4 > (width_px as u64) * 5;
    let (eff_cols, eff_rows, px_height_cap) = if is_scrollable {
        if is_tall {
            let fit_cols = if clamped_rows > 140 {
                clamped_cols
            } else {
                clamped_cols.min(92)
            };
            (fit_cols, clamped_rows, MAX_SCROLLABLE_PIXEL_HEIGHT)
        } else {
            (
                clamped_cols,
                (clamped_rows / 4).max(4),
                MAX_SCROLLABLE_PIXEL_HEIGHT,
            )
        }
    } else {
        (clamped_cols, clamped_rows, MAX_PIXEL_HEIGHT)
    };

    let raw_max_h = (eff_rows as u32 * cell_h as u32)
        .min(px_height_cap)
        .max(1);
    let max_target_h = if protocol == TerminalImageProtocol::Sixel && !is_scrollable {
        let sixel_safe_h = compute_sixel_safe_pixel_height(eff_rows, cell_h);
        if sixel_safe_h >= 6 {
            sixel_safe_h.min(raw_max_h)
        } else {
            raw_max_h
        }
    } else {
        raw_max_h
    };

    let max_target_w = (eff_cols as u32 * cell_w as u32)
        .min(MAX_PIXEL_WIDTH)
        .max(1);
    let box_px_w = max_target_w as f64;
    let box_px_h = max_target_h as f64;
    let scale = (box_px_w / width_px as f64)
        .min(box_px_h / height_px as f64)
        .min(2.0);

    let target_px_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_target_w);
    let target_px_h = ((height_px as f64 * scale).round() as u32).clamp(1, max_target_h);

    let occ_cols = (target_px_w.div_ceil(cell_w as u32)).clamp(1, eff_cols.max(1) as u32) as u16;
    let occ_rows = if protocol == TerminalImageProtocol::Sixel && !is_scrollable {
        let band_px_h = target_px_h.div_ceil(6) * 6;
        (band_px_h.div_ceil(cell_h as u32)).clamp(1, eff_rows.max(1) as u32) as u16
    } else {
        (target_px_h.div_ceil(cell_h as u32)).clamp(1, eff_rows.max(1) as u32) as u16
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

fn fit_within_max_pixels(width_px: u32, height_px: u32) -> (u32, u32) {
    fit_within_max_bounds(width_px, height_px, MAX_PIXEL_WIDTH, MAX_PIXEL_HEIGHT)
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

fn quantize_rgba_to_256(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
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
        return (exact_palette, indexed);
    }

    // Median-cut over 5-bit-per-channel RGB buckets for > 256 distinct colors.
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

            let span_r = max_r - min_r;
            let span_g = max_g - min_g;
            let span_b = max_b - min_b;
            let (channel, max_span) = if span_r >= span_g && span_r >= span_b {
                (0u8, span_r)
            } else if span_g >= span_b {
                (1u8, span_g)
            } else {
                (2u8, span_b)
            };

            let score = (max_span as u64) * pop;
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
        let mut split_rel = 1usize;
        for (idx, b) in slice.iter().enumerate() {
            accum += b.count as u64;
            if accum >= half_pop && idx + 1 < slice.len() {
                split_rel = (idx + 1).max(1);
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

    // Floyd-Steinberg error diffusion dithering across 2D pixel grid with edge clamping.
    let mut work: Vec<[f32; 3]> = flat_rgba
        .chunks_exact(4)
        .take(pixel_count)
        .map(|px| [px[0] as f32, px[1] as f32, px[2] as f32])
        .collect();

    for y in 0..height {
        let row_base = y * width;
        for x in 0..width {
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
                let mut best_dist = i32::MAX;
                for (pi, &(pr, pg, pb)) in raw_palette.iter().enumerate() {
                    let dr = r as i32 - pr as i32;
                    let dg = g as i32 - pg as i32;
                    let db = b as i32 - pb as i32;
                    let dist = dr * dr + dg * dg + db * db;
                    if dist < best_dist {
                        best_dist = dist;
                        best_i = pi as u8;
                        if dist == 0 {
                            break;
                        }
                    }
                }
                lookup_cache.insert(key, best_i);
                best_i
            };

            used[best_idx as usize] = true;
            indexed[pos] = best_idx;

            let (pr, pg, pb) = raw_palette[best_idx as usize];
            let er = r as f32 - pr as f32;
            let eg = g as f32 - pg as f32;
            let eb = b as f32 - pb as f32;

            let diffuse = |buf: &mut [[f32; 3]], idx: usize, weight: f32| {
                let p = &mut buf[idx];
                p[0] = (p[0] + er * weight).clamp(0.0, 255.0);
                p[1] = (p[1] + eg * weight).clamp(0.0, 255.0);
                p[2] = (p[2] + eb * weight).clamp(0.0, 255.0);
            };

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
        }
    }

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

    (compact_palette, indexed)
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
    match caps.protocol {
        TerminalImageProtocol::ITerm2 => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let (cols, _) = compute_target_dims(width_px, height_px, max_cols, max_rows);
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
            {
                bytes
            } else {
                let (fit_w, fit_h) = fit_within_max_pixels(width_px, height_px);
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
                && width_px <= MAX_PIXEL_WIDTH
                && height_px <= MAX_PIXEL_HEIGHT
            {
                bytes
            } else if width_px <= MAX_PIXEL_WIDTH && height_px <= MAX_PIXEL_HEIGHT {
                png_owned = encode_png(&dyn_img)?;
                &png_owned
            } else {
                let (fit_w, fit_h) = fit_within_max_pixels(width_px, height_px);
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
                max_cols,
                max_rows,
                DEFAULT_PREVIEW_BG_RGB,
                caps,
            )?;
            let mut out = rendered.pixel_payload.unwrap_or_default();
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Halfblock => {
            let rendered =
                decode_and_render_halfblocks(bytes, max_cols, max_rows, DEFAULT_PREVIEW_BG_RGB)?;
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
        assert!(!is_image_attachment("image/svg+xml", "diagram.svg"));
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

        // WezTerm + WT_SESSION -> ITerm2 wins over WT_SESSION
        let mut wez_wt_env = HashMap::new();
        wez_wt_env.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        wez_wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_wt_env),
            TerminalImageProtocol::ITerm2
        );

        let mut iterm_env = HashMap::new();
        iterm_env.insert("TERM_PROGRAM", "iTerm.app");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &iterm_env),
            TerminalImageProtocol::ITerm2
        );

        // WT_SESSION alone -> Sixel
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Sixel
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
            decode_and_render_for_protocol(&solid_png, 40, 20, (0, 0, 0), caps).unwrap();
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
            decode_and_render_for_protocol(&grad_png, 20, 10, (12, 24, 36), caps).unwrap();
        let grad_payload = grad_rendered.pixel_payload.as_deref().unwrap();
        assert!(grad_payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(grad_payload.contains(";2;"));
        assert!(grad_payload.contains('-'));
        assert!(grad_payload.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_and_iterm2_tui_vs_cli_stdout_payloads() {
        let png = make_test_png(16, 8);

        // TUI Kitty payload: fixed image id 4242, q=2, c=..., r=..., C=1, no trailing newline
        let kitty_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let kitty_tui =
            decode_and_render_for_protocol(&png, 20, 10, (0, 0, 0), kitty_caps).unwrap();
        let kitty_payload = kitty_tui.pixel_payload.as_deref().unwrap();
        assert!(kitty_payload.starts_with(&format!(
            "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={},r={},C=1,m=",
            kitty_tui.cols, kitty_tui.rows
        )));
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
            decode_and_render_for_protocol(&png, 20, 10, (0, 0, 0), iterm_caps).unwrap();
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
                decode_and_render_for_protocol(b"bad-bytes", 16, 8, (0, 0, 0), caps).is_err()
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
            decode_and_render_for_protocol(&jpeg_bytes, 20, 10, (0, 0, 0), caps).unwrap();
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
        // MIME & filename classification must never accept HTML/XHTML/SVG
        assert!(!is_image_mime_or_name("text/html", Some("photo.png")));
        assert!(!is_image_mime_or_name("text/html; charset=utf-8", None));
        assert!(!is_image_mime_or_name("application/xhtml+xml", Some("x.png")));
        assert!(!is_image_mime_or_name("image/svg+xml", Some("icon.svg")));
        assert!(!is_image_mime_or_name("image/svg+xml; charset=utf-8", Some("logo.png")));
        assert!(!is_image_mime_or_name("image/png", Some("login.html")));
        assert!(!is_image_mime_or_name("image/png", Some("page.HTM")));
        assert!(!is_image_mime_or_name("application/octet-stream", Some("vector.SVG")));
        assert!(!is_image_attachment("text/html", "attachment.png"));
        assert!(!is_image_attachment("image/svg+xml", "diagram.svg"));

        // Byte sniffing
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
                let err = decode_and_render_for_protocol(sample, 40, 20, (0, 0, 0), caps)
                    .unwrap_err()
                    .to_string();
                assert!(
                    err.contains("Received HTML document instead of raster image"),
                    "unexpected {proto:?} HTML error: {err}"
                );
            }
        }

        let svg_samples: &[&[u8]] = &[
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>",
            b"<?xml version=\"1.0\"?><svg viewBox=\"0 0 10 10\"/>",
            b"\xEF\xBB\xBF  <!-- icon -->\n<!DOCTYPE svg><SVG width=\"24\"></SVG>",
        ];
        for sample in svg_samples {
            assert!(looks_like_svg_bytes(sample), "expected SVG sniff for {:?}", sample);
            assert!(!looks_like_html_bytes(sample), "SVG should not sniff as HTML");

            let err_hb = decode_and_render_halfblocks(sample, 40, 20, (0, 0, 0))
                .unwrap_err()
                .to_string();
            assert!(
                err_hb.contains("SVG vector image cannot be raster-decoded"),
                "unexpected halfblock SVG error: {err_hb}"
            );
            let caps = TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Sixel,
                ..Default::default()
            };
            let err_px = decode_and_render_for_protocol(sample, 40, 20, (0, 0, 0), caps)
                .unwrap_err()
                .to_string();
            assert!(
                err_px.contains("SVG vector image cannot be raster-decoded"),
                "unexpected Sixel SVG error: {err_px}"
            );
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
            let rendered = decode_and_render_for_protocol(&png, 80, 120, (20, 20, 20), caps).unwrap();
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
}
