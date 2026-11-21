//! Unified Rich Document Engine: ADF JSON ⇄ `DocBlock` AST ⇄ Markdown, plus Confluence Storage XHTML & Jira Wiki Markup parsers.

use std::{collections::HashSet, sync::OnceLock};

use chrono::DateTime;
use regex::Regex;
use scraper::{ElementRef, Html, Node};
use serde_json::{json, Value};

use crate::models::{
    CrossRefTarget, DocBlock, InlineNode, InlineStyle, PanelKind, StatusCategory, TaskItem,
    TocEntry,
};

fn issue_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Z][A-Z0-9_]{1,15}-\d+").expect("valid issue key regex"))
}

fn browse_issue_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"/browse/([A-Z][A-Z0-9_]{1,15}-\d+)").expect("valid browse regex")
    })
}

fn confluence_page_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:/pages/(\d+)|(?:[?&]|\b)pageId=(\d+))").expect("valid page id regex")
    })
}

fn is_false_positive_key(key: &str) -> bool {
    matches!(
        key,
        "UTF-8" | "UTF-16" | "UTF-32" | "SHA-1" | "SHA-256" | "SHA-512" | "MD-5" | "ISO-8601"
    )
}

fn is_boundary_before(s: &str, start: usize) -> bool {
    if start == 0 {
        return true;
    }
    s[..start]
        .chars()
        .next_back()
        .map(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .unwrap_or(true)
}

fn is_boundary_after(s: &str, end: usize) -> bool {
    if end >= s.len() {
        return true;
    }
    s[end..]
        .chars()
        .next()
        .map(|c| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(true)
}

fn is_inside_url_at(s: &str, start: usize) -> bool {
    let prefix = &s[..start];
    if let Some(last_ws) = prefix.rfind(char::is_whitespace) {
        let token = &prefix[last_ws..];
        token.contains("http://") || token.contains("https://")
    } else {
        prefix.contains("http://") || prefix.contains("https://")
    }
}

fn extract_issue_key_from_url(url: &str) -> Option<String> {
    browse_issue_regex()
        .captures(url)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn extract_page_id_from_url(url: &str) -> Option<String> {
    confluence_page_regex()
        .captures(url)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().to_string())
}

fn split_text_with_issue_refs(text: &str, style: &InlineStyle) -> Vec<InlineNode> {
    if text.is_empty() {
        return Vec::new();
    }
    if style.code || style.link_url.is_some() {
        return vec![InlineNode::Text {
            text: text.to_string(),
            style: style.clone(),
        }];
    }

    let mut out = Vec::new();
    let mut cursor = 0usize;
    for m in issue_key_regex().find_iter(text) {
        let key = m.as_str();
        if is_false_positive_key(key)
            || !is_boundary_before(text, m.start())
            || !is_boundary_after(text, m.end())
            || is_inside_url_at(text, m.start())
        {
            continue;
        }
        if m.start() > cursor {
            out.push(InlineNode::Text {
                text: text[cursor..m.start()].to_string(),
                style: style.clone(),
            });
        }
        out.push(InlineNode::IssueRef {
            key: key.to_string(),
        });
        cursor = m.end();
    }

    if cursor < text.len() {
        out.push(InlineNode::Text {
            text: text[cursor..].to_string(),
            style: style.clone(),
        });
    }
    if out.is_empty() {
        out.push(InlineNode::Text {
            text: text.to_string(),
            style: style.clone(),
        });
    }
    out
}

// ============================================================================
// 1. Atlassian Document Format (ADF v1) Parser
// ============================================================================

/// Parses an Atlassian Document Format (`{"type":"doc","version":1,"content":[...]}`) JSON value into `DocBlock`s.
pub fn parse_adf_value(val: &Value) -> Vec<DocBlock> {
    if val.is_null() {
        return Vec::new();
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        return vec![DocBlock::Paragraph {
            spans: split_text_with_issue_refs(s, &InlineStyle::default()),
        }];
    }
    if let Some(arr) = val.as_array() {
        return parse_adf_block_slice(arr);
    }
    if let Some(obj) = val.as_object() {
        if obj.get("type").and_then(|v| v.as_str()) == Some("doc") {
            if let Some(arr) = obj.get("content").and_then(|v| v.as_array()) {
                return parse_adf_block_slice(arr);
            }
            return Vec::new();
        }
        return parse_adf_block_node(val);
    }
    Vec::new()
}

/// Parses an ADF JSON string into `DocBlock`s.
pub fn parse_adf_str(raw: &str) -> Vec<DocBlock> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => parse_adf_value(&v),
        Err(_) => parse_jira_wiki(raw),
    }
}

fn parse_adf_block_slice(nodes: &[Value]) -> Vec<DocBlock> {
    let mut blocks = Vec::new();
    for node in nodes {
        blocks.extend(parse_adf_block_node(node));
    }
    blocks
}

fn parse_adf_block_node(node: &Value) -> Vec<DocBlock> {
    let node_type = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let attrs = node.get("attrs");
    let content = node
        .get("content")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    match node_type {
        "paragraph" => vec![DocBlock::Paragraph {
            spans: parse_adf_inlines(content),
        }],
        "heading" => {
            let level = attrs
                .and_then(|a| a.get("level"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .clamp(1, 6) as u8;
            vec![DocBlock::Heading {
                level,
                spans: parse_adf_inlines(content),
            }]
        }
        "bulletList" => {
            let items = content
                .iter()
                .filter_map(|item| {
                    let sub = item
                        .get("content")
                        .and_then(|v| v.as_array())
                        .map(|arr| parse_adf_block_slice(arr))?;
                    Some(sub)
                })
                .collect();
            vec![DocBlock::BulletList { items }]
        }
        "orderedList" => {
            let start = attrs
                .and_then(|a| a.get("order"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .max(1) as usize;
            let items = content
                .iter()
                .filter_map(|item| {
                    let sub = item
                        .get("content")
                        .and_then(|v| v.as_array())
                        .map(|arr| parse_adf_block_slice(arr))?;
                    Some(sub)
                })
                .collect();
            vec![DocBlock::OrderedList { start, items }]
        }
        "taskList" => {
            let items = collect_adf_task_items(content);
            vec![DocBlock::TaskList { items }]
        }
        "decisionList" => {
            let items = content
                .iter()
                .filter_map(|item| {
                    let spans = item
                        .get("content")
                        .and_then(|v| v.as_array())
                        .map(|arr| parse_adf_inlines(arr))?;
                    Some(vec![DocBlock::Paragraph { spans }])
                })
                .collect();
            vec![DocBlock::BulletList { items }]
        }
        "codeBlock" => {
            let language = attrs
                .and_then(|a| a.get("language"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let mut code = String::new();
            for child in content {
                match child.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = child.get("text").and_then(|v| v.as_str()) {
                            code.push_str(t);
                        }
                    }
                    "hardBreak" => code.push('\n'),
                    _ => {}
                }
            }
            vec![DocBlock::CodeBlock { language, code }]
        }
        "blockquote" => vec![DocBlock::Blockquote {
            blocks: parse_adf_block_slice(content),
        }],
        "panel" => {
            let kind = match attrs
                .and_then(|a| a.get("panelType"))
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_ascii_lowercase()
                .as_str()
            {
                "note" => PanelKind::Note,
                "warning" => PanelKind::Warning,
                "success" | "tip" => PanelKind::Success,
                "error" => PanelKind::Error,
                _ => PanelKind::Info,
            };
            let title = attrs
                .and_then(|a| a.get("title"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            vec![DocBlock::Panel {
                kind,
                title,
                blocks: parse_adf_block_slice(content),
            }]
        }
        "expand" | "nestedExpand" => {
            let title = attrs
                .and_then(|a| a.get("title"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Details")
                .to_string();
            vec![DocBlock::Expand {
                title,
                blocks: parse_adf_block_slice(content),
            }]
        }
        "table" => {
            let mut headers: Vec<Vec<InlineNode>> = Vec::new();
            let mut rows: Vec<Vec<Vec<InlineNode>>> = Vec::new();
            for (row_idx, row_node) in content.iter().enumerate() {
                if row_node.get("type").and_then(|v| v.as_str()) != Some("tableRow") {
                    continue;
                }
                let cells = row_node
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let has_header_cells = cells
                    .iter()
                    .any(|c| c.get("type").and_then(|v| v.as_str()) == Some("tableHeader"));
                let parsed_row: Vec<Vec<InlineNode>> = cells
                    .iter()
                    .map(|cell| {
                        let cell_blocks = cell
                            .get("content")
                            .and_then(|v| v.as_array())
                            .map(|arr| parse_adf_block_slice(arr))
                            .unwrap_or_default();
                        flatten_blocks_to_inlines(&cell_blocks)
                    })
                    .collect();
                if row_idx == 0 && has_header_cells {
                    headers = parsed_row;
                } else {
                    rows.push(parsed_row);
                }
            }
            vec![DocBlock::Table { headers, rows }]
        }
        "mediaSingle" | "mediaGroup" => {
            let mut caption: Option<String> = None;
            let mut media_blocks = Vec::new();
            for child in content {
                let ctype = child.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ctype == "caption" {
                    let cap_inlines = child
                        .get("content")
                        .and_then(|v| v.as_array())
                        .map(|arr| parse_adf_inlines(arr))
                        .unwrap_or_default();
                    let cap_txt = inlines_to_plain_text(&cap_inlines).trim().to_string();
                    if !cap_txt.is_empty() {
                        caption = Some(cap_txt);
                    }
                } else if ctype == "media" {
                    media_blocks.extend(parse_adf_block_node(child));
                }
            }
            if let Some(cap) = caption {
                for b in &mut media_blocks {
                    if let DocBlock::Media { alt, .. } = b {
                        if alt.is_none() {
                            *alt = Some(cap.clone());
                        }
                    }
                }
            }
            media_blocks
        }
        "media" => {
            let id_or_filename = attrs
                .and_then(|a| {
                    a.get("__fileName")
                        .or_else(|| a.get("id"))
                        .or_else(|| a.get("url"))
                })
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("media")
                .to_string();
            let alt = attrs
                .and_then(|a| a.get("alt"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let url = attrs
                .and_then(|a| a.get("url"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            vec![DocBlock::Media {
                id_or_filename,
                alt,
                url,
            }]
        }
        "rule" => vec![DocBlock::Rule],
        "doc" => parse_adf_block_slice(content),
        _ => {
            if !content.is_empty() {
                parse_adf_block_slice(content)
            } else {
                Vec::new()
            }
        }
    }
}

fn collect_adf_task_items(nodes: &[Value]) -> Vec<TaskItem> {
    let mut items = Vec::new();
    for node in nodes {
        match node.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "taskItem" => {
                let checked = node
                    .get("attrs")
                    .and_then(|a| a.get("state"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.eq_ignore_ascii_case("DONE"))
                    .unwrap_or(false);
                let inner = node
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let content = parse_adf_inlines(inner);
                items.push(TaskItem { checked, content });
            }
            "taskList" => {
                let inner = node
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                items.extend(collect_adf_task_items(inner));
            }
            _ => {}
        }
    }
    items
}

fn parse_adf_inlines(nodes: &[Value]) -> Vec<InlineNode> {
    let mut out = Vec::new();
    for node in nodes {
        let ntype = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let attrs = node.get("attrs");
        match ntype {
            "text" => {
                let text = node.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let mut style = InlineStyle::default();
                if let Some(marks) = node.get("marks").and_then(|v| v.as_array()) {
                    for mark in marks {
                        match mark.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                            "strong" => style.bold = true,
                            "em" => style.italic = true,
                            "strike" => style.strike = true,
                            "underline" => style.underline = true,
                            "code" => style.code = true,
                            "link" => {
                                if let Some(href) = mark
                                    .get("attrs")
                                    .and_then(|a| a.get("href"))
                                    .and_then(|v| v.as_str())
                                {
                                    style.link_url = Some(href.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(ref href) = style.link_url {
                    if !style.code {
                        if let Some(key) = extract_issue_key_from_url(href) {
                            let trimmed = text.trim();
                            if trimmed == key || trimmed == href || trimmed.is_empty() {
                                out.push(InlineNode::IssueRef { key });
                                continue;
                            }
                        }
                        if let Some(page_id) = extract_page_id_from_url(href) {
                            out.push(InlineNode::PageRef {
                                page_id,
                                title: text.to_string(),
                            });
                            continue;
                        }
                    }
                }
                out.extend(split_text_with_issue_refs(text, &style));
            }
            "mention" => {
                let id = attrs
                    .and_then(|a| a.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let raw_text = attrs
                    .and_then(|a| a.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let display = raw_text.trim_start_matches('@').to_string();
                let display = if display.is_empty() {
                    id.clone()
                } else {
                    display
                };
                out.push(InlineNode::Mention { id, display });
            }
            "status" => {
                let text = attrs
                    .and_then(|a| a.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let color = attrs
                    .and_then(|a| a.get("color"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("neutral")
                    .to_ascii_lowercase();
                let category = match color.as_str() {
                    "green" => StatusCategory::Done,
                    "blue" | "purple" => StatusCategory::InProgress,
                    _ => StatusCategory::Todo,
                };
                out.push(InlineNode::StatusLozenge { text, category });
            }
            "inlineCard" | "blockCard" => {
                let url = attrs
                    .and_then(|a| a.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(key) = extract_issue_key_from_url(&url) {
                    out.push(InlineNode::IssueRef { key });
                } else if let Some(page_id) = extract_page_id_from_url(&url) {
                    out.push(InlineNode::PageRef {
                        page_id,
                        title: url.clone(),
                    });
                } else if !url.is_empty() {
                    out.push(InlineNode::Text {
                        text: url.clone(),
                        style: InlineStyle {
                            link_url: Some(url),
                            ..Default::default()
                        },
                    });
                }
            }
            "emoji" => {
                let text = attrs
                    .and_then(|a| a.get("text").or_else(|| a.get("shortName")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    out.push(InlineNode::Emoji { text });
                }
            }
            "hardBreak" => out.push(InlineNode::HardBreak),
            "date" => {
                if let Some(ts_str) = attrs.and_then(|a| a.get("timestamp")).and_then(|v| v.as_str())
                {
                    let formatted = ts_str
                        .parse::<i64>()
                        .ok()
                        .and_then(|ms| {
                            let secs = if ms.abs() > 10_000_000_000 {
                                ms / 1000
                            } else {
                                ms
                            };
                            DateTime::from_timestamp(secs, 0)
                        })
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| ts_str.to_string());
                    out.push(InlineNode::Text {
                        text: formatted,
                        style: InlineStyle::default(),
                    });
                }
            }
            _ => {
                if let Some(arr) = node.get("content").and_then(|v| v.as_array()) {
                    out.extend(parse_adf_inlines(arr));
                }
            }
        }
    }
    out
}

fn flatten_blocks_to_inlines(blocks: &[DocBlock]) -> Vec<InlineNode> {
    let mut out = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 && !out.is_empty() {
            out.push(InlineNode::HardBreak);
        }
        match block {
            DocBlock::Paragraph { spans } | DocBlock::Heading { spans, .. } => {
                out.extend(spans.clone());
            }
            DocBlock::Blockquote { blocks: inner }
            | DocBlock::Panel { blocks: inner, .. }
            | DocBlock::Expand { blocks: inner, .. } => {
                out.extend(flatten_blocks_to_inlines(inner));
            }
            DocBlock::CodeBlock { code, .. } => {
                out.push(InlineNode::Text {
                    text: code.clone(),
                    style: InlineStyle {
                        code: true,
                        ..Default::default()
                    },
                });
            }
            DocBlock::BulletList { items } | DocBlock::OrderedList { items, .. } => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(InlineNode::HardBreak);
                    }
                    out.extend(flatten_blocks_to_inlines(item));
                }
            }
            DocBlock::TaskList { items } => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(InlineNode::HardBreak);
                    }
                    out.extend(item.content.clone());
                }
            }
            _ => {}
        }
    }
    out
}

// ============================================================================
// 2. Confluence Storage XHTML Parser (`scraper`)
// ============================================================================

fn cdata_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").expect("valid cdata regex"))
}

fn self_closing_ac_ri_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<((?:ac|ri):[a-zA-Z0-9_-]+)([^>]*?)\s*/>")
            .expect("valid self-closing xml regex")
    })
}

fn preprocess_storage_xhtml(xhtml: &str) -> String {
    let escaped_cdata = cdata_regex().replace_all(xhtml, |caps: &regex::Captures<'_>| {
        let inner = &caps[1];
        inner
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    });
    self_closing_ac_ri_regex()
        .replace_all(&escaped_cdata, "<$1$2></$1>")
        .into_owned()
}

/// Parses Confluence Storage XHTML (`<ac:structured-macro>`, tables, headings, status macros, etc.) into `DocBlock`s.
pub fn parse_storage_xhtml(xhtml: &str) -> Vec<DocBlock> {
    if xhtml.trim().is_empty() {
        return Vec::new();
    }
    let normalized = preprocess_storage_xhtml(xhtml);
    let fragment = Html::parse_fragment(&normalized);
    let root = fragment.root_element();
    parse_xhtml_children(root)
}

fn parse_xhtml_children(parent: ElementRef<'_>) -> Vec<DocBlock> {
    let mut blocks = Vec::new();
    let mut pending_inlines: Vec<InlineNode> = Vec::new();

    let flush_inlines = |pending: &mut Vec<InlineNode>, out: &mut Vec<DocBlock>| {
        if !pending.is_empty() {
            let has_content = pending.iter().any(|n| match n {
                InlineNode::Text { text, .. } => !text.trim().is_empty(),
                InlineNode::HardBreak => false,
                _ => true,
            });
            if has_content {
                out.push(DocBlock::Paragraph {
                    spans: trim_edge_whitespace_inlines(std::mem::take(pending)),
                });
            } else {
                pending.clear();
            }
        }
    };

    for child in parent.children() {
        match child.value() {
            Node::Text(t) => {
                if !t.is_empty() {
                    let collapsed = collapse_html_whitespace(t);
                    if !collapsed.is_empty() {
                        pending_inlines.extend(split_text_with_issue_refs(
                            &collapsed,
                            &InlineStyle::default(),
                        ));
                    }
                }
            }
            Node::Element(el) => {
                let tag = el.name().to_ascii_lowercase();
                if let Some(el_ref) = ElementRef::wrap(child) {
                    if is_inline_xhtml_element(&tag, el_ref) {
                        parse_xhtml_inline_element(el_ref, &InlineStyle::default(), &mut pending_inlines);
                    } else {
                        flush_inlines(&mut pending_inlines, &mut blocks);
                        blocks.extend(parse_xhtml_block_element(el_ref));
                    }
                }
            }
            _ => {}
        }
    }
    flush_inlines(&mut pending_inlines, &mut blocks);
    blocks
}

fn is_inline_xhtml_element(tag: &str, el: ElementRef<'_>) -> bool {
    match tag {
        "a" | "strong" | "b" | "em" | "i" | "del" | "s" | "u" | "code" | "span" | "br"
        | "sub" | "sup" | "ac:link" | "time" => true,
        "ac:structured-macro" => {
            let name = el
                .value()
                .attr("ac:name")
                .unwrap_or("")
                .to_ascii_lowercase();
            matches!(name.as_str(), "status" | "jira")
        }
        _ => false,
    }
}

fn parse_xhtml_block_element(el: ElementRef<'_>) -> Vec<DocBlock> {
    let tag = el.value().name().to_ascii_lowercase();
    match tag.as_str() {
        "p" => {
            let mut spans = Vec::new();
            parse_xhtml_inlines_of(el, &InlineStyle::default(), &mut spans);
            let spans = trim_edge_whitespace_inlines(spans);
            if spans.is_empty() {
                Vec::new()
            } else {
                vec![DocBlock::Paragraph { spans }]
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag[1..].parse::<u8>().unwrap_or(1).clamp(1, 6);
            let mut spans = Vec::new();
            parse_xhtml_inlines_of(el, &InlineStyle::default(), &mut spans);
            vec![DocBlock::Heading {
                level,
                spans: trim_edge_whitespace_inlines(spans),
            }]
        }
        "ul" => {
            let items = el
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|c| c.value().name().eq_ignore_ascii_case("li"))
                .map(parse_xhtml_children)
                .collect();
            vec![DocBlock::BulletList { items }]
        }
        "ol" => {
            let start = el
                .value()
                .attr("start")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            let items = el
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|c| c.value().name().eq_ignore_ascii_case("li"))
                .map(parse_xhtml_children)
                .collect();
            vec![DocBlock::OrderedList { start, items }]
        }
        "pre" => {
            let code_el = el
                .children()
                .filter_map(ElementRef::wrap)
                .find(|c| c.value().name().eq_ignore_ascii_case("code"));
            let language = code_el
                .and_then(|c| c.value().attr("class"))
                .and_then(|cls| {
                    cls.split_whitespace()
                        .find_map(|tok| tok.strip_prefix("language-"))
                })
                .map(str::to_string);
            let code = collect_raw_text(code_el.unwrap_or(el));
            vec![DocBlock::CodeBlock { language, code }]
        }
        "blockquote" => vec![DocBlock::Blockquote {
            blocks: parse_xhtml_children(el),
        }],
        "hr" => vec![DocBlock::Rule],
        "table" => vec![parse_xhtml_table(el)],
        "img" => {
            let src = el.value().attr("src").map(str::to_string);
            let alt = el
                .value()
                .attr("alt")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let id_or_filename = src.clone().unwrap_or_else(|| "image".to_string());
            vec![DocBlock::Media {
                id_or_filename,
                alt,
                url: src,
            }]
        }
        "ac:image" => {
            let alt = el
                .value()
                .attr("ac:alt")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let mut filename: Option<String> = None;
            let mut url: Option<String> = None;
            for child in el.descendants().filter_map(ElementRef::wrap) {
                let cname = child.value().name().to_ascii_lowercase();
                if cname == "ri:attachment" {
                    if let Some(f) = child.value().attr("ri:filename") {
                        filename = Some(f.to_string());
                    }
                } else if cname == "ri:url" {
                    if let Some(u) = child.value().attr("ri:value") {
                        url = Some(u.to_string());
                    }
                }
            }
            let id_or_filename = filename
                .or_else(|| url.clone())
                .unwrap_or_else(|| "attachment".to_string());
            vec![DocBlock::Media {
                id_or_filename,
                alt,
                url,
            }]
        }
        "ac:task-list" => {
            let mut items = Vec::new();
            for child in el.children().filter_map(ElementRef::wrap) {
                if child.value().name().eq_ignore_ascii_case("ac:task") {
                    let mut checked = false;
                    let mut content = Vec::new();
                    for sub in child.children().filter_map(ElementRef::wrap) {
                        let sname = sub.value().name().to_ascii_lowercase();
                        if sname == "ac:task-status" {
                            let status = collect_raw_text(sub).trim().to_ascii_lowercase();
                            checked = matches!(status.as_str(), "complete" | "completed" | "done");
                        } else if sname == "ac:task-body" {
                            parse_xhtml_inlines_of(sub, &InlineStyle::default(), &mut content);
                        }
                    }
                    items.push(TaskItem {
                        checked,
                        content: trim_edge_whitespace_inlines(content),
                    });
                }
            }
            vec![DocBlock::TaskList { items }]
        }
        "ac:structured-macro" => parse_xhtml_macro(el),
        _ => parse_xhtml_children(el),
    }
}

fn parse_xhtml_macro(el: ElementRef<'_>) -> Vec<DocBlock> {
    let macro_name = el
        .value()
        .attr("ac:name")
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut params = std::collections::HashMap::<String, String>::new();
    let mut plain_body: Option<String> = None;
    let mut rich_body_el: Option<ElementRef<'_>> = None;

    for child in el.children().filter_map(ElementRef::wrap) {
        let cname = child.value().name().to_ascii_lowercase();
        if cname == "ac:parameter" {
            if let Some(pname) = child.value().attr("ac:name") {
                params.insert(pname.to_ascii_lowercase(), collect_raw_text(child).trim().to_string());
            }
        } else if cname == "ac:plain-text-body" {
            plain_body = Some(collect_raw_text(child));
        } else if cname == "ac:rich-text-body" {
            rich_body_el = Some(child);
        }
    }

    match macro_name.as_str() {
        "info" | "note" | "warning" | "tip" | "panel" => {
            let kind = match macro_name.as_str() {
                "note" => PanelKind::Note,
                "warning" => PanelKind::Warning,
                "tip" => PanelKind::Success,
                _ => PanelKind::Info,
            };
            let title = params
                .get("title")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let blocks = rich_body_el
                .map(parse_xhtml_children)
                .unwrap_or_else(|| {
                    plain_body
                        .map(|t| {
                            vec![DocBlock::Paragraph {
                                spans: split_text_with_issue_refs(&t, &InlineStyle::default()),
                            }]
                        })
                        .unwrap_or_default()
                });
            vec![DocBlock::Panel {
                kind,
                title,
                blocks,
            }]
        }
        "code" | "noformat" => {
            let language = params
                .get("language")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let code = plain_body
                .or_else(|| rich_body_el.map(collect_raw_text))
                .unwrap_or_default();
            vec![DocBlock::CodeBlock { language, code }]
        }
        "expand" => {
            let title = params
                .get("title")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Details".to_string());
            let blocks = rich_body_el.map(parse_xhtml_children).unwrap_or_default();
            vec![DocBlock::Expand { title, blocks }]
        }
        "status" => {
            let title = params.get("title").cloned().unwrap_or_default();
            let colour = params
                .get("colour")
                .or_else(|| params.get("color"))
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let category = match colour.as_str() {
                "green" => StatusCategory::Done,
                "blue" | "purple" => StatusCategory::InProgress,
                _ => StatusCategory::Todo,
            };
            vec![DocBlock::Paragraph {
                spans: vec![InlineNode::StatusLozenge {
                    text: title,
                    category,
                }],
            }]
        }
        "jira" => {
            if let Some(key) = params.get("key").filter(|k| !k.trim().is_empty()) {
                vec![DocBlock::Paragraph {
                    spans: vec![InlineNode::IssueRef {
                        key: key.trim().to_string(),
                    }],
                }]
            } else {
                Vec::new()
            }
        }
        _ => {
            if let Some(rich) = rich_body_el {
                parse_xhtml_children(rich)
            } else if let Some(plain) = plain_body {
                if plain.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![DocBlock::Paragraph {
                        spans: split_text_with_issue_refs(&plain, &InlineStyle::default()),
                    }]
                }
            } else {
                Vec::new()
            }
        }
    }
}

fn parse_xhtml_table(table_el: ElementRef<'_>) -> DocBlock {
    let mut headers: Vec<Vec<InlineNode>> = Vec::new();
    let mut rows: Vec<Vec<Vec<InlineNode>>> = Vec::new();
    let mut tr_elements: Vec<ElementRef<'_>> = Vec::new();

    collect_table_rows(table_el, &mut tr_elements);

    for (idx, tr) in tr_elements.into_iter().enumerate() {
        let mut row_cells: Vec<Vec<InlineNode>> = Vec::new();
        let mut all_th = true;
        let mut any_cell = false;

        for cell in tr.children().filter_map(ElementRef::wrap) {
            let cname = cell.value().name().to_ascii_lowercase();
            if cname == "th" || cname == "td" {
                any_cell = true;
                if cname != "th" {
                    all_th = false;
                }
                let cell_blocks = parse_xhtml_children(cell);
                row_cells.push(flatten_blocks_to_inlines(&cell_blocks));
            }
        }

        if !any_cell {
            continue;
        }
        if idx == 0 && all_th && headers.is_empty() {
            headers = row_cells;
        } else {
            rows.push(row_cells);
        }
    }

    DocBlock::Table { headers, rows }
}

fn collect_table_rows<'a>(el: ElementRef<'a>, out: &mut Vec<ElementRef<'a>>) {
    for child in el.children().filter_map(ElementRef::wrap) {
        let cname = child.value().name().to_ascii_lowercase();
        if cname == "tr" {
            out.push(child);
        } else if matches!(cname.as_str(), "thead" | "tbody" | "tfoot") {
            collect_table_rows(child, out);
        }
    }
}

fn parse_xhtml_inlines_of(el: ElementRef<'_>, style: &InlineStyle, out: &mut Vec<InlineNode>) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let collapsed = collapse_html_whitespace(t);
                if !collapsed.is_empty() {
                    out.extend(split_text_with_issue_refs(&collapsed, style));
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    parse_xhtml_inline_element(child_el, style, out);
                }
            }
            _ => {}
        }
    }
}

fn parse_xhtml_inline_element(
    el: ElementRef<'_>,
    parent_style: &InlineStyle,
    out: &mut Vec<InlineNode>,
) {
    let tag = el.value().name().to_ascii_lowercase();
    match tag.as_str() {
        "strong" | "b" => {
            let mut st = parent_style.clone();
            st.bold = true;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "em" | "i" => {
            let mut st = parent_style.clone();
            st.italic = true;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "del" | "s" => {
            let mut st = parent_style.clone();
            st.strike = true;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "u" => {
            let mut st = parent_style.clone();
            st.underline = true;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "code" => {
            let mut st = parent_style.clone();
            st.code = true;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "a" => {
            let href = el.value().attr("href").map(str::to_string);
            if let Some(ref url) = href {
                let label = collect_raw_text(el).trim().to_string();
                if let Some(key) = extract_issue_key_from_url(url) {
                    if label == key || label == *url || label.is_empty() {
                        out.push(InlineNode::IssueRef { key });
                        return;
                    }
                }
                if let Some(page_id) = extract_page_id_from_url(url) {
                    out.push(InlineNode::PageRef {
                        page_id,
                        title: if label.is_empty() { url.clone() } else { label },
                    });
                    return;
                }
            }
            let mut st = parent_style.clone();
            st.link_url = href;
            parse_xhtml_inlines_of(el, &st, out);
        }
        "br" => out.push(InlineNode::HardBreak),
        "ac:structured-macro" => {
            let macro_name = el
                .value()
                .attr("ac:name")
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut params = std::collections::HashMap::<String, String>::new();
            for child in el.children().filter_map(ElementRef::wrap) {
                if child.value().name().eq_ignore_ascii_case("ac:parameter") {
                    if let Some(pname) = child.value().attr("ac:name") {
                        params.insert(
                            pname.to_ascii_lowercase(),
                            collect_raw_text(child).trim().to_string(),
                        );
                    }
                }
            }
            if macro_name == "status" {
                let text = params.get("title").cloned().unwrap_or_default();
                let colour = params
                    .get("colour")
                    .or_else(|| params.get("color"))
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                let category = match colour.as_str() {
                    "green" => StatusCategory::Done,
                    "blue" | "purple" => StatusCategory::InProgress,
                    _ => StatusCategory::Todo,
                };
                out.push(InlineNode::StatusLozenge { text, category });
            } else if macro_name == "jira" {
                if let Some(key) = params.get("key").filter(|k| !k.trim().is_empty()) {
                    out.push(InlineNode::IssueRef {
                        key: key.trim().to_string(),
                    });
                }
            } else {
                parse_xhtml_inlines_of(el, parent_style, out);
            }
        }
        "ac:link" => {
            let mut user_key: Option<String> = None;
            let mut page_title: Option<String> = None;
            let mut link_body: Option<String> = None;
            for child in el.descendants().filter_map(ElementRef::wrap) {
                let cname = child.value().name().to_ascii_lowercase();
                if cname == "ri:user" {
                    user_key = child
                        .value()
                        .attr("ri:account-id")
                        .or_else(|| child.value().attr("ri:userkey"))
                        .or_else(|| child.value().attr("ri:username"))
                        .map(str::to_string);
                } else if cname == "ri:page" {
                    page_title = child
                        .value()
                        .attr("ri:content-title")
                        .map(str::to_string);
                } else if cname == "ac:plain-text-link-body" || cname == "ac:link-body" {
                    link_body = Some(collect_raw_text(child).trim().to_string());
                }
            }
            if let Some(uid) = user_key {
                let display = link_body.filter(|s| !s.is_empty()).unwrap_or_else(|| uid.clone());
                out.push(InlineNode::Mention { id: uid, display });
            } else if let Some(title) = page_title {
                let text = link_body.filter(|s| !s.is_empty()).unwrap_or(title);
                out.push(InlineNode::Text {
                    text,
                    style: parent_style.clone(),
                });
            } else {
                parse_xhtml_inlines_of(el, parent_style, out);
            }
        }
        "time" => {
            if let Some(dt) = el.value().attr("datetime") {
                out.push(InlineNode::Text {
                    text: dt.to_string(),
                    style: parent_style.clone(),
                });
            }
        }
        _ => parse_xhtml_inlines_of(el, parent_style, out),
    }
}

fn collect_raw_text(el: ElementRef<'_>) -> String {
    let mut s = String::new();
    for node in el.descendants() {
        match node.value() {
            Node::Text(t) => s.push_str(t),
            Node::Comment(c) => {
                let trimmed = c.trim();
                if let Some(inner) = trimmed
                    .strip_prefix("[CDATA[")
                    .and_then(|rest| rest.strip_suffix("]]"))
                {
                    s.push_str(inner);
                }
            }
            Node::Element(e) if e.name().eq_ignore_ascii_case("br") => s.push('\n'),
            _ => {}
        }
    }
    s
}

fn collapse_html_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_ws = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn trim_edge_whitespace_inlines(mut spans: Vec<InlineNode>) -> Vec<InlineNode> {
    if let Some(InlineNode::Text { text, .. }) = spans.first_mut() {
        let trimmed = text.trim_start().to_string();
        *text = trimmed;
    }
    if let Some(InlineNode::Text { text, .. }) = spans.last_mut() {
        let trimmed = text.trim_end().to_string();
        *text = trimmed;
    }
    spans.retain(|n| match n {
        InlineNode::Text { text, .. } => !text.is_empty(),
        _ => true,
    });
    spans
}

// ============================================================================
// 3. Jira Wiki Markup Parser
// ============================================================================

/// Parses Jira Data Center/Server Wiki Markup (`h1.`, `{code}`, `{panel}`, `||hdr||`, `[PROJ-123]`) into `DocBlock`s.
pub fn parse_jira_wiki(markup: &str) -> Vec<DocBlock> {
    if markup.trim().is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = markup.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Horizontal rule
        if trimmed == "----" || trimmed == "---" {
            blocks.push(DocBlock::Rule);
            i += 1;
            continue;
        }

        // Headings: h1. .. h6.
        if let Some((level, rest)) = parse_wiki_heading(trimmed) {
            blocks.push(DocBlock::Heading {
                level,
                spans: parse_wiki_inlines(rest),
            });
            i += 1;
            continue;
        }

        // {code...} / {noformat}
        if trimmed.starts_with("{code") || trimmed.starts_with("{noformat") {
            let is_noformat = trimmed.starts_with("{noformat");
            let end_tag = if is_noformat { "{noformat}" } else { "{code}" };
            if let Some(close_brace) = trimmed.find('}') {
                let header = &trimmed[1..close_brace];
                let language = if is_noformat {
                    None
                } else {
                    extract_wiki_code_lang(header)
                };
                let after_open = &trimmed[close_brace + 1..];
                if let Some(end_pos) = after_open.find(end_tag) {
                    blocks.push(DocBlock::CodeBlock {
                        language,
                        code: after_open[..end_pos].trim().to_string(),
                    });
                    i += 1;
                    continue;
                }
                let mut code_lines = Vec::new();
                if !after_open.is_empty() {
                    code_lines.push(after_open.to_string());
                }
                i += 1;
                while i < lines.len() {
                    let cur = lines[i];
                    if let Some(pos) = cur.find(end_tag) {
                        let tail = cur[..pos].trim_end();
                        if !tail.is_empty() {
                            code_lines.push(tail.to_string());
                        }
                        i += 1;
                        break;
                    }
                    code_lines.push(cur.to_string());
                    i += 1;
                }
                blocks.push(DocBlock::CodeBlock {
                    language,
                    code: code_lines.join("\n"),
                });
                continue;
            }
        }

        // {panel...} / {info} / {note} / {warning} / {tip}
        if let Some((kind, title, open_tag, after_open)) = parse_wiki_panel_open(trimmed) {
            let end_tag = format!("{{{open_tag}}}");
            if let Some(pos) = after_open.find(&end_tag) {
                let inner = &after_open[..pos];
                blocks.push(DocBlock::Panel {
                    kind,
                    title,
                    blocks: parse_jira_wiki(inner),
                });
                i += 1;
                continue;
            }
            let mut inner_lines = Vec::new();
            if !after_open.trim().is_empty() {
                inner_lines.push(after_open.to_string());
            }
            i += 1;
            while i < lines.len() {
                let cur = lines[i];
                if let Some(pos) = cur.find(&end_tag) {
                    let tail = cur[..pos].trim_end();
                    if !tail.is_empty() {
                        inner_lines.push(tail.to_string());
                    }
                    i += 1;
                    break;
                }
                inner_lines.push(cur.to_string());
                i += 1;
            }
            blocks.push(DocBlock::Panel {
                kind,
                title,
                blocks: parse_jira_wiki(&inner_lines.join("\n")),
            });
            continue;
        }

        // {quote}...{quote}
        if trimmed.starts_with("{quote}") {
            let after_open = &trimmed["{quote}".len()..];
            if let Some(pos) = after_open.find("{quote}") {
                blocks.push(DocBlock::Blockquote {
                    blocks: parse_jira_wiki(&after_open[..pos]),
                });
                i += 1;
                continue;
            }
            let mut inner_lines = Vec::new();
            if !after_open.trim().is_empty() {
                inner_lines.push(after_open.to_string());
            }
            i += 1;
            while i < lines.len() {
                let cur = lines[i];
                if let Some(pos) = cur.find("{quote}") {
                    let tail = cur[..pos].trim_end();
                    if !tail.is_empty() {
                        inner_lines.push(tail.to_string());
                    }
                    i += 1;
                    break;
                }
                inner_lines.push(cur.to_string());
                i += 1;
            }
            blocks.push(DocBlock::Blockquote {
                blocks: parse_jira_wiki(&inner_lines.join("\n")),
            });
            continue;
        }

        // bq. single line quote
        if let Some(rest) = trimmed.strip_prefix("bq. ") {
            blocks.push(DocBlock::Blockquote {
                blocks: vec![DocBlock::Paragraph {
                    spans: parse_wiki_inlines(rest),
                }],
            });
            i += 1;
            continue;
        }

        // Table: ||hdr|| or |cell|
        if trimmed.starts_with("||") || (trimmed.starts_with('|') && trimmed.ends_with('|')) {
            let mut headers: Vec<Vec<InlineNode>> = Vec::new();
            let mut rows: Vec<Vec<Vec<InlineNode>>> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with("||") {
                    let cells = split_wiki_table_cells(t, "||");
                    let parsed: Vec<Vec<InlineNode>> =
                        cells.into_iter().map(|c| parse_wiki_inlines(&c)).collect();
                    if headers.is_empty() && rows.is_empty() {
                        headers = parsed;
                    } else {
                        rows.push(parsed);
                    }
                    i += 1;
                } else if t.starts_with('|') {
                    let cells = split_wiki_table_cells(t, "|");
                    let parsed: Vec<Vec<InlineNode>> =
                        cells.into_iter().map(|c| parse_wiki_inlines(&c)).collect();
                    rows.push(parsed);
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(DocBlock::Table { headers, rows });
            continue;
        }

        // Bullet / Ordered list
        if is_wiki_bullet_line(trimmed) {
            let mut items = Vec::new();
            while i < lines.len() && is_wiki_bullet_line(lines[i].trim()) {
                let t = lines[i].trim();
                let rest = t.trim_start_matches('*').trim_start();
                items.push(vec![DocBlock::Paragraph {
                    spans: parse_wiki_inlines(rest),
                }]);
                i += 1;
            }
            blocks.push(DocBlock::BulletList { items });
            continue;
        }

        if is_wiki_ordered_line(trimmed) {
            let mut items = Vec::new();
            while i < lines.len() && is_wiki_ordered_line(lines[i].trim()) {
                let t = lines[i].trim();
                let rest = t.trim_start_matches('#').trim_start();
                items.push(vec![DocBlock::Paragraph {
                    spans: parse_wiki_inlines(rest),
                }]);
                i += 1;
            }
            blocks.push(DocBlock::OrderedList { start: 1, items });
            continue;
        }

        // Media line: !file.png! or !file.png|alt=foo!
        if trimmed.starts_with('!') && trimmed.ends_with('!') && trimmed.len() > 2 && !trimmed[1..trimmed.len() - 1].contains('\n') && !trimmed[1..trimmed.len() - 1].contains(' ') {
            let inner = &trimmed[1..trimmed.len() - 1];
            let filename = inner.split('|').next().unwrap_or(inner).trim().to_string();
            blocks.push(DocBlock::Media {
                id_or_filename: filename,
                alt: None,
                url: None,
            });
            i += 1;
            continue;
        }

        // Paragraph: collect contiguous normal lines
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty()
                || t == "----"
                || parse_wiki_heading(t).is_some()
                || t.starts_with("{code")
                || t.starts_with("{noformat")
                || t.starts_with("{quote}")
                || t.starts_with("bq. ")
                || parse_wiki_panel_open(t).is_some()
                || t.starts_with("||")
                || (t.starts_with('|') && t.ends_with('|'))
                || is_wiki_bullet_line(t)
                || is_wiki_ordered_line(t)
            {
                break;
            }
            para_lines.push(t);
            i += 1;
        }
        let joined = para_lines.join(" ");
        blocks.push(DocBlock::Paragraph {
            spans: parse_wiki_inlines(&joined),
        });
    }

    blocks
}

fn parse_wiki_heading(trimmed: &str) -> Option<(u8, &str)> {
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'h' && (b'1'..=b'6').contains(&bytes[1]) && bytes[2] == b'.' {
        let level = bytes[1] - b'0';
        let rest = trimmed[3..].trim();
        Some((level, rest))
    } else {
        None
    }
}

fn extract_wiki_code_lang(header: &str) -> Option<String> {
    let rest = header.strip_prefix("code")?.trim_start_matches(':').trim();
    if rest.is_empty() {
        return None;
    }
    if !rest.contains('=') {
        return Some(rest.to_string());
    }
    for part in rest.split('|') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case("language") || k.trim().eq_ignore_ascii_case("lang") {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        } else if !part.is_empty() {
            return Some(part.to_string());
        }
    }
    None
}

fn parse_wiki_panel_open(trimmed: &str) -> Option<(PanelKind, Option<String>, &'static str, &str)> {
    for (tag, kind) in [
        ("panel", PanelKind::Info),
        ("info", PanelKind::Info),
        ("note", PanelKind::Note),
        ("warning", PanelKind::Warning),
        ("tip", PanelKind::Success),
    ] {
        let prefix = format!("{{{tag}");
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            if rest.starts_with('}') || rest.starts_with(':') {
                if let Some(close_idx) = trimmed.find('}') {
                    let header = &trimmed[1..close_idx];
                    let title = header.split_once(':').and_then(|(_, params)| {
                        params.split('|').find_map(|p| {
                            let (k, v) = p.split_once('=')?;
                            if k.trim().eq_ignore_ascii_case("title") {
                                let t = v.trim();
                                if !t.is_empty() {
                                    return Some(t.to_string());
                                }
                            }
                            None
                        })
                    });
                    return Some((kind, title, tag, &trimmed[close_idx + 1..]));
                }
            }
        }
    }
    None
}

fn split_wiki_table_cells(line: &str, delim: &str) -> Vec<String> {
    let trimmed = line.trim();
    let stripped = trimmed
        .strip_prefix(delim)
        .unwrap_or(trimmed)
        .strip_suffix(delim)
        .unwrap_or(trimmed);
    stripped
        .split(delim)
        .map(|s| s.trim().to_string())
        .collect()
}

fn is_wiki_bullet_line(trimmed: &str) -> bool {
    let stars = trimmed.bytes().take_while(|&b| b == b'*').count();
    stars >= 1 && trimmed[stars..].starts_with(' ')
}

fn is_wiki_ordered_line(trimmed: &str) -> bool {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    hashes >= 1 && trimmed[hashes..].starts_with(' ')
}

fn parse_wiki_inlines(input: &str) -> Vec<InlineNode> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;

    let flush_buf = |buf: &mut String, out: &mut Vec<InlineNode>| {
        if !buf.is_empty() {
            let taken = std::mem::take(buf);
            out.extend(split_text_with_issue_refs(&taken, &InlineStyle::default()));
        }
    };

    while i < chars.len() {
        // {{monospace}}
        if i + 1 < chars.len() && chars[i] == '{' && chars[i + 1] == '{' {
            if let Some(end) = find_char_seq(&chars, i + 2, &['}', '}']) {
                flush_buf(&mut buf, &mut out);
                let code_text: String = chars[i + 2..end].iter().collect();
                out.push(InlineNode::Text {
                    text: code_text,
                    style: InlineStyle {
                        code: true,
                        ..Default::default()
                    },
                });
                i = end + 2;
                continue;
            }
        }

        // [~username] or [title|url] or [url]
        if chars[i] == '[' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == ']').map(|p| i + 1 + p) {
                let inner: String = chars[i + 1..end].iter().collect();
                if let Some(user) = inner.strip_prefix('~') {
                    flush_buf(&mut buf, &mut out);
                    let clean = user.strip_prefix("accountid:").unwrap_or(user).trim();
                    out.push(InlineNode::Mention {
                        id: clean.to_string(),
                        display: clean.to_string(),
                    });
                    i = end + 1;
                    continue;
                } else if let Some((title, url)) = inner.split_once('|') {
                    flush_buf(&mut buf, &mut out);
                    let url_t = url.trim();
                    let title_t = title.trim();
                    if let Some(key) = extract_issue_key_from_url(url_t) {
                        out.push(InlineNode::IssueRef { key });
                    } else if let Some(page_id) = extract_page_id_from_url(url_t) {
                        out.push(InlineNode::PageRef {
                            page_id,
                            title: title_t.to_string(),
                        });
                    } else {
                        out.push(InlineNode::Text {
                            text: title_t.to_string(),
                            style: InlineStyle {
                                link_url: Some(url_t.to_string()),
                                ..Default::default()
                            },
                        });
                    }
                    i = end + 1;
                    continue;
                } else if inner.starts_with("http://") || inner.starts_with("https://") {
                    flush_buf(&mut buf, &mut out);
                    let url_t = inner.trim();
                    if let Some(key) = extract_issue_key_from_url(url_t) {
                        out.push(InlineNode::IssueRef { key });
                    } else if let Some(page_id) = extract_page_id_from_url(url_t) {
                        out.push(InlineNode::PageRef {
                            page_id,
                            title: url_t.to_string(),
                        });
                    } else {
                        out.push(InlineNode::Text {
                            text: url_t.to_string(),
                            style: InlineStyle {
                                link_url: Some(url_t.to_string()),
                                ..Default::default()
                            },
                        });
                    }
                    i = end + 1;
                    continue;
                }
            }
        }

        // *bold*, _italic_, -strike-, +underline+
        if matches!(chars[i], '*' | '_' | '-' | '+') && is_wiki_delim_open(&chars, i) {
            let delim = chars[i];
            if let Some(end) = find_closing_wiki_delim(&chars, i + 1, delim) {
                flush_buf(&mut buf, &mut out);
                let inner: String = chars[i + 1..end].iter().collect();
                let mut style = InlineStyle::default();
                match delim {
                    '*' => style.bold = true,
                    '_' => style.italic = true,
                    '-' => style.strike = true,
                    '+' => style.underline = true,
                    _ => {}
                }
                out.push(InlineNode::Text { text: inner, style });
                i = end + 1;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush_buf(&mut buf, &mut out);
    out
}

fn find_char_seq(chars: &[char], start: usize, seq: &[char]) -> Option<usize> {
    if seq.is_empty() || start + seq.len() > chars.len() {
        return None;
    }
    (start..=chars.len() - seq.len()).find(|&idx| &chars[idx..idx + seq.len()] == seq)
}

fn is_wiki_delim_open(chars: &[char], idx: usize) -> bool {
    let prev_ok = idx == 0 || !chars[idx - 1].is_alphanumeric();
    let next_ok = idx + 1 < chars.len() && !chars[idx + 1].is_whitespace() && chars[idx + 1] != chars[idx];
    prev_ok && next_ok
}

fn find_closing_wiki_delim(chars: &[char], start: usize, delim: char) -> Option<usize> {
    for j in start..chars.len() {
        if chars[j] == delim && j > start && !chars[j - 1].is_whitespace() {
            let next_ok = j + 1 >= chars.len() || !chars[j + 1].is_alphanumeric();
            if next_ok {
                return Some(j);
            }
        }
    }
    None
}

// ============================================================================
// 4. Renderers: `DocBlock` -> Markdown, Plain Text, TOC, Cross-Refs
// ============================================================================

/// Renders `DocBlock`s back to clean GitHub-Flavored Markdown (for CLI output, `$EDITOR` round-trip, and FTS5 indexing).
pub fn blocks_to_markdown(blocks: &[DocBlock]) -> String {
    let (md, _) = render_markdown_with_toc(blocks);
    md
}

/// Renders `DocBlock`s to plain searchable text.
pub fn blocks_to_plain_text(blocks: &[DocBlock]) -> String {
    let mut parts = Vec::new();
    collect_plain_text_blocks(blocks, &mut parts);
    parts.join("\n")
}

/// Extracts Table of Contents (`H1`–`H6`) entries from `DocBlock`s with cumulative `line_hint`.
pub fn extract_toc(blocks: &[DocBlock]) -> Vec<TocEntry> {
    let (_, toc) = render_markdown_with_toc(blocks);
    toc
}

fn render_markdown_with_toc(blocks: &[DocBlock]) -> (String, Vec<TocEntry>) {
    let mut lines: Vec<String> = Vec::new();
    let mut toc: Vec<TocEntry> = Vec::new();
    render_blocks_to_lines(blocks, &mut lines, &mut toc, "");
    (lines.join("\n"), toc)
}

fn render_blocks_to_lines(
    blocks: &[DocBlock],
    lines: &mut Vec<String>,
    toc: &mut Vec<TocEntry>,
    prefix: &str,
) {
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            lines.push(prefix.trim_end().to_string());
        }
        match block {
            DocBlock::Paragraph { spans } => {
                let text = inlines_to_markdown(spans);
                for line in text.split('\n') {
                    lines.push(format!("{prefix}{line}"));
                }
            }
            DocBlock::Heading { level, spans } => {
                let hashes = "#".repeat((*level).clamp(1, 6) as usize);
                let line_hint = lines.len();
                let plain_title = inlines_to_plain_text(spans).trim().to_string();
                toc.push(TocEntry {
                    level: *level,
                    text: plain_title,
                    line_hint,
                });
                let rendered = inlines_to_markdown(spans);
                lines.push(format!("{prefix}{hashes} {rendered}"));
            }
            DocBlock::BulletList { items } => {
                for item in items {
                    render_list_item(item, "- ", "  ", prefix, lines, toc);
                }
            }
            DocBlock::OrderedList { start, items } => {
                for (offset, item) in items.iter().enumerate() {
                    let marker = format!("{}. ", start + offset);
                    let indent = " ".repeat(marker.len());
                    render_list_item(item, &marker, &indent, prefix, lines, toc);
                }
            }
            DocBlock::TaskList { items } => {
                for item in items {
                    let box_str = if item.checked { "[x]" } else { "[ ]" };
                    let rendered = inlines_to_markdown(&item.content);
                    lines.push(format!("{prefix}- {box_str} {rendered}"));
                }
            }
            DocBlock::CodeBlock { language, code } => {
                let fence = if code.contains("```") { "````" } else { "```" };
                let lang = language.as_deref().unwrap_or("");
                lines.push(format!("{prefix}{fence}{lang}"));
                for code_line in code.lines() {
                    lines.push(format!("{prefix}{code_line}"));
                }
                if code.ends_with('\n') || code.is_empty() {
                    if code.is_empty() {
                        lines.push(prefix.to_string());
                    }
                }
                lines.push(format!("{prefix}{fence}"));
            }
            DocBlock::Blockquote { blocks: inner } => {
                let quote_prefix = format!("{prefix}> ");
                render_blocks_to_lines(inner, lines, toc, &quote_prefix);
            }
            DocBlock::Panel {
                kind,
                title,
                blocks: inner,
            } => {
                let tag = match kind {
                    PanelKind::Info => "[!INFO]",
                    PanelKind::Note => "[!NOTE]",
                    PanelKind::Warning => "[!WARNING]",
                    PanelKind::Success => "[!TIP]",
                    PanelKind::Error => "[!CAUTION]",
                };
                if let Some(t) = title.as_deref().filter(|s| !s.trim().is_empty()) {
                    lines.push(format!("{prefix}> {tag} {t}"));
                } else {
                    lines.push(format!("{prefix}> {tag}"));
                }
                if !inner.is_empty() {
                    let quote_prefix = format!("{prefix}> ");
                    render_blocks_to_lines(inner, lines, toc, &quote_prefix);
                }
            }
            DocBlock::Expand {
                title,
                blocks: inner,
            } => {
                lines.push(format!("{prefix}<details><summary>{title}</summary>"));
                lines.push(prefix.trim_end().to_string());
                render_blocks_to_lines(inner, lines, toc, prefix);
                lines.push(prefix.trim_end().to_string());
                lines.push(format!("{prefix}</details>"));
            }
            DocBlock::Table { headers, rows } => {
                let col_count = headers
                    .len()
                    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                if col_count > 0 {
                    let hdr_cells: Vec<String> = (0..col_count)
                        .map(|c| {
                            headers
                                .get(c)
                                .map(|s| escape_table_cell(&inlines_to_markdown(s)))
                                .unwrap_or_default()
                        })
                        .collect();
                    lines.push(format!("{prefix}| {} |", hdr_cells.join(" | ")));
                    let sep: Vec<&str> = vec!["---"; col_count];
                    lines.push(format!("{prefix}| {} |", sep.join(" | ")));
                    for row in rows {
                        let row_cells: Vec<String> = (0..col_count)
                            .map(|c| {
                                row.get(c)
                                    .map(|s| escape_table_cell(&inlines_to_markdown(s)))
                                    .unwrap_or_default()
                            })
                            .collect();
                        lines.push(format!("{prefix}| {} |", row_cells.join(" | ")));
                    }
                }
            }
            DocBlock::Media {
                id_or_filename,
                alt,
                url,
            } => {
                let label = alt.as_deref().unwrap_or(id_or_filename);
                let target = url.as_deref().unwrap_or(id_or_filename);
                lines.push(format!("{prefix}![{label}]({target})"));
            }
            DocBlock::Rule => {
                lines.push(format!("{prefix}---"));
            }
        }
    }
}

fn render_list_item(
    item_blocks: &[DocBlock],
    first_marker: &str,
    cont_indent: &str,
    prefix: &str,
    lines: &mut Vec<String>,
    toc: &mut Vec<TocEntry>,
) {
    if item_blocks.is_empty() {
        lines.push(format!("{prefix}{first_marker}"));
        return;
    }
    let mut sub_lines = Vec::new();
    render_blocks_to_lines(item_blocks, &mut sub_lines, toc, "");
    for (idx, sub_line) in sub_lines.into_iter().enumerate() {
        if idx == 0 {
            lines.push(format!("{prefix}{first_marker}{sub_line}"));
        } else if sub_line.is_empty() {
            lines.push(prefix.trim_end().to_string());
        } else {
            lines.push(format!("{prefix}{cont_indent}{sub_line}"));
        }
    }
}

fn escape_table_cell(s: &str) -> String {
    s.replace('\n', " ").replace('|', "\\|")
}

fn inlines_to_markdown(spans: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in spans {
        match node {
            InlineNode::Text { text, style } => {
                let mut formatted = if style.code {
                    if text.contains('`') {
                        format!("`` {text} ``")
                    } else {
                        format!("`{text}`")
                    }
                } else {
                    text.clone()
                };
                if style.bold {
                    formatted = format!("**{formatted}**");
                }
                if style.italic {
                    formatted = format!("*{formatted}*");
                }
                if style.strike {
                    formatted = format!("~~{formatted}~~");
                }
                if style.underline && !style.code {
                    formatted = format!("<u>{formatted}</u>");
                }
                if let Some(ref url) = style.link_url {
                    formatted = format!("[{formatted}]({url})");
                }
                out.push_str(&formatted);
            }
            InlineNode::Mention { display, .. } => {
                out.push('@');
                out.push_str(display.trim_start_matches('@'));
            }
            InlineNode::StatusLozenge { text, .. } => {
                out.push('[');
                out.push_str(text);
                out.push(']');
            }
            InlineNode::IssueRef { key } => {
                out.push_str(key);
            }
            InlineNode::PageRef { page_id, title } => {
                if title.trim().is_empty() || title == page_id {
                    out.push_str(&format!("[page:{page_id}](/pages/{page_id})"));
                } else {
                    out.push_str(&format!("[{title}](/pages/{page_id})"));
                }
            }
            InlineNode::Emoji { text } => out.push_str(text),
            InlineNode::HardBreak => out.push('\n'),
        }
    }
    out
}

fn inlines_to_plain_text(spans: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in spans {
        match node {
            InlineNode::Text { text, .. } => out.push_str(text),
            InlineNode::Mention { display, .. } => {
                out.push('@');
                out.push_str(display.trim_start_matches('@'));
            }
            InlineNode::StatusLozenge { text, .. } => out.push_str(text),
            InlineNode::IssueRef { key } => out.push_str(key),
            InlineNode::PageRef { title, page_id } => {
                if title.trim().is_empty() {
                    out.push_str(page_id);
                } else {
                    out.push_str(title);
                }
            }
            InlineNode::Emoji { text } => out.push_str(text),
            InlineNode::HardBreak => out.push('\n'),
        }
    }
    out
}

fn collect_plain_text_blocks(blocks: &[DocBlock], parts: &mut Vec<String>) {
    for block in blocks {
        match block {
            DocBlock::Paragraph { spans } | DocBlock::Heading { spans, .. } => {
                let t = inlines_to_plain_text(spans);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            DocBlock::BulletList { items } | DocBlock::OrderedList { items, .. } => {
                for item in items {
                    collect_plain_text_blocks(item, parts);
                }
            }
            DocBlock::TaskList { items } => {
                for item in items {
                    let t = inlines_to_plain_text(&item.content);
                    if !t.trim().is_empty() {
                        parts.push(t);
                    }
                }
            }
            DocBlock::CodeBlock { code, .. } => {
                if !code.trim().is_empty() {
                    parts.push(code.clone());
                }
            }
            DocBlock::Blockquote { blocks: inner } => {
                collect_plain_text_blocks(inner, parts);
            }
            DocBlock::Panel {
                title,
                blocks: inner,
                ..
            } => {
                if let Some(t) = title.as_deref().filter(|s| !s.trim().is_empty()) {
                    parts.push(t.to_string());
                }
                collect_plain_text_blocks(inner, parts);
            }
            DocBlock::Expand {
                title,
                blocks: inner,
            } => {
                if !title.trim().is_empty() {
                    parts.push(title.clone());
                }
                collect_plain_text_blocks(inner, parts);
            }
            DocBlock::Table { headers, rows } => {
                if !headers.is_empty() {
                    let row_str: Vec<String> =
                        headers.iter().map(|c| inlines_to_plain_text(c)).collect();
                    parts.push(row_str.join(" "));
                }
                for row in rows {
                    let row_str: Vec<String> =
                        row.iter().map(|c| inlines_to_plain_text(c)).collect();
                    parts.push(row_str.join(" "));
                }
            }
            DocBlock::Media {
                id_or_filename,
                alt,
                ..
            } => {
                parts.push(alt.as_deref().unwrap_or(id_or_filename).to_string());
            }
            DocBlock::Rule => {}
        }
    }
}

struct RefCollector {
    seen_issues: HashSet<String>,
    seen_pages: HashSet<String>,
    refs: Vec<CrossRefTarget>,
}

impl RefCollector {
    fn new() -> Self {
        Self {
            seen_issues: HashSet::new(),
            seen_pages: HashSet::new(),
            refs: Vec::new(),
        }
    }

    fn push_issue(&mut self, key: String) {
        if !is_false_positive_key(&key) && self.seen_issues.insert(key.clone()) {
            self.refs.push(CrossRefTarget::JiraIssue { key, label: None });
        }
    }

    fn push_page(&mut self, page_id: String, title: Option<String>) {
        if self.seen_pages.insert(page_id.clone()) {
            self.refs
                .push(CrossRefTarget::ConfluencePage { page_id, title });
        }
    }
}

/// Extracts all Jira issue keys (`PROJ-123`) and Confluence page links referenced inside `DocBlock`s or raw text.
pub fn extract_cross_refs(blocks: &[DocBlock], raw_text: &str) -> Vec<CrossRefTarget> {
    let mut collector = RefCollector::new();
    collect_refs_from_blocks(blocks, &mut collector);
    scan_text_for_refs(raw_text, &mut collector);
    collector.refs
}

fn collect_refs_from_blocks(blocks: &[DocBlock], collector: &mut RefCollector) {
    for block in blocks {
        match block {
            DocBlock::Paragraph { spans } | DocBlock::Heading { spans, .. } => {
                collect_refs_from_inlines(spans, collector);
            }
            DocBlock::BulletList { items } | DocBlock::OrderedList { items, .. } => {
                for item in items {
                    collect_refs_from_blocks(item, collector);
                }
            }
            DocBlock::TaskList { items } => {
                for item in items {
                    collect_refs_from_inlines(&item.content, collector);
                }
            }
            DocBlock::Blockquote { blocks: inner }
            | DocBlock::Panel { blocks: inner, .. }
            | DocBlock::Expand { blocks: inner, .. } => {
                collect_refs_from_blocks(inner, collector);
            }
            DocBlock::Table { headers, rows } => {
                for cell in headers {
                    collect_refs_from_inlines(cell, collector);
                }
                for row in rows {
                    for cell in row {
                        collect_refs_from_inlines(cell, collector);
                    }
                }
            }
            DocBlock::Media { url: Some(u), .. } => {
                scan_text_for_refs(u, collector);
            }
            _ => {}
        }
    }
}

fn collect_refs_from_inlines(spans: &[InlineNode], collector: &mut RefCollector) {
    for node in spans {
        match node {
            InlineNode::IssueRef { key } => collector.push_issue(key.clone()),
            InlineNode::PageRef { page_id, title } => {
                let t = title.trim();
                collector.push_page(
                    page_id.clone(),
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    },
                );
            }
            InlineNode::Text { text, style } => {
                if let Some(ref url) = style.link_url {
                    scan_text_for_refs(url, collector);
                }
                if !style.code {
                    scan_text_for_refs(text, collector);
                }
            }
            _ => {}
        }
    }
}

fn scan_text_for_refs(text: &str, collector: &mut RefCollector) {
    for m in issue_key_regex().find_iter(text) {
        let key = m.as_str();
        if !is_false_positive_key(key)
            && is_boundary_before(text, m.start())
            && is_boundary_after(text, m.end())
        {
            collector.push_issue(key.to_string());
        }
    }
    for cap in confluence_page_regex().captures_iter(text) {
        if let Some(m) = cap.get(1).or_else(|| cap.get(2)) {
            collector.push_page(m.as_str().to_string(), None);
        }
    }
}

// ============================================================================
// 5. Markdown Parser -> `DocBlock` + Compilers (ADF v1 / Storage XHTML / Jira Wiki)
// ============================================================================

/// Parses GitHub-Flavored Markdown into `Vec<DocBlock>`.
pub fn parse_markdown(markdown: &str) -> Vec<DocBlock> {
    if markdown.trim().is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block ```lang
        if trimmed.starts_with("```") {
            let fence_len = trimmed.bytes().take_while(|&b| b == b'`').count();
            let fence = "`".repeat(fence_len);
            let lang = trimmed[fence_len..].trim();
            let language = if lang.is_empty() {
                None
            } else {
                Some(lang.to_string())
            };
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                let cur = lines[i];
                if cur.trim().starts_with(&fence) && cur.trim().trim_start_matches('`').is_empty()
                {
                    i += 1;
                    break;
                }
                code_lines.push(cur);
                i += 1;
            }
            blocks.push(DocBlock::CodeBlock {
                language,
                code: code_lines.join("\n"),
            });
            continue;
        }

        // Horizontal rule
        if is_md_rule(trimmed) {
            blocks.push(DocBlock::Rule);
            i += 1;
            continue;
        }

        // ATX Heading (# .. ######)
        if let Some((level, rest)) = parse_md_heading(trimmed) {
            blocks.push(DocBlock::Heading {
                level,
                spans: parse_md_inlines(rest),
            });
            i += 1;
            continue;
        }

        // Blockquote / GFM Callout Panel (> [!INFO] / > [!WARNING] / > ...)
        if trimmed.starts_with('>') {
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if let Some(rest) = t.strip_prefix('>') {
                    quote_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
                    i += 1;
                } else {
                    break;
                }
            }
            if let Some(first) = quote_lines.first().copied() {
                if let Some((kind, title)) = parse_callout_header(first.trim()) {
                    let body = quote_lines[1..].join("\n");
                    blocks.push(DocBlock::Panel {
                        kind,
                        title,
                        blocks: parse_markdown(&body),
                    });
                    continue;
                }
            }
            blocks.push(DocBlock::Blockquote {
                blocks: parse_markdown(&quote_lines.join("\n")),
            });
            continue;
        }

        // GFM Pipe Table
        if trimmed.starts_with('|') && i + 1 < lines.len() && is_md_table_separator(lines[i + 1].trim()) {
            let headers = split_md_table_row(trimmed)
                .into_iter()
                .map(|c| parse_md_inlines(&c))
                .collect();
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with('|') {
                    rows.push(
                        split_md_table_row(t)
                            .into_iter()
                            .map(|c| parse_md_inlines(&c))
                            .collect(),
                    );
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(DocBlock::Table { headers, rows });
            continue;
        }

        // Task list: - [ ] / - [x]
        if parse_md_task_line(trimmed).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                if let Some((checked, text)) = parse_md_task_line(lines[i].trim()) {
                    items.push(TaskItem {
                        checked,
                        content: parse_md_inlines(text),
                    });
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(DocBlock::TaskList { items });
            continue;
        }

        // Bullet list: - / * / +
        if let Some(_) = parse_md_bullet_line(trimmed) {
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if parse_md_task_line(t).is_some() {
                    break;
                }
                if let Some(rest) = parse_md_bullet_line(t) {
                    items.push(vec![DocBlock::Paragraph {
                        spans: parse_md_inlines(rest),
                    }]);
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(DocBlock::BulletList { items });
            continue;
        }

        // Ordered list: 1. ...
        if let Some((first_num, _)) = parse_md_ordered_line(trimmed) {
            let mut items = Vec::new();
            while i < lines.len() {
                if let Some((_, rest)) = parse_md_ordered_line(lines[i].trim()) {
                    items.push(vec![DocBlock::Paragraph {
                        spans: parse_md_inlines(rest),
                    }]);
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(DocBlock::OrderedList {
                start: first_num.max(1),
                items,
            });
            continue;
        }

        // Paragraph
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty()
                || t.starts_with("```")
                || is_md_rule(t)
                || parse_md_heading(t).is_some()
                || t.starts_with('>')
                || parse_md_task_line(t).is_some()
                || parse_md_bullet_line(t).is_some()
                || parse_md_ordered_line(t).is_some()
                || (t.starts_with('|')
                    && i + 1 < lines.len()
                    && is_md_table_separator(lines[i + 1].trim()))
            {
                break;
            }
            para_lines.push(t);
            i += 1;
        }
        let joined = para_lines.join(" ");
        blocks.push(DocBlock::Paragraph {
            spans: parse_md_inlines(&joined),
        });
    }

    blocks
}

fn is_md_rule(trimmed: &str) -> bool {
    let clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    clean.len() >= 3
        && (clean.chars().all(|c| c == '-')
            || clean.chars().all(|c| c == '*')
            || clean.chars().all(|c| c == '_'))
}

fn parse_md_heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        Some((hashes as u8, trimmed[hashes + 1..].trim()))
    } else {
        None
    }
}

fn parse_callout_header(line: &str) -> Option<(PanelKind, Option<String>)> {
    let upper_tags = [
        ("[!INFO]", PanelKind::Info),
        ("[!NOTE]", PanelKind::Note),
        ("[!WARNING]", PanelKind::Warning),
        ("[!TIP]", PanelKind::Success),
        ("[!SUCCESS]", PanelKind::Success),
        ("[!IMPORTANT]", PanelKind::Note),
        ("[!CAUTION]", PanelKind::Error),
        ("[!ERROR]", PanelKind::Error),
    ];
    let upper = line.to_ascii_uppercase();
    for (tag, kind) in upper_tags {
        if upper.starts_with(tag) {
            let rest = line[tag.len()..].trim();
            let title = if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
            return Some((kind, title));
        }
    }
    None
}

fn is_md_table_separator(trimmed: &str) -> bool {
    if !trimmed.contains('-') || !trimmed.starts_with('|') {
        return false;
    }
    trimmed
        .trim_matches('|')
        .split('|')
        .all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn split_md_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
    trimmed.split('|').map(|s| s.trim().to_string()).collect()
}

fn parse_md_task_line(trimmed: &str) -> Option<(bool, &str)> {
    let rest = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))?;
    if let Some(after) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
        Some((true, after.trim()))
    } else if let Some(after) = rest.strip_prefix("[ ] ") {
        Some((false, after.trim()))
    } else {
        None
    }
}

fn parse_md_bullet_line(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .map(str::trim)
}

fn parse_md_ordered_line(trimmed: &str) -> Option<(usize, &str)> {
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = &trimmed[digits..];
    if let Some(rest) = after.strip_prefix(". ") {
        let num = trimmed[..digits].parse::<usize>().ok()?;
        Some((num, rest.trim()))
    } else {
        None
    }
}

fn parse_md_inlines(input: &str) -> Vec<InlineNode> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;

    let flush_buf = |buf: &mut String, out: &mut Vec<InlineNode>| {
        if !buf.is_empty() {
            let taken = std::mem::take(buf);
            out.extend(split_text_with_issue_refs(&taken, &InlineStyle::default()));
        }
    };

    while i < chars.len() {
        // Inline code: `...`
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`').map(|p| i + 1 + p) {
                flush_buf(&mut buf, &mut out);
                let code_text: String = chars[i + 1..end].iter().collect();
                out.push(InlineNode::Text {
                    text: code_text,
                    style: InlineStyle {
                        code: true,
                        ..Default::default()
                    },
                });
                i = end + 1;
                continue;
            }
        }

        // Bold: **...**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_char_seq(&chars, i + 2, &['*', '*']) {
                flush_buf(&mut buf, &mut out);
                let inner: String = chars[i + 2..end].iter().collect();
                out.push(InlineNode::Text {
                    text: inner,
                    style: InlineStyle {
                        bold: true,
                        ..Default::default()
                    },
                });
                i = end + 2;
                continue;
            }
        }

        // Strike: ~~...~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_char_seq(&chars, i + 2, &['~', '~']) {
                flush_buf(&mut buf, &mut out);
                let inner: String = chars[i + 2..end].iter().collect();
                out.push(InlineNode::Text {
                    text: inner,
                    style: InlineStyle {
                        strike: true,
                        ..Default::default()
                    },
                });
                i = end + 2;
                continue;
            }
        }

        // Italic: *...* or _..._
        if (chars[i] == '*' || chars[i] == '_') && is_wiki_delim_open(&chars, i) {
            let delim = chars[i];
            if let Some(end) = find_closing_wiki_delim(&chars, i + 1, delim) {
                flush_buf(&mut buf, &mut out);
                let inner: String = chars[i + 1..end].iter().collect();
                out.push(InlineNode::Text {
                    text: inner,
                    style: InlineStyle {
                        italic: true,
                        ..Default::default()
                    },
                });
                i = end + 1;
                continue;
            }
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some(close_br) = chars[i + 1..].iter().position(|&c| c == ']').map(|p| i + 1 + p) {
                if close_br + 1 < chars.len() && chars[close_br + 1] == '(' {
                    if let Some(close_paren) = chars[close_br + 2..]
                        .iter()
                        .position(|&c| c == ')')
                        .map(|p| close_br + 2 + p)
                    {
                        flush_buf(&mut buf, &mut out);
                        let label: String = chars[i + 1..close_br].iter().collect();
                        let url: String = chars[close_br + 2..close_paren].iter().collect();
                        out.push(InlineNode::Text {
                            text: label,
                            style: InlineStyle {
                                link_url: Some(url),
                                ..Default::default()
                            },
                        });
                        i = close_paren + 1;
                        continue;
                    }
                }
            }
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush_buf(&mut buf, &mut out);
    out
}

/// Compiles Markdown into Atlassian Document Format (ADF v1) JSON value.
pub fn markdown_to_adf(markdown: &str) -> Value {
    let blocks = parse_markdown(markdown);
    let content: Vec<Value> = blocks.iter().map(block_to_adf).collect();
    json!({
        "type": "doc",
        "version": 1,
        "content": content
    })
}

fn block_to_adf(block: &DocBlock) -> Value {
    match block {
        DocBlock::Paragraph { spans } => json!({
            "type": "paragraph",
            "content": inlines_to_adf(spans)
        }),
        DocBlock::Heading { level, spans } => json!({
            "type": "heading",
            "attrs": { "level": (*level).clamp(1, 6) },
            "content": inlines_to_adf(spans)
        }),
        DocBlock::BulletList { items } => json!({
            "type": "bulletList",
            "content": items.iter().map(|item| json!({
                "type": "listItem",
                "content": item.iter().map(block_to_adf).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }),
        DocBlock::OrderedList { start, items } => json!({
            "type": "orderedList",
            "attrs": { "order": *start },
            "content": items.iter().map(|item| json!({
                "type": "listItem",
                "content": item.iter().map(block_to_adf).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }),
        DocBlock::TaskList { items } => json!({
            "type": "taskList",
            "attrs": { "localId": "task-list" },
            "content": items.iter().enumerate().map(|(idx, item)| json!({
                "type": "taskItem",
                "attrs": {
                    "localId": format!("task-{idx}"),
                    "state": if item.checked { "DONE" } else { "TODO" }
                },
                "content": inlines_to_adf(&item.content)
            })).collect::<Vec<_>>()
        }),
        DocBlock::CodeBlock { language, code } => {
            let mut attrs = serde_json::Map::new();
            if let Some(lang) = language.as_deref().filter(|s| !s.is_empty()) {
                attrs.insert("language".to_string(), Value::String(lang.to_string()));
            }
            json!({
                "type": "codeBlock",
                "attrs": Value::Object(attrs),
                "content": [{ "type": "text", "text": code }]
            })
        }
        DocBlock::Blockquote { blocks: inner } => json!({
            "type": "blockquote",
            "content": inner.iter().map(block_to_adf).collect::<Vec<_>>()
        }),
        DocBlock::Panel {
            kind,
            title,
            blocks: inner,
        } => {
            let panel_type = match kind {
                PanelKind::Info => "info",
                PanelKind::Note => "note",
                PanelKind::Warning => "warning",
                PanelKind::Success => "success",
                PanelKind::Error => "error",
            };
            let mut attrs = serde_json::Map::new();
            attrs.insert("panelType".to_string(), Value::String(panel_type.to_string()));
            if let Some(t) = title.as_deref().filter(|s| !s.is_empty()) {
                attrs.insert("title".to_string(), Value::String(t.to_string()));
            }
            let mut content: Vec<Value> = inner.iter().map(block_to_adf).collect();
            if content.is_empty() {
                content.push(json!({ "type": "paragraph", "content": [] }));
            }
            json!({
                "type": "panel",
                "attrs": Value::Object(attrs),
                "content": content
            })
        }
        DocBlock::Expand {
            title,
            blocks: inner,
        } => json!({
            "type": "expand",
            "attrs": { "title": title },
            "content": inner.iter().map(block_to_adf).collect::<Vec<_>>()
        }),
        DocBlock::Table { headers, rows } => {
            let mut table_rows = Vec::new();
            if !headers.is_empty() {
                let hdr_cells: Vec<Value> = headers
                    .iter()
                    .map(|cell| {
                        json!({
                            "type": "tableHeader",
                            "content": [{
                                "type": "paragraph",
                                "content": inlines_to_adf(cell)
                            }]
                        })
                    })
                    .collect();
                table_rows.push(json!({ "type": "tableRow", "content": hdr_cells }));
            }
            for row in rows {
                let cells: Vec<Value> = row
                    .iter()
                    .map(|cell| {
                        json!({
                            "type": "tableCell",
                            "content": [{
                                "type": "paragraph",
                                "content": inlines_to_adf(cell)
                            }]
                        })
                    })
                    .collect();
                table_rows.push(json!({ "type": "tableRow", "content": cells }));
            }
            json!({
                "type": "table",
                "attrs": { "isNumberColumnEnabled": false, "layout": "default" },
                "content": table_rows
            })
        }
        DocBlock::Media {
            id_or_filename,
            alt,
            url,
        } => {
            let mut attrs = serde_json::Map::new();
            attrs.insert("id".to_string(), Value::String(id_or_filename.clone()));
            attrs.insert("type".to_string(), Value::String("file".to_string()));
            if let Some(a) = alt {
                attrs.insert("alt".to_string(), Value::String(a.clone()));
            }
            if let Some(u) = url {
                attrs.insert("url".to_string(), Value::String(u.clone()));
            }
            json!({
                "type": "mediaSingle",
                "content": [{ "type": "media", "attrs": Value::Object(attrs) }]
            })
        }
        DocBlock::Rule => json!({ "type": "rule" }),
    }
}

fn inlines_to_adf(spans: &[InlineNode]) -> Vec<Value> {
    spans
        .iter()
        .map(|node| match node {
            InlineNode::Text { text, style } => {
                let mut marks = Vec::new();
                if style.bold {
                    marks.push(json!({ "type": "strong" }));
                }
                if style.italic {
                    marks.push(json!({ "type": "em" }));
                }
                if style.strike {
                    marks.push(json!({ "type": "strike" }));
                }
                if style.underline {
                    marks.push(json!({ "type": "underline" }));
                }
                if style.code {
                    marks.push(json!({ "type": "code" }));
                }
                if let Some(ref href) = style.link_url {
                    marks.push(json!({
                        "type": "link",
                        "attrs": { "href": href }
                    }));
                }
                if marks.is_empty() {
                    json!({ "type": "text", "text": text })
                } else {
                    json!({ "type": "text", "text": text, "marks": marks })
                }
            }
            InlineNode::Mention { id, display } => json!({
                "type": "mention",
                "attrs": { "id": id, "text": format!("@{display}") }
            }),
            InlineNode::StatusLozenge { text, category } => {
                let color = match category {
                    StatusCategory::Done => "green",
                    StatusCategory::InProgress => "blue",
                    StatusCategory::Todo => "neutral",
                };
                json!({
                    "type": "status",
                    "attrs": { "text": text, "color": color }
                })
            }
            InlineNode::IssueRef { key } => json!({ "type": "text", "text": key }),
            InlineNode::PageRef { page_id, title } => json!({
                "type": "text",
                "text": title,
                "marks": [{ "type": "link", "attrs": { "href": format!("/pages/{page_id}") } }]
            }),
            InlineNode::Emoji { text } => json!({
                "type": "emoji",
                "attrs": { "shortName": text, "text": text }
            }),
            InlineNode::HardBreak => json!({ "type": "hardBreak" }),
        })
        .collect()
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Compiles Markdown into Confluence Storage XHTML (for Data Center v1 page/comment writes).
pub fn markdown_to_storage_xhtml(markdown: &str) -> String {
    let blocks = parse_markdown(markdown);
    blocks_to_storage_xhtml(&blocks)
}

fn blocks_to_storage_xhtml(blocks: &[DocBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            DocBlock::Paragraph { spans } => {
                out.push_str("<p>");
                out.push_str(&inlines_to_storage_xhtml(spans));
                out.push_str("</p>");
            }
            DocBlock::Heading { level, spans } => {
                let lvl = (*level).clamp(1, 6);
                out.push_str(&format!("<h{lvl}>{}</h{lvl}>", inlines_to_storage_xhtml(spans)));
            }
            DocBlock::BulletList { items } => {
                out.push_str("<ul>");
                for item in items {
                    out.push_str("<li>");
                    out.push_str(&blocks_to_storage_xhtml(item));
                    out.push_str("</li>");
                }
                out.push_str("</ul>");
            }
            DocBlock::OrderedList { items, .. } => {
                out.push_str("<ol>");
                for item in items {
                    out.push_str("<li>");
                    out.push_str(&blocks_to_storage_xhtml(item));
                    out.push_str("</li>");
                }
                out.push_str("</ol>");
            }
            DocBlock::TaskList { items } => {
                out.push_str("<ac:task-list>");
                for item in items {
                    let st = if item.checked { "complete" } else { "incomplete" };
                    out.push_str(&format!(
                        "<ac:task><ac:task-status>{st}</ac:task-status><ac:task-body>{}</ac:task-body></ac:task>",
                        inlines_to_storage_xhtml(&item.content)
                    ));
                }
                out.push_str("</ac:task-list>");
            }
            DocBlock::CodeBlock { language, code } => {
                out.push_str("<ac:structured-macro ac:name=\"code\">");
                if let Some(lang) = language.as_deref().filter(|s| !s.is_empty()) {
                    out.push_str(&format!(
                        "<ac:parameter ac:name=\"language\">{}</ac:parameter>",
                        xml_escape(lang)
                    ));
                }
                let safe_code = code.replace("]]>", "]]]]><![CDATA[>");
                out.push_str(&format!(
                    "<ac:plain-text-body><![CDATA[{safe_code}]]></ac:plain-text-body>"
                ));
                out.push_str("</ac:structured-macro>");
            }
            DocBlock::Blockquote { blocks: inner } => {
                out.push_str("<blockquote>");
                out.push_str(&blocks_to_storage_xhtml(inner));
                out.push_str("</blockquote>");
            }
            DocBlock::Panel {
                kind,
                title,
                blocks: inner,
            } => {
                let macro_name = match kind {
                    PanelKind::Info => "info",
                    PanelKind::Note | PanelKind::Error => "note",
                    PanelKind::Warning => "warning",
                    PanelKind::Success => "tip",
                };
                out.push_str(&format!("<ac:structured-macro ac:name=\"{macro_name}\">"));
                if let Some(t) = title.as_deref().filter(|s| !s.is_empty()) {
                    out.push_str(&format!(
                        "<ac:parameter ac:name=\"title\">{}</ac:parameter>",
                        xml_escape(t)
                    ));
                }
                out.push_str("<ac:rich-text-body>");
                out.push_str(&blocks_to_storage_xhtml(inner));
                out.push_str("</ac:rich-text-body></ac:structured-macro>");
            }
            DocBlock::Expand {
                title,
                blocks: inner,
            } => {
                out.push_str("<ac:structured-macro ac:name=\"expand\">");
                out.push_str(&format!(
                    "<ac:parameter ac:name=\"title\">{}</ac:parameter>",
                    xml_escape(title)
                ));
                out.push_str("<ac:rich-text-body>");
                out.push_str(&blocks_to_storage_xhtml(inner));
                out.push_str("</ac:rich-text-body></ac:structured-macro>");
            }
            DocBlock::Table { headers, rows } => {
                out.push_str("<table>");
                if !headers.is_empty() {
                    out.push_str("<thead><tr>");
                    for cell in headers {
                        out.push_str(&format!("<th>{}</th>", inlines_to_storage_xhtml(cell)));
                    }
                    out.push_str("</tr></thead>");
                }
                out.push_str("<tbody>");
                for row in rows {
                    out.push_str("<tr>");
                    for cell in row {
                        out.push_str(&format!("<td>{}</td>", inlines_to_storage_xhtml(cell)));
                    }
                    out.push_str("</tr>");
                }
                out.push_str("</tbody></table>");
            }
            DocBlock::Media {
                id_or_filename,
                alt,
                url,
            } => {
                let alt_attr = alt
                    .as_deref()
                    .map(|a| format!(" ac:alt=\"{}\"", xml_escape(a)))
                    .unwrap_or_default();
                if let Some(u) = url {
                    out.push_str(&format!(
                        "<ac:image{alt_attr}><ri:url ri:value=\"{}\"/></ac:image>",
                        xml_escape(u)
                    ));
                } else {
                    out.push_str(&format!(
                        "<ac:image{alt_attr}><ri:attachment ri:filename=\"{}\"/></ac:image>",
                        xml_escape(id_or_filename)
                    ));
                }
            }
            DocBlock::Rule => out.push_str("<hr/>"),
        }
    }
    out
}

fn inlines_to_storage_xhtml(spans: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in spans {
        match node {
            InlineNode::Text { text, style } => {
                let mut inner = xml_escape(text);
                if style.code {
                    inner = format!("<code>{inner}</code>");
                }
                if style.bold {
                    inner = format!("<strong>{inner}</strong>");
                }
                if style.italic {
                    inner = format!("<em>{inner}</em>");
                }
                if style.strike {
                    inner = format!("<del>{inner}</del>");
                }
                if style.underline {
                    inner = format!("<u>{inner}</u>");
                }
                if let Some(ref href) = style.link_url {
                    inner = format!("<a href=\"{}\">{inner}</a>", xml_escape(href));
                }
                out.push_str(&inner);
            }
            InlineNode::Mention { id, display } => {
                out.push_str(&format!(
                    "<ac:link><ri:user ri:account-id=\"{}\"/><ac:plain-text-link-body><![CDATA[@{}]]></ac:plain-text-link-body></ac:link>",
                    xml_escape(id),
                    display
                ));
            }
            InlineNode::StatusLozenge { text, category } => {
                let colour = match category {
                    StatusCategory::Done => "Green",
                    StatusCategory::InProgress => "Blue",
                    StatusCategory::Todo => "Grey",
                };
                out.push_str(&format!(
                    "<ac:structured-macro ac:name=\"status\"><ac:parameter ac:name=\"colour\">{colour}</ac:parameter><ac:parameter ac:name=\"title\">{}</ac:parameter></ac:structured-macro>",
                    xml_escape(text)
                ));
            }
            InlineNode::IssueRef { key } => {
                out.push_str(&format!(
                    "<ac:structured-macro ac:name=\"jira\"><ac:parameter ac:name=\"key\">{}</ac:parameter></ac:structured-macro>",
                    xml_escape(key)
                ));
            }
            InlineNode::PageRef { page_id, title } => {
                out.push_str(&format!(
                    "<a href=\"/pages/{}\">{}</a>",
                    xml_escape(page_id),
                    xml_escape(title)
                ));
            }
            InlineNode::Emoji { text } => out.push_str(&xml_escape(text)),
            InlineNode::HardBreak => out.push_str("<br/>"),
        }
    }
    out
}

/// Compiles Markdown into Jira Wiki Markup (for Data Center v2 issue/comment writes).
pub fn markdown_to_jira_wiki(markdown: &str) -> String {
    let blocks = parse_markdown(markdown);
    blocks_to_jira_wiki(&blocks)
}

fn blocks_to_jira_wiki(blocks: &[DocBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            DocBlock::Paragraph { spans } => parts.push(inlines_to_jira_wiki(spans)),
            DocBlock::Heading { level, spans } => {
                let lvl = (*level).clamp(1, 6);
                parts.push(format!("h{lvl}. {}", inlines_to_jira_wiki(spans)));
            }
            DocBlock::BulletList { items } => {
                let lines: Vec<String> = items
                    .iter()
                    .map(|item| {
                        let text = blocks_to_jira_wiki(item);
                        format!("* {text}")
                    })
                    .collect();
                parts.push(lines.join("\n"));
            }
            DocBlock::OrderedList { items, .. } => {
                let lines: Vec<String> = items
                    .iter()
                    .map(|item| {
                        let text = blocks_to_jira_wiki(item);
                        format!("# {text}")
                    })
                    .collect();
                parts.push(lines.join("\n"));
            }
            DocBlock::TaskList { items } => {
                let lines: Vec<String> = items
                    .iter()
                    .map(|item| {
                        let mark = if item.checked { "(/) " } else { "(x) " };
                        format!("* {mark}{}", inlines_to_jira_wiki(&item.content))
                    })
                    .collect();
                parts.push(lines.join("\n"));
            }
            DocBlock::CodeBlock { language, code } => {
                let open = match language.as_deref().filter(|s| !s.is_empty()) {
                    Some(lang) => format!("{{code:{lang}}}"),
                    None => "{code}".to_string(),
                };
                parts.push(format!("{open}\n{code}\n{{code}}"));
            }
            DocBlock::Blockquote { blocks: inner } => {
                parts.push(format!("{{quote}}\n{}\n{{quote}}", blocks_to_jira_wiki(inner)));
            }
            DocBlock::Panel {
                title,
                blocks: inner,
                ..
            } => {
                let open = match title.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => format!("{{panel:title={t}}}"),
                    None => "{panel}".to_string(),
                };
                parts.push(format!("{open}\n{}\n{{panel}}", blocks_to_jira_wiki(inner)));
            }
            DocBlock::Expand {
                title,
                blocks: inner,
            } => {
                parts.push(format!(
                    "{{panel:title={title}}}\n{}\n{{panel}}",
                    blocks_to_jira_wiki(inner)
                ));
            }
            DocBlock::Table { headers, rows } => {
                let mut tlines = Vec::new();
                if !headers.is_empty() {
                    let cells: Vec<String> = headers.iter().map(|c| inlines_to_jira_wiki(c)).collect();
                    tlines.push(format!("||{}||", cells.join("||")));
                }
                for row in rows {
                    let cells: Vec<String> = row.iter().map(|c| inlines_to_jira_wiki(c)).collect();
                    tlines.push(format!("|{}|", cells.join("|")));
                }
                parts.push(tlines.join("\n"));
            }
            DocBlock::Media { id_or_filename, .. } => parts.push(format!("!{id_or_filename}!")),
            DocBlock::Rule => parts.push("----".to_string()),
        }
    }
    parts.join("\n\n")
}

fn inlines_to_jira_wiki(spans: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in spans {
        match node {
            InlineNode::Text { text, style } => {
                let mut s = if style.code {
                    format!("{{{{{text}}}}}")
                } else {
                    text.clone()
                };
                if style.bold {
                    s = format!("*{s}*");
                }
                if style.italic {
                    s = format!("_{s}_");
                }
                if style.strike {
                    s = format!("-{s}-");
                }
                if style.underline {
                    s = format!("+{s}+");
                }
                if let Some(ref url) = style.link_url {
                    s = format!("[{s}|{url}]");
                }
                out.push_str(&s);
            }
            InlineNode::Mention { id, .. } => out.push_str(&format!("[~{id}]")),
            InlineNode::StatusLozenge { text, .. } => out.push_str(&format!("[{text}]")),
            InlineNode::IssueRef { key } => out.push_str(key),
            InlineNode::PageRef { page_id, title } => {
                out.push_str(&format!("[{title}|/pages/{page_id}]"));
            }
            InlineNode::Emoji { text } => out.push_str(text),
            InlineNode::HardBreak => out.push('\n'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adf_full_parse_and_markdown_roundtrip() {
        let adf = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "heading",
                    "attrs": { "level": 1 },
                    "content": [{ "type": "text", "text": "아키텍처 설계" }]
                },
                {
                    "type": "paragraph",
                    "content": [
                        { "type": "text", "text": "담당: " },
                        { "type": "mention", "attrs": { "id": "u1", "text": "@Alice" } },
                        { "type": "text", "text": " 상태: " },
                        { "type": "status", "attrs": { "text": "IN PROGRESS", "color": "blue" } },
                        { "type": "text", "text": " 관련: PROJ-123 및 " },
                        { "type": "inlineCard", "attrs": { "url": "https://acme.atlassian.net/wiki/spaces/ENG/pages/456789" } }
                    ]
                },
                {
                    "type": "panel",
                    "attrs": { "panelType": "warning", "title": "주의사항" },
                    "content": [
                        { "type": "paragraph", "content": [{ "type": "text", "text": "배포 전 확인 필요" }] }
                    ]
                },
                {
                    "type": "taskList",
                    "content": [
                        {
                            "type": "taskItem",
                            "attrs": { "state": "DONE" },
                            "content": [{ "type": "text", "text": "DB 마이그레이션" }]
                        },
                        {
                            "type": "taskItem",
                            "attrs": { "state": "TODO" },
                            "content": [{ "type": "text", "text": "카나리 배포" }]
                        }
                    ]
                },
                {
                    "type": "codeBlock",
                    "attrs": { "language": "rust" },
                    "content": [{ "type": "text", "text": "fn main() {}" }]
                },
                {
                    "type": "table",
                    "content": [
                        {
                            "type": "tableRow",
                            "content": [
                                { "type": "tableHeader", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "항목" }] }] },
                                { "type": "tableHeader", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "값" }] }] }
                            ]
                        },
                        {
                            "type": "tableRow",
                            "content": [
                                { "type": "tableCell", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "포트" }] }] },
                                { "type": "tableCell", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "8789" }] }] }
                            ]
                        }
                    ]
                }
            ]
        });

        let blocks = parse_adf_value(&adf);
        assert_eq!(blocks.len(), 6);

        let md = blocks_to_markdown(&blocks);
        assert!(md.contains("# 아키텍처 설계"));
        assert!(md.contains("@Alice"));
        assert!(md.contains("[IN PROGRESS]"));
        assert!(md.contains("PROJ-123"));
        assert!(md.contains("> [!WARNING] 주의사항"));
        assert!(md.contains("- [x] DB 마이그레이션"));
        assert!(md.contains("- [ ] 카나리 배포"));
        assert!(md.contains("```rust\nfn main() {}\n```"));
        assert!(md.contains("| 항목 | 값 |"));

        let toc = extract_toc(&blocks);
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].text, "아키텍처 설계");
        assert_eq!(toc[0].line_hint, 0);

        let refs = extract_cross_refs(&blocks, "Also check OPS-99 and pageId=112233");
        assert!(refs.contains(&CrossRefTarget::JiraIssue {
            key: "PROJ-123".into(),
            label: None,
        }));
        assert!(refs.contains(&CrossRefTarget::JiraIssue {
            key: "OPS-99".into(),
            label: None,
        }));
        assert!(refs.contains(&CrossRefTarget::ConfluencePage {
            page_id: "456789".into(),
            title: Some("https://acme.atlassian.net/wiki/spaces/ENG/pages/456789".into()),
        }));
        assert!(refs.contains(&CrossRefTarget::ConfluencePage {
            page_id: "112233".into(),
            title: None,
        }));
    }

    #[test]
    fn test_markdown_to_adf_roundtrip() {
        let md = "# Title\n\nHello **world** with `code` and [link](https://example.com).\n\n> [!NOTE] Heads up\n> Check PROJ-42\n\n- [x] Done\n- [ ] Todo\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n---";
        let adf = markdown_to_adf(md);
        assert_eq!(adf["type"], "doc");
        assert_eq!(adf["version"], 1);
        let parsed = parse_adf_value(&adf);
        let roundtrip = blocks_to_markdown(&parsed);
        assert!(roundtrip.contains("# Title"));
        assert!(roundtrip.contains("**world**"));
        assert!(roundtrip.contains("`code`"));
        assert!(roundtrip.contains("[link](https://example.com)"));
        assert!(roundtrip.contains("> [!NOTE] Heads up"));
        assert!(roundtrip.contains("- [x] Done"));
        assert!(roundtrip.contains("```rust\nlet x = 1;\n```"));
        assert!(roundtrip.contains("| A | B |"));
        assert!(roundtrip.contains("---"));
    }

    #[test]
    fn test_storage_xhtml_macros_and_tasks() {
        let xhtml = r#"
            <h2>Release Notes</h2>
            <p>See <ac:structured-macro ac:name="jira"><ac:parameter ac:name="key">PLAT-77</ac:parameter></ac:structured-macro>
            Status: <ac:structured-macro ac:name="status"><ac:parameter ac:name="colour">Green</ac:parameter><ac:parameter ac:name="title">SHIPPED</ac:parameter></ac:structured-macro></p>
            <ac:structured-macro ac:name="info"><ac:parameter ac:name="title">Notice</ac:parameter><ac:rich-text-body><p>Zero downtime</p></ac:rich-text-body></ac:structured-macro>
            <ac:structured-macro ac:name="code"><ac:parameter ac:name="language">rust</ac:parameter><ac:plain-text-body><![CDATA[fn ok(a: i32, b: i32) -> bool { a < b && b > 0 }]]></ac:plain-text-body></ac:structured-macro>
            <ac:image><ri:attachment ri:filename="arch.png"/></ac:image>
            <ac:task-list><ac:task><ac:task-status>complete</ac:task-status><ac:task-body>Verify SHA-256</ac:task-body></ac:task></ac:task-list>
        "#;
        let blocks = parse_storage_xhtml(xhtml);
        let md = blocks_to_markdown(&blocks);
        assert!(md.contains("## Release Notes"));
        assert!(md.contains("PLAT-77"));
        assert!(md.contains("[SHIPPED]"));
        assert!(md.contains("> [!INFO] Notice"));
        assert!(md.contains("a < b && b > 0"));
        assert!(md.contains("![arch.png](arch.png)"));
        assert!(md.contains("- [x] Verify SHA-256"));
    }

    #[test]
    fn test_jira_wiki_parse_and_compile() {
        let wiki = "h2. Deployment\n{panel:title=Alert}\nCheck *bold* and {{inline}} and [~alice] for OPS-10.\n{panel}\n{code:bash}\necho ok\n{code}\n||Key||Owner||\n|OPS-10|Alice|";
        let blocks = parse_jira_wiki(wiki);
        let md = blocks_to_markdown(&blocks);
        assert!(md.contains("## Deployment"));
        assert!(md.contains("**bold**"));
        assert!(md.contains("`inline`"));
        assert!(md.contains("@alice"));
        assert!(md.contains("OPS-10"));
        assert!(md.contains("```bash\necho ok\n```"));
        assert!(md.contains("| Key | Owner |"));

        let xhtml = markdown_to_storage_xhtml("# Hi\n\n**Bold** & <tag>");
        assert!(xhtml.contains("<h1>Hi</h1>"));
        assert!(xhtml.contains("<strong>Bold</strong> &amp; &lt;tag&gt;"));

        let back_wiki = markdown_to_jira_wiki("# Hi\n\n`code` [docs](https://a.b)");
        assert!(back_wiki.contains("h1. Hi"));
        assert!(back_wiki.contains("{{code}}"));
        assert!(back_wiki.contains("[docs|https://a.b]"));
    }
}
