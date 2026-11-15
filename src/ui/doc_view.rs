//! Rich `DocBlock` -> styled `Vec<Line<'static>>` renderer (CJK Unicode width-aware).

use std::collections::HashMap;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    img_preview::RenderedTerminalImage,
    models::{DocBlock, InlineNode, InlineStyle, TaskItem},
    ui::theme::Theme,
};

/// Truncates `text` so its terminal display width is at most `max_width`, appending `…` if cut.
pub fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > target {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Pads `text` on the right with spaces to reach at least `target_width` display columns.
pub fn pad_right_width(text: &str, target_width: usize) -> String {
    let w = text.width();
    if w >= target_width {
        text.to_string()
    } else {
        let mut out = String::with_capacity(text.len() + (target_width - w));
        out.push_str(text);
        for _ in 0..(target_width - w) {
            out.push(' ');
        }
        out
    }
}

/// Wraps plain text into lines of at most `max_width` display columns (CJK-aware, UTF-8 safe).
pub fn wrap_plain_text(text: &str, max_width: usize) -> Vec<String> {
    let w = max_width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut cur_w = 0usize;
        for ch in line.chars() {
            let cw = ch.width().unwrap_or(0);
            if cur_w + cw > w && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                cur_w = 0;
            }
            current.push(ch);
            cur_w += cw;
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Renders a slice of `DocBlock`s into styled ratatui `Line`s wrapped to `width`.
pub fn render_blocks(
    blocks: &[DocBlock],
    width: u16,
    theme: &Theme,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    inline_images: bool,
) -> Vec<Line<'static>> {
    let w = (width as usize).max(16);
    let mut out = Vec::new();
    render_block_list(blocks, w, theme, image_cache, inline_images, &[], &mut out);
    // Trim trailing blank lines for tidy layout.
    while out.last().is_some_and(|l| l.spans.is_empty() || l.width() == 0) {
        out.pop();
    }
    out
}

/// Renders a slice of `InlineNode`s into styled `Span`s.
pub fn inline_nodes_to_spans(nodes: &[InlineNode], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for node in nodes {
        match node {
            InlineNode::Text { text, style } => {
                spans.push(Span::styled(text.clone(), map_inline_style(style, theme)));
            }
            InlineNode::Mention { display, .. } => {
                let label = if display.starts_with('@') {
                    display.clone()
                } else {
                    format!("@{display}")
                };
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .bg(theme.mention_bg)
                        .fg(theme.mention_fg)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            InlineNode::StatusLozenge { text, category } => {
                spans.push(theme.status_lozenge_span(text, *category));
            }
            InlineNode::IssueRef { key } => {
                spans.push(Span::styled(
                    format!("[{key}]"),
                    Style::default()
                        .bg(theme.chip_bg)
                        .fg(theme.chip_fg)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            InlineNode::PageRef { title, .. } => {
                spans.push(Span::styled(
                    format!("[📄 {title}]"),
                    Style::default()
                        .bg(Color::Rgb(16, 62, 74))
                        .fg(theme.confluence_teal)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            InlineNode::Emoji { text } => {
                spans.push(Span::raw(text.clone()));
            }
            InlineNode::HardBreak => {
                spans.push(Span::raw("\n"));
            }
        }
    }
    spans
}

pub fn inline_nodes_plain_text(nodes: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            InlineNode::Text { text, .. } => out.push_str(text),
            InlineNode::Mention { display, .. } => {
                if !display.starts_with('@') {
                    out.push('@');
                }
                out.push_str(display);
            }
            InlineNode::StatusLozenge { text, category } => {
                let lbl = if text.trim().is_empty() {
                    category.badge_label()
                } else {
                    text.trim()
                };
                out.push('[');
                out.push_str(&lbl.to_ascii_uppercase());
                out.push(']');
            }
            InlineNode::IssueRef { key } => {
                out.push('[');
                out.push_str(key);
                out.push(']');
            }
            InlineNode::PageRef { title, .. } => {
                out.push_str("[📄 ");
                out.push_str(title);
                out.push(']');
            }
            InlineNode::Emoji { text } => out.push_str(text),
            InlineNode::HardBreak => out.push('\n'),
        }
    }
    out
}

fn map_inline_style(style: &InlineStyle, theme: &Theme) -> Style {
    let mut st = Style::default();
    if style.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if style.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.underline {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if style.code {
        st = st.fg(theme.code_fg).bg(theme.code_bg);
    }
    if style.link_url.is_some() {
        st = st.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
    }
    st
}

fn render_block_list(
    blocks: &[DocBlock],
    width: usize,
    theme: &Theme,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    inline_images: bool,
    prefix: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let prefix_w: usize = prefix.iter().map(|s| s.content.width()).sum();
    let inner_w = width.saturating_sub(prefix_w).max(8);

    for (idx, block) in blocks.iter().enumerate() {
        match block {
            DocBlock::Paragraph { spans } => {
                let styled = inline_nodes_to_spans(spans, theme);
                let wrapped = wrap_spans_with_prefix(&styled, inner_w, prefix, prefix);
                if wrapped.is_empty() {
                    out.push(Line::from(prefix.to_vec()));
                } else {
                    out.extend(wrapped);
                }
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Heading { level, spans } => {
                let (pfx_str, style) = heading_style(*level, theme);
                let mut h_spans = vec![Span::styled(pfx_str.to_string(), style)];
                for mut sp in inline_nodes_to_spans(spans, theme) {
                    sp.style = sp.style.patch(style);
                    h_spans.push(sp);
                }
                out.extend(wrap_spans_with_prefix(&h_spans, inner_w, prefix, prefix));
                if *level <= 2 {
                    let rule_w = inner_w.min(48);
                    let mut r_spans = prefix.to_vec();
                    r_spans.push(Span::styled(
                        "─".repeat(rule_w),
                        Style::default().fg(theme.border),
                    ));
                    out.push(Line::from(r_spans));
                }
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::BulletList { items } => {
                for item in items {
                    render_list_item(
                        item,
                        "• ",
                        "  ",
                        Style::default().fg(theme.accent),
                        inner_w,
                        theme,
                        image_cache,
                        inline_images,
                        prefix,
                        out,
                    );
                }
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::OrderedList { start, items } => {
                for (i, item) in items.iter().enumerate() {
                    let marker = format!("{}. ", start + i);
                    let cont = " ".repeat(marker.width());
                    render_list_item(
                        item,
                        &marker,
                        &cont,
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                        inner_w,
                        theme,
                        image_cache,
                        inline_images,
                        prefix,
                        out,
                    );
                }
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::TaskList { items } => {
                for TaskItem { checked, content } in items {
                    let (icon, icon_style) = if *checked {
                        ("☑ ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD))
                    } else {
                        ("☐ ", Style::default().fg(theme.muted))
                    };
                    let mut first_pfx = prefix.to_vec();
                    first_pfx.push(Span::styled(icon, icon_style));
                    let mut cont_pfx = prefix.to_vec();
                    cont_pfx.push(Span::raw("  "));
                    let mut spans = inline_nodes_to_spans(content, theme);
                    if *checked {
                        for sp in &mut spans {
                            sp.style = sp.style.fg(theme.muted).add_modifier(Modifier::CROSSED_OUT);
                        }
                    }
                    out.extend(wrap_spans_with_prefix(
                        &spans,
                        inner_w.saturating_sub(2).max(6),
                        &first_pfx,
                        &cont_pfx,
                    ));
                }
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::CodeBlock { language, code } => {
                render_code_block(language.as_deref(), code, inner_w, theme, prefix, out);
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Blockquote { blocks: inner } => {
                let mut bq_pfx = prefix.to_vec();
                bq_pfx.push(Span::styled("│ ", Style::default().fg(theme.border)));
                render_block_list(inner, width, theme, image_cache, inline_images, &bq_pfx, out);
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Panel {
                kind,
                title,
                blocks: inner,
            } => {
                let (color, icon, default_lbl) = theme.panel_meta(*kind);
                let border_span = Span::styled("▌ ", Style::default().fg(color).add_modifier(Modifier::BOLD));
                let mut hdr = prefix.to_vec();
                hdr.push(border_span.clone());
                let title_text = title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or(default_lbl);
                hdr.push(Span::styled(
                    format!("{icon} {title_text}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
                out.push(Line::from(hdr));

                let mut panel_pfx = prefix.to_vec();
                panel_pfx.push(border_span);
                render_block_list(inner, width, theme, image_cache, inline_images, &panel_pfx, out);
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Expand {
                title,
                blocks: inner,
            } => {
                let mut hdr = prefix.to_vec();
                hdr.push(Span::styled(
                    format!("▾ {title}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
                out.push(Line::from(hdr));

                let mut exp_pfx = prefix.to_vec();
                exp_pfx.push(Span::styled("  ", theme.muted_style()));
                render_block_list(inner, width, theme, image_cache, inline_images, &exp_pfx, out);
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Table { headers, rows } => {
                render_table(headers, rows, inner_w, theme, prefix, out);
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Media {
                id_or_filename,
                alt,
                ..
            } => {
                let label = alt
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(id_or_filename);
                if inline_images {
                    if let Some(cache) = image_cache {
                        if let Some(img) = cache.get(id_or_filename) {
                            for line in &img.lines {
                                let mut sp = prefix.to_vec();
                                sp.extend(line.spans.clone());
                                out.push(Line::from(sp));
                            }
                            continue;
                        }
                    }
                }
                let mut sp = prefix.to_vec();
                sp.push(Span::styled(
                    format!("[🖼 {label} — press v to preview]"),
                    Style::default()
                        .bg(theme.chip_bg)
                        .fg(theme.chip_fg)
                        .add_modifier(Modifier::BOLD),
                ));
                out.push(Line::from(sp));
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
            DocBlock::Rule => {
                let mut sp = prefix.to_vec();
                sp.push(Span::styled(
                    "─".repeat(inner_w.min(64)),
                    Style::default().fg(theme.border),
                ));
                out.push(Line::from(sp));
                if idx + 1 < blocks.len() {
                    out.push(Line::from(prefix.to_vec()));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list_item(
    item_blocks: &[DocBlock],
    marker: &str,
    cont_pad: &str,
    marker_style: Style,
    inner_w: usize,
    theme: &Theme,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    inline_images: bool,
    prefix: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let mut sub_out = Vec::new();
    let avail = inner_w.saturating_sub(marker.width()).max(8);
    render_block_list(item_blocks, avail, theme, image_cache, inline_images, &[], &mut sub_out);
    if sub_out.is_empty() {
        let mut sp = prefix.to_vec();
        sp.push(Span::styled(marker.to_string(), marker_style));
        out.push(Line::from(sp));
        return;
    }
    for (i, line) in sub_out.into_iter().enumerate() {
        let mut sp = prefix.to_vec();
        if i == 0 {
            sp.push(Span::styled(marker.to_string(), marker_style));
        } else {
            sp.push(Span::raw(cont_pad.to_string()));
        }
        sp.extend(line.spans);
        out.push(Line::from(sp));
    }
}

fn heading_style(level: u8, theme: &Theme) -> (&'static str, Style) {
    match level {
        1 => (
            "H1 ",
            Style::default()
                .fg(theme.confluence_teal)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
        2 => (
            "H2 ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        3 => (
            "H3 ",
            Style::default()
                .fg(Color::Rgb(179, 212, 255))
                .add_modifier(Modifier::BOLD),
        ),
        4 => (
            "H4 ",
            Style::default()
                .fg(theme.purple)
                .add_modifier(Modifier::BOLD),
        ),
        5 => (
            "H5 ",
            Style::default()
                .fg(theme.amber)
                .add_modifier(Modifier::BOLD),
        ),
        _ => (
            "H6 ",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

fn render_code_block(
    language: Option<&str>,
    code: &str,
    inner_w: usize,
    theme: &Theme,
    prefix: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let box_w = inner_w.clamp(12, 96);
    let lang_label = language
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("code");
    let header_title = format!("─ {lang_label} ");
    let top_fill = box_w
        .saturating_sub(2 + header_title.width())
        .max(1);
    let mut top = prefix.to_vec();
    top.push(Span::styled(
        format!("┌{header_title}{}┐", "─".repeat(top_fill)),
        Style::default().fg(theme.border),
    ));
    out.push(Line::from(top));

    let code_inner_w = box_w.saturating_sub(4).max(4);
    for raw_line in code.lines() {
        let wrapped = wrap_plain_text(raw_line, code_inner_w);
        for seg in wrapped {
            let padded = pad_right_width(&seg, code_inner_w);
            let mut row = prefix.to_vec();
            row.push(Span::styled("│ ", Style::default().fg(theme.border)));
            row.push(Span::styled(padded, theme.code_style()));
            row.push(Span::styled(" │", Style::default().fg(theme.border)));
            out.push(Line::from(row));
        }
    }
    if code.is_empty() {
        let mut row = prefix.to_vec();
        row.push(Span::styled("│ ", Style::default().fg(theme.border)));
        row.push(Span::styled(" ".repeat(code_inner_w), theme.code_style()));
        row.push(Span::styled(" │", Style::default().fg(theme.border)));
        out.push(Line::from(row));
    }

    let mut bottom = prefix.to_vec();
    bottom.push(Span::styled(
        format!("└{}┘", "─".repeat(box_w.saturating_sub(2))),
        Style::default().fg(theme.border),
    ));
    out.push(Line::from(bottom));
}

fn render_table(
    headers: &[Vec<InlineNode>],
    rows: &[Vec<Vec<InlineNode>>],
    inner_w: usize,
    theme: &Theme,
    prefix: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let col_count = headers
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return;
    }

    let mut col_widths = vec![4usize; col_count];
    for (c, cell) in headers.iter().enumerate() {
        let txt = inline_nodes_plain_text(cell);
        col_widths[c] = col_widths[c].max(txt.width());
    }
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let txt = inline_nodes_plain_text(cell);
            col_widths[c] = col_widths[c].max(txt.width());
        }
    }

    // Budget: borders use 1 + col_count * 3 (space + cell + space + │).
    let overhead = 1 + col_count * 3;
    let avail = inner_w.saturating_sub(overhead).max(col_count * 3);
    let total_req: usize = col_widths.iter().sum();
    if total_req > avail {
        // Proportionally shrink widest columns while keeping minimum 3 cols each.
        let mut remaining = total_req - avail;
        while remaining > 0 {
            let Some((idx, _)) = col_widths.iter().enumerate().max_by_key(|(_, w)| *w) else {
                break;
            };
            if col_widths[idx] <= 3 {
                break;
            }
            col_widths[idx] -= 1;
            remaining -= 1;
        }
    }

    let border_style = Style::default().fg(theme.border);
    let make_border = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (i, w) in col_widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            if i + 1 < col_widths.len() {
                s.push_str(mid);
            }
        }
        s.push_str(right);
        let mut sp = prefix.to_vec();
        sp.push(Span::styled(s, border_style));
        Line::from(sp)
    };

    out.push(make_border("┌", "┬", "┐"));

    if !headers.is_empty() {
        let mut line_spans = prefix.to_vec();
        line_spans.push(Span::styled("│", border_style));
        for (c, w) in col_widths.iter().enumerate() {
            let txt = headers
                .get(c)
                .map(|n| inline_nodes_plain_text(n))
                .unwrap_or_default();
            let cell = pad_right_width(&truncate_width(&txt, *w), *w);
            line_spans.push(Span::styled(
                format!(" {cell} "),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            line_spans.push(Span::styled("│", border_style));
        }
        out.push(Line::from(line_spans));
        out.push(make_border("├", "┼", "┤"));
    }

    for row in rows {
        let mut line_spans = prefix.to_vec();
        line_spans.push(Span::styled("│", border_style));
        for (c, w) in col_widths.iter().enumerate() {
            let txt = row
                .get(c)
                .map(|n| inline_nodes_plain_text(n))
                .unwrap_or_default();
            let cell = pad_right_width(&truncate_width(&txt, *w), *w);
            line_spans.push(Span::raw(format!(" {cell} ")));
            line_spans.push(Span::styled("│", border_style));
        }
        out.push(Line::from(line_spans));
    }

    out.push(make_border("└", "┴", "┘"));
}

/// Wraps a sequence of styled spans into `Line<'static>`s fitting `width` display columns.
pub fn wrap_spans_with_prefix(
    spans: &[Span<'static>],
    width: usize,
    first_prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
) -> Vec<Line<'static>> {
    let max_w = width.max(4);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = first_prefix.to_vec();
    let mut cur_w = 0usize;

    let flush = |lines: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>, cur_w: &mut usize| {
        lines.push(Line::from(std::mem::take(cur)));
        *cur = cont_prefix.to_vec();
        *cur_w = 0;
    };

    for span in spans {
        let style = span.style;
        let text = span.content.as_ref();
        // Atomic chips/pills shouldn't break mid-token if possible.
        let is_chip = style.bg.is_some();

        for (seg_idx, segment) in text.split('\n').enumerate() {
            if seg_idx > 0 {
                flush(&mut lines, &mut cur_spans, &mut cur_w);
            }
            if segment.is_empty() {
                continue;
            }
            if is_chip {
                let sw = segment.width();
                if cur_w > 0 && cur_w + sw > max_w {
                    flush(&mut lines, &mut cur_spans, &mut cur_w);
                }
                cur_spans.push(Span::styled(segment.to_string(), style));
                cur_w += sw;
                continue;
            }

            let mut buf = String::new();
            for ch in segment.chars() {
                let cw = ch.width().unwrap_or(0);
                if cur_w + cw > max_w && (cur_w > 0 || !buf.is_empty()) {
                    if !buf.is_empty() {
                        cur_spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    flush(&mut lines, &mut cur_spans, &mut cur_w);
                }
                buf.push(ch);
                cur_w += cw;
            }
            if !buf.is_empty() {
                cur_spans.push(Span::styled(buf, style));
            }
        }
    }

    if cur_w > 0 || lines.is_empty() {
        lines.push(Line::from(cur_spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PanelKind, StatusCategory};

    fn lines_to_strings(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_status_lozenge_panels_tables_and_code_blocks() {
        let theme = Theme::default();
        let blocks = vec![
            DocBlock::Paragraph {
                spans: vec![
                    InlineNode::Text {
                        text: "현재 상태: ".into(),
                        style: InlineStyle::default(),
                    },
                    InlineNode::StatusLozenge {
                        text: "In Review".into(),
                        category: StatusCategory::InProgress,
                    },
                ],
            },
            DocBlock::Panel {
                kind: PanelKind::Info,
                title: None,
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![InlineNode::Text {
                        text: "정보 패널 본문입니다".into(),
                        style: InlineStyle::default(),
                    }],
                }],
            },
            DocBlock::Panel {
                kind: PanelKind::Warning,
                title: Some("주의".into()),
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![InlineNode::Text {
                        text: "배포 전 확인".into(),
                        style: InlineStyle::default(),
                    }],
                }],
            },
            DocBlock::Table {
                headers: vec![
                    vec![InlineNode::Text {
                        text: "키".into(),
                        style: InlineStyle::default(),
                    }],
                    vec![InlineNode::Text {
                        text: "담당자".into(),
                        style: InlineStyle::default(),
                    }],
                ],
                rows: vec![vec![
                    vec![InlineNode::Text {
                        text: "ATL-1".into(),
                        style: InlineStyle::default(),
                    }],
                    vec![InlineNode::Text {
                        text: "김철수".into(),
                        style: InlineStyle::default(),
                    }],
                ]],
            },
            DocBlock::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {}".into(),
            },
        ];

        let rendered = render_blocks(&blocks, 80, &theme, None, false);
        let text = lines_to_strings(&rendered).join("\n");

        assert!(text.contains(" IN REVIEW "), "expected lozenge in:\n{text}");
        assert!(text.contains("▌ ℹ Info"), "expected info panel in:\n{text}");
        assert!(text.contains("▌ ⚠ 주의"), "expected warning panel in:\n{text}");
        assert!(text.contains("┌"), "expected box-drawing table/code in:\n{text}");
        assert!(text.contains("┬"), "expected table header separator in:\n{text}");
        assert!(text.contains("│ ATL-1"), "expected table row in:\n{text}");
        assert!(text.contains("┌─ rust"), "expected code block header in:\n{text}");
        assert!(text.contains("fn main() {}"), "expected code body in:\n{text}");
    }

    #[test]
    fn cjk_wrapping_and_truncation_never_split_utf8() {
        let s = "한글테스트문장입니다";
        let truncated = truncate_width(s, 7);
        assert!(truncated.width() <= 7);
        assert!(truncated.ends_with('…'));

        let wrapped = wrap_plain_text(s, 6);
        assert!(wrapped.iter().all(|l| l.width() <= 6));
        assert_eq!(wrapped.concat(), s);
    }
}
