//! Link Extractor modal overlay (`u`) for Jira issues and Confluence pages.
//! Extracts Markdown `[label](url)`, ADF/DocBlock links, bare `https://...` URLs,
//! web URLs, linked Confluence pages, and attachment URLs into a rounded modal
//! supporting `Enter` (open in browser), `y` (copy URL), and `Esc` (close).

use std::{collections::HashSet, sync::OnceLock};

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use regex::Regex;

use crate::{
    models::{DocBlock, InlineNode, IssueDetail, PageDetail},
    ui::{
        app::{AppState, UiState, WorkspaceTab},
        doc_view::truncate_width,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::Action,
        overlays::{
            centered_rect, modal_block, modal_pill_primary, modal_pill_secondary,
            register_modal_chrome, render_button_bar, OverlayOutcome,
        },
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub url: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCommand {
    Open(String),
    Copy(String),
}

#[derive(Debug, Clone)]
pub struct LinksOverlay {
    pub source_label: String,
    pub items: Vec<ExtractedLink>,
    pub selected: usize,
    pub pending_command: Option<LinkCommand>,
}

impl LinksOverlay {
    pub fn new(source_label: impl Into<String>, items: Vec<ExtractedLink>) -> Self {
        Self {
            source_label: source_label.into(),
            items,
            selected: 0,
            pending_command: None,
        }
    }

    pub fn from_app(app: &AppState, ui: &UiState) -> Self {
        if ui.active_tab == WorkspaceTab::Confluence {
            if let Some(page) = &app.active_page {
                let items = extract_links_from_page(page);
                return Self::new(page.summary.title.clone(), items);
            }
        }
        if let Some(issue) = &app.active_issue {
            let items = extract_links_from_issue(issue);
            return Self::new(issue.summary.key.clone(), items);
        }
        if let Some(page) = &app.active_page {
            let items = extract_links_from_page(page);
            return Self::new(page.summary.title.clone(), items);
        }
        Self::new("Workspace", Vec::new())
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }

    pub fn selected_link(&self) -> Option<&ExtractedLink> {
        self.items.get(self.selected)
    }

    pub fn take_pending_command(&mut self) -> Option<LinkCommand> {
        self.pending_command.take()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('u') => OverlayOutcome::Close,
            Action::MoveUp | Action::CursorUp | Action::PrevField => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::NextField => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = self.selected.saturating_sub(5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + 5).min(self.items.len() - 1);
                }
                OverlayOutcome::Handled
            }
            Action::ConfirmYes | Action::Input('y') | Action::Input('Y') => {
                if let Some(link) = self.selected_link() {
                    self.pending_command = Some(LinkCommand::Copy(link.url.clone()));
                }
                OverlayOutcome::Handled
            }
            Action::Activate
            | Action::Submit
            | Action::Input('o')
            | Action::Input('O')
            | Action::OpenInBrowser => {
                if let Some(link) = self.selected_link() {
                    self.pending_command = Some(LinkCommand::Open(link.url.clone()));
                    OverlayOutcome::Close
                } else {
                    OverlayOutcome::Close
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            82.min(area.width.saturating_sub(4)).max(34),
            20.min(area.height.saturating_sub(2)).max(8),
        );
        frame.render_widget(Clear, rect);

        let block = modal_block(
            format!(" Links — {} ({}) ", self.source_label, self.items.len()),
            theme,
            true,
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        register_modal_chrome(rect, frame, theme, hitmap, true);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let list_h = inner.height.saturating_sub(1) as usize;
        let mut lines = Vec::new();
        if self.items.is_empty() {
            lines.push(Line::from(Span::styled(
                " No URLs found in the active issue or page.",
                theme.muted_style(),
            )));
        } else {
            // Render 2 lines per link when there is room (label + URL), or 1 line on very short screens.
            let rows_per_item = if list_h >= 6 { 2usize } else { 1usize };
            let visible_items = (list_h / rows_per_item).max(1);
            let start = if self.selected >= visible_items {
                self.selected - visible_items + 1
            } else {
                0
            };
            let w = inner.width as usize;
            let mut cursor_y = inner.y;
            let max_y = inner.y + list_h as u16;

            for (vis_idx, link) in self
                .items
                .iter()
                .skip(start)
                .take(visible_items)
                .enumerate()
            {
                if cursor_y >= max_y {
                    break;
                }
                let abs_idx = start + vis_idx;
                let is_sel = abs_idx == self.selected;
                let row_style = if is_sel {
                    theme.selection()
                } else {
                    Style::default().fg(theme.fg)
                };
                let url_style = if is_sel {
                    theme.selection().remove_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.accent)
                };
                let prefix = if is_sel { "▸ " } else { "  " };
                let badge = format!("[{}] ", link.source);
                let num = format!("{}. ", abs_idx + 1);

                if rows_per_item == 2 && cursor_y + 1 < max_y {
                    let label_budget = w.saturating_sub(prefix.len() + num.len() + badge.len());
                    let display_label = if link.label.trim().is_empty() {
                        link.url.as_str()
                    } else {
                        link.label.as_str()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, row_style),
                        Span::styled(num, theme.muted_style().patch(row_style)),
                        Span::styled(
                            badge,
                            Style::default()
                                .fg(theme.confluence_teal)
                                .add_modifier(Modifier::BOLD)
                                .patch(if is_sel { row_style } else { Style::default() }),
                        ),
                        Span::styled(truncate_width(display_label, label_budget), row_style),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("     ", row_style),
                        Span::styled(truncate_width(&link.url, w.saturating_sub(5)), url_style),
                    ]));
                    hitmap.add_line(inner.x, cursor_y, inner.width, HitTarget::OverlayItem(abs_idx));
                    hitmap.add_line(
                        inner.x,
                        cursor_y + 1,
                        inner.width,
                        HitTarget::OverlayItem(abs_idx),
                    );
                    cursor_y += 2;
                } else {
                    let budget = w.saturating_sub(prefix.len() + num.len() + badge.len());
                    let combined = if link.label.trim().is_empty() || link.label == link.url {
                        link.url.clone()
                    } else {
                        format!("{} — {}", link.label, link.url)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, row_style),
                        Span::styled(num, theme.muted_style().patch(row_style)),
                        Span::styled(badge, theme.accent_style().patch(row_style)),
                        Span::styled(truncate_width(&combined, budget), row_style),
                    ]));
                    hitmap.add_line(inner.x, cursor_y, inner.width, HitTarget::OverlayItem(abs_idx));
                    cursor_y += 1;
                }
            }
        }

        let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        render_button_bar(
            frame,
            footer_area,
            &[
                ("[y] Copy URL", ButtonId::OverlayCopyUrl, modal_pill_primary(theme)),
                ("[o] Browser", ButtonId::OpenBrowser, modal_pill_secondary(theme)),
            ],
            Some("· ↑/↓ navigate · Enter open · Esc close"),
            theme,
            hitmap,
        );
    }
}

/// Extracts Markdown `[label](url)` links and bare `http(s)://...` URLs from raw text.
pub fn extract_links_from_text(text: &str, source: &str) -> Vec<ExtractedLink> {
    static MD_LINK_RE: OnceLock<Regex> = OnceLock::new();
    static BARE_URL_RE: OnceLock<Regex> = OnceLock::new();

    let md_re = MD_LINK_RE.get_or_init(|| {
        Regex::new(r"\[([^\[\]\n]+)\]\((https?://[^\s\)\]]+)\)").expect("valid md link regex")
    });
    let bare_re = BARE_URL_RE.get_or_init(|| {
        Regex::new(r#"https?://[^\s<>"'`]+"#).expect("valid bare url regex")
    });

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for cap in md_re.captures_iter(text) {
        let label = cap[1].trim().to_string();
        let url = clean_extracted_url(&cap[2]);
        if !url.is_empty() && seen.insert(url.clone()) {
            out.push(ExtractedLink {
                label: if label.is_empty() { url.clone() } else { label },
                url,
                source: source.to_string(),
            });
        }
    }

    for m in bare_re.find_iter(text) {
        let url = clean_extracted_url(m.as_str());
        if !url.is_empty() && seen.insert(url.clone()) {
            out.push(ExtractedLink {
                label: url.clone(),
                url,
                source: source.to_string(),
            });
        }
    }

    out
}

/// Recursively extracts ADF/DocBlock links (`InlineStyle::link_url`), media URLs, and inline bare URLs.
pub fn extract_links_from_blocks(blocks: &[DocBlock], source: &str) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    collect_links_from_blocks(blocks, source, &mut out);
    dedup_links(out)
}

fn collect_links_from_blocks(blocks: &[DocBlock], source: &str, out: &mut Vec<ExtractedLink>) {
    for block in blocks {
        match block {
            DocBlock::Paragraph { spans } | DocBlock::Heading { spans, .. } => {
                collect_links_from_inlines(spans, source, out);
            }
            DocBlock::BulletList { items } | DocBlock::OrderedList { items, .. } => {
                for item in items {
                    collect_links_from_blocks(item, source, out);
                }
            }
            DocBlock::TaskList { items } => {
                for item in items {
                    collect_links_from_inlines(&item.content, source, out);
                }
            }
            DocBlock::Blockquote { blocks: inner }
            | DocBlock::Panel { blocks: inner, .. }
            | DocBlock::Expand { blocks: inner, .. } => {
                collect_links_from_blocks(inner, source, out);
            }
            DocBlock::Table { headers, rows } => {
                for cell in headers {
                    collect_links_from_inlines(cell, source, out);
                }
                for row in rows {
                    for cell in row {
                        collect_links_from_inlines(cell, source, out);
                    }
                }
            }
            DocBlock::Media {
                id_or_filename,
                alt,
                url,
            } => {
                if let Some(u) = url.as_deref().map(clean_extracted_url).filter(|u| is_http_url(u))
                {
                    let label = alt
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(id_or_filename)
                        .to_string();
                    out.push(ExtractedLink {
                        label,
                        url: u,
                        source: source.to_string(),
                    });
                }
            }
            DocBlock::CodeBlock { code, .. } => {
                out.extend(extract_links_from_text(code, source));
            }
            DocBlock::Rule => {}
        }
    }
}

fn collect_links_from_inlines(nodes: &[InlineNode], source: &str, out: &mut Vec<ExtractedLink>) {
    for node in nodes {
        if let InlineNode::Text { text, style } = node {
            if let Some(raw_url) = style.link_url.as_deref() {
                let url = clean_extracted_url(raw_url);
                if is_http_url(&url) {
                    let trimmed = text.trim();
                    out.push(ExtractedLink {
                        label: if trimmed.is_empty() {
                            url.clone()
                        } else {
                            trimmed.to_string()
                        },
                        url,
                        source: source.to_string(),
                    });
                }
            }
            out.extend(extract_links_from_text(text, source));
        }
    }
}

/// Extracts and deduplicates all URLs from a Jira `IssueDetail`.
pub fn extract_links_from_issue(issue: &IssueDetail) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    let web_url = clean_extracted_url(&issue.summary.web_url);
    if is_http_url(&web_url) {
        out.push(ExtractedLink {
            label: format!("{} — {}", issue.summary.key, issue.summary.summary),
            url: web_url,
            source: "Issue".to_string(),
        });
    }

    collect_links_from_blocks(&issue.description_blocks, "Description", &mut out);
    out.extend(extract_links_from_text(
        &issue.description_markdown,
        "Description",
    ));

    for lp in &issue.linked_pages {
        let u = clean_extracted_url(&lp.web_url);
        if is_http_url(&u) {
            out.push(ExtractedLink {
                label: if lp.title.trim().is_empty() {
                    u.clone()
                } else {
                    lp.title.clone()
                },
                url: u,
                source: "Confluence".to_string(),
            });
        }
    }

    for att in &issue.attachments {
        let u = clean_extracted_url(&att.content_url);
        if is_http_url(&u) {
            out.push(ExtractedLink {
                label: att.filename.clone(),
                url: u,
                source: "Attachment".to_string(),
            });
        }
    }

    for comment in &issue.comments {
        collect_links_from_blocks(&comment.blocks, "Comment", &mut out);
        out.extend(extract_links_from_text(&comment.body_markdown, "Comment"));
    }

    dedup_links(out)
}

/// Extracts and deduplicates all URLs from a Confluence `PageDetail`.
pub fn extract_links_from_page(page: &PageDetail) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    let web_url = clean_extracted_url(&page.summary.web_url);
    if is_http_url(&web_url) {
        out.push(ExtractedLink {
            label: page.summary.title.clone(),
            url: web_url,
            source: "Page".to_string(),
        });
    }

    collect_links_from_blocks(&page.blocks, "Body", &mut out);
    out.extend(extract_links_from_text(&page.body_markdown, "Body"));

    for att in &page.attachments {
        let u = clean_extracted_url(&att.content_url);
        if is_http_url(&u) {
            out.push(ExtractedLink {
                label: att.filename.clone(),
                url: u,
                source: "Attachment".to_string(),
            });
        }
    }

    for comment in &page.comments {
        collect_links_from_blocks(&comment.blocks, "Comment", &mut out);
        out.extend(extract_links_from_text(&comment.body_markdown, "Comment"));
    }

    dedup_links(out)
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn clean_extracted_url(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    loop {
        let last = match s.chars().last() {
            Some(c) => c,
            None => break,
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '>' | '"' | '\'' => true,
            ')' => {
                let opens = s.chars().filter(|&c| c == '(').count();
                let closes = s.chars().filter(|&c| c == ')').count();
                closes > opens
            }
            ']' => {
                let opens = s.chars().filter(|&c| c == '[').count();
                let closes = s.chars().filter(|&c| c == ']').count();
                closes > opens
            }
            _ => false,
        };
        if strip {
            s.pop();
        } else {
            break;
        }
    }
    s
}

fn dedup_links(links: Vec<ExtractedLink>) -> Vec<ExtractedLink> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(links.len());
    for link in links {
        if !is_http_url(&link.url) {
            continue;
        }
        if seen.insert(link.url.clone()) {
            out.push(link);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AttachmentItem, InlineStyle, IssueComment, IssueSummary, LinkedConfluencePage, PageSummary};

    #[test]
    fn extracts_markdown_bare_and_balanced_paren_urls() {
        let text = "Check [Design Doc](https://acme.atlassian.net/wiki/1) and https://en.wikipedia.org/wiki/Rust_(programming_language). Also (https://example.com/path?a=1).";
        let links = extract_links_from_text(text, "Description");
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].label, "Design Doc");
        assert_eq!(links[0].url, "https://acme.atlassian.net/wiki/1");
        assert_eq!(
            links[1].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert_eq!(links[2].url, "https://example.com/path?a=1");
    }

    #[test]
    fn extracts_and_deduplicates_from_issue_and_blocks() {
        let issue = IssueDetail {
            summary: IssueSummary {
                key: "ENG-9".into(),
                summary: "Fix pipeline".into(),
                web_url: "https://acme.atlassian.net/browse/ENG-9".into(),
                ..Default::default()
            },
            description_blocks: vec![DocBlock::Paragraph {
                spans: vec![InlineNode::Text {
                    text: "Runbook".into(),
                    style: InlineStyle {
                        link_url: Some("https://docs.acme.dev/runbook".into()),
                        ..Default::default()
                    },
                }],
            }],
            description_markdown: "See [Runbook](https://docs.acme.dev/runbook) and https://status.acme.dev".into(),
            linked_pages: vec![LinkedConfluencePage {
                page_id: "100".into(),
                title: "Architecture".into(),
                space_key: Some("ENG".into()),
                web_url: "https://acme.atlassian.net/wiki/pages/100".into(),
            }],
            attachments: vec![AttachmentItem {
                id: "1".into(),
                filename: "diag.png".into(),
                mime_type: "image/png".into(),
                size: 512,
                author_name: None,
                created_at: String::new(),
                content_url: "https://acme.atlassian.net/att/1".into(),
                thumbnail_url: None,
            }],
            comments: vec![IssueComment {
                id: "c1".into(),
                body_markdown: "Dashboard: https://metrics.acme.dev/d/1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let links = extract_links_from_issue(&issue);
        let urls: Vec<&str> = links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://acme.atlassian.net/browse/ENG-9",
                "https://docs.acme.dev/runbook",
                "https://status.acme.dev",
                "https://acme.atlassian.net/wiki/pages/100",
                "https://acme.atlassian.net/att/1",
                "https://metrics.acme.dev/d/1",
            ]
        );
        assert_eq!(links[1].label, "Runbook");
    }

    #[test]
    fn overlay_handles_navigation_copy_open_and_close() {
        let mut ov = LinksOverlay::new(
            "ENG-9",
            vec![
                ExtractedLink {
                    label: "First".into(),
                    url: "https://one.example.com".into(),
                    source: "Issue".into(),
                },
                ExtractedLink {
                    label: "Second".into(),
                    url: "https://two.example.com".into(),
                    source: "Comment".into(),
                },
            ],
        );

        ov.handle(&Action::MoveDown);
        assert_eq!(ov.selected, 1);

        // `y` maps to ConfirmYes in non-text overlay mode
        assert_eq!(ov.handle(&Action::ConfirmYes), OverlayOutcome::Handled);
        assert_eq!(
            ov.take_pending_command(),
            Some(LinkCommand::Copy("https://two.example.com".into()))
        );

        // Enter opens in browser and closes overlay
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Close);
        assert_eq!(
            ov.take_pending_command(),
            Some(LinkCommand::Open("https://two.example.com".into()))
        );

        // Esc closes cleanly
        assert_eq!(ov.handle(&Action::CloseOverlay), OverlayOutcome::Close);

        // Page extraction works too
        let page = PageDetail {
            summary: PageSummary {
                id: "50".into(),
                title: "Spec".into(),
                web_url: "https://acme.atlassian.net/wiki/pages/50".into(),
                ..Default::default()
            },
            body_markdown: "Ref: https://example.org/spec".into(),
            ..Default::default()
        };
        let page_links = extract_links_from_page(&page);
        assert_eq!(page_links.len(), 2);
    }
}
