//! Terminal inline image decoder & renderer (24-bit Truecolor Half-Block `▀` for `ratatui` + Kitty/iTerm2/ANSI CLI output).

use std::io::Cursor;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Default dark terminal background (`#1E1F22`) used for alpha compositing in TUI half-blocks.
pub const DEFAULT_PREVIEW_BG_RGB: (u8, u8, u8) = (0x1E, 0x1F, 0x22);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalGraphicsProtocol {
    Kitty,
    ITerm2,
    HalfBlocks,
}

#[derive(Debug, Clone)]
pub struct RenderedTerminalImage {
    pub width_px: u32,
    pub height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Line<'static>>,
}

/// Returns `true` if a MIME type or filename represents a supported raster image (`png`, `jpeg`, `jpg`, `gif`, `webp`).
pub fn is_image_attachment(mime_type: &str, filename: &str) -> bool {
    let m = mime_type.trim().to_ascii_lowercase();
    if m.starts_with("image/") && !m.contains("svg") {
        return true;
    }
    let f = filename.trim().to_ascii_lowercase();
    f.ends_with(".png")
        || f.ends_with(".jpg")
        || f.ends_with(".jpeg")
        || f.ends_with(".gif")
        || f.ends_with(".webp")
}

/// Detects the preferred stdout inline image protocol from environment variables.
pub fn detect_stdout_protocol() -> TerminalGraphicsProtocol {
    if let Ok(proto) = std::env::var("ATLASSIAN_IMAGE_PROTOCOL") {
        let lower = proto.trim().to_ascii_lowercase();
        match lower.as_str() {
            "kitty" => return TerminalGraphicsProtocol::Kitty,
            "iterm2" | "iterm" => return TerminalGraphicsProtocol::ITerm2,
            "halfblock" | "halfblocks" | "ansi" => return TerminalGraphicsProtocol::HalfBlocks,
            _ => {}
        }
    }

    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let wezterm_exec = std::env::var("WEZTERM_EXECUTABLE").unwrap_or_default();
    if matches!(term_program.as_str(), "iterm.app" | "wezterm") || !wezterm_exec.trim().is_empty() {
        return TerminalGraphicsProtocol::ITerm2;
    }

    let kitty_win = std::env::var("KITTY_WINDOW_ID").unwrap_or_default();
    if !kitty_win.trim().is_empty() || term_program == "ghostty" {
        return TerminalGraphicsProtocol::Kitty;
    }

    TerminalGraphicsProtocol::HalfBlocks
}

/// Decodes raster image bytes and renders aspect-ratio-preserving Truecolor Half-Blocks (`▀`)
/// alpha-composited against `#1E1F22` for `ratatui`.
pub fn render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
) -> Result<RenderedTerminalImage> {
    render_halfblocks_with_bg(bytes, max_cols, max_rows, DEFAULT_PREVIEW_BG_RGB)
}

/// Decodes raster image bytes and renders Truecolor Half-Blocks (`▀`) composited over `bg_rgb`.
pub fn render_halfblocks_with_bg(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    let dyn_img = image::load_from_memory(bytes).context("이미지 디코딩 실패")?;
    let rgba = dyn_img.to_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("이미지 크기가 0x0입니다");
    }

    let clamped_cols = max_cols.max(4);
    let clamped_rows = max_rows.max(2);
    let max_px_w = clamped_cols as u32;
    let max_px_h = (clamped_rows as u32) * 2;

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

    let resized = image::imageops::resize(&rgba, target_w, target_h, FilterType::Triangle);
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
        lines,
    })
}

/// Renders an image for CLI stdout, auto-detecting Kitty / iTerm2 / ANSI 24-bit Half-Blocks.
pub fn render_for_stdout(bytes: &[u8], max_cols: u16, max_rows: u16) -> Result<String> {
    render_for_stdout_with_protocol(bytes, max_cols, max_rows, detect_stdout_protocol())
}

/// Renders an image for CLI stdout using an explicit graphics protocol.
pub fn render_for_stdout_with_protocol(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    protocol: TerminalGraphicsProtocol,
) -> Result<String> {
    let rendered = render_halfblocks(bytes, max_cols, max_rows)?;

    match protocol {
        TerminalGraphicsProtocol::ITerm2 => {
            let b64 = B64_STANDARD.encode(bytes);
            Ok(format!(
                "\x1b]1337;File=inline=1;width={};preserveAspectRatio=1:{b64}\x07\n",
                rendered.cols
            ))
        }
        TerminalGraphicsProtocol::Kitty => {
            let png_bytes = if matches!(image::guess_format(bytes), Ok(ImageFormat::Png)) {
                bytes.to_vec()
            } else {
                let dyn_img = image::load_from_memory(bytes).context("이미지 디코딩 실패")?;
                encode_png(&dyn_img)?
            };
            let b64 = B64_STANDARD.encode(&png_bytes);
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
                    out.push_str(&format!("\x1b_Gf=100,a=T,m={more};{chunk}\x1b\\"));
                } else {
                    out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                }
            }
            out.push('\n');
            Ok(out)
        }
        TerminalGraphicsProtocol::HalfBlocks => {
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
                    out.push_str(&format!(
                        "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_c};{bb}m{}",
                        span.content
                    ));
                }
                out.push_str("\x1b[0m\n");
            }
            Ok(out)
        }
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
        .context("PNG 인코딩 실패")?;
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

    #[test]
    fn test_is_image_attachment() {
        assert!(is_image_attachment("image/png", "shot.bin"));
        assert!(is_image_attachment("application/octet-stream", "photo.WEBP"));
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
    }

    #[test]
    fn test_decode_rejects_invalid_bytes() {
        assert!(render_halfblocks(b"not-an-image", 20, 10).is_err());
        assert!(render_for_stdout(b"not-an-image", 20, 10).is_err());
    }
}
