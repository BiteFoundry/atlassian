//! Partial edits of native Atlassian bodies.

use std::{ops::Range, sync::LazyLock};

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::adf::{
    adf_heading_display_variants, is_wiki_escapable, markdown_to_adf, markdown_to_jira_wiki,
    markdown_to_storage_xhtml, normalize_markdown_heading_text, xml_escape,
};

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct BodyEditError(pub(crate) &'static str);

pub(crate) fn same_markdown(left: &str, right: &str) -> bool {
    left.replace("\r\n", "\n").trim_end_matches('\n')
        == right.replace("\r\n", "\n").trim_end_matches('\n')
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextReplacement {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SectionReplacement {
    pub(crate) heading: String,
    pub(crate) markdown: String,
}

#[derive(Clone, Copy)]
pub(crate) enum BodyEdit<'a> {
    Full(&'a str),
    Append(&'a str),
    Replace(&'a TextReplacement),
    Section(&'a SectionReplacement),
}

impl<'a> BodyEdit<'a> {
    pub(crate) fn from_inputs(
        full: Option<&'a str>,
        append: Option<&'a str>,
        replace: Option<&'a TextReplacement>,
        section: Option<&'a SectionReplacement>,
    ) -> Result<Option<Self>, BodyEditError> {
        match (full, append, replace, section) {
            (None, None, None, None) => Ok(None),
            (Some(text), None, None, None) => Ok(Some(Self::Full(text))),
            (None, Some(text), None, None) => Ok(Some(Self::Append(text))),
            (None, None, Some(edit), None) if edit.old_text.trim().is_empty() => {
                Err(BodyEditError("old_text must contain non-whitespace text"))
            }
            (None, None, Some(edit), None) => Ok(Some(Self::Replace(edit))),
            (None, None, None, Some(edit)) if edit.heading.trim().is_empty() => {
                Err(BodyEditError("heading must contain non-whitespace text"))
            }
            (None, None, None, Some(edit)) => Ok(Some(Self::Section(edit))),
            _ => Err(BodyEditError(
                "specify one body operation: full replacement, append, text replacement, or section replacement",
            )),
        }
    }

    pub(crate) fn adf(self, mut original: Value) -> Result<Option<Value>> {
        if let Self::Full(md) = self {
            return Ok(Some(markdown_to_adf(md)));
        }
        if matches!(self, Self::Append(md) if md.trim().is_empty()) {
            return Ok(None);
        }
        if original.get("type").and_then(Value::as_str) != Some("doc")
            || !original.get("content").is_some_and(Value::is_array)
        {
            return Err(BodyEditError("the original body is not an ADF document").into());
        }
        match self {
            Self::Append(md) => {
                let mut fragment = markdown_to_adf(md);
                let nodes = fragment["content"]
                    .as_array_mut()
                    .context("invalid ADF fragment")?;
                if nodes.is_empty() {
                    return Ok(None);
                }
                original["content"].as_array_mut().unwrap().append(nodes);
            }
            Self::Replace(edit) => {
                let mut runs = Vec::new();
                adf_text_runs(&original, &mut Vec::new(), &mut runs);
                let (path, range) = unique_match(&runs, &edit.old_text)?;
                if edit.old_text == edit.new_text {
                    return Ok(None);
                }
                let mut node = &mut original;
                for &index in path {
                    node = &mut node["content"][index];
                }
                let mut text = node["text"].as_str().unwrap().to_owned();
                text.replace_range(range, &edit.new_text);
                if text.is_empty() {
                    // An ADF text node must have a nonempty text value.
                    let (index, parents) = path.split_last().unwrap();
                    let mut parent = &mut original;
                    for &index in parents {
                        parent = &mut parent["content"][index];
                    }
                    parent["content"].as_array_mut().unwrap().remove(*index);
                } else {
                    node["text"] = Value::String(text);
                }
            }
            Self::Section(edit) => {
                let content = original["content"].as_array_mut().unwrap();
                let target = section_body_range(content, &edit.heading)?;
                let mut fragment = markdown_to_adf(&edit.markdown);
                let replacement = fragment["content"]
                    .as_array_mut()
                    .context("invalid ADF fragment")?;
                let (container, range) = match target {
                    SectionTarget::TopLevel(range) => (content, range),
                    SectionTarget::LayoutColumn {
                        section_index,
                        column_index,
                        range,
                    } => (
                        content[section_index]["content"][column_index]["content"]
                            .as_array_mut()
                            .unwrap(),
                        range,
                    ),
                };
                if container[range.clone()] == replacement[..] {
                    return Ok(None);
                }
                container.splice(range, replacement.drain(..));
            }
            Self::Full(_) => unreachable!(),
        }
        Ok(Some(original))
    }

    pub(crate) fn storage(self, original: &str) -> Result<Option<String>> {
        match self {
            Self::Full(md) => Ok(Some(markdown_to_storage_xhtml(md))),
            Self::Append(md) => {
                let fragment = markdown_to_storage_xhtml(md);
                Ok((!fragment.trim().is_empty()).then(|| format!("{original}{fragment}")))
            }
            Self::Replace(edit) => replace_storage_text(original, edit),
            Self::Section(_) => Err(BodyEditError(
                "section replacement requires a Confluence Cloud ADF page",
            )
            .into()),
        }
    }

    pub(crate) fn wiki(self, original: &str) -> Result<Option<String>> {
        match self {
            Self::Full(md) => Ok(Some(markdown_to_jira_wiki(md))),
            Self::Append(md) => {
                let fragment = markdown_to_jira_wiki(md);
                let separator = if original.is_empty() || original.ends_with("\n\n") {
                    ""
                } else if original.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                Ok((!fragment.trim().is_empty())
                    .then(|| format!("{original}{separator}{fragment}")))
            }
            Self::Replace(edit) => replace_wiki_text(original, edit),
            Self::Section(_) => Err(BodyEditError(
                "section replacement requires a Confluence Cloud ADF page",
            )
            .into()),
        }
    }
}

enum SectionTarget {
    TopLevel(Range<usize>),
    LayoutColumn {
        section_index: usize,
        column_index: usize,
        range: Range<usize>,
    },
}

fn heading_level(node: &Value) -> Option<u64> {
    (node["type"] == "heading")
        .then(|| node.pointer("/attrs/level").and_then(Value::as_u64))
        .flatten()
}

fn heading_matches(node: &Value, target_trimmed: &str, target_normalized: &str) -> bool {
    let Some((plain, markdown)) = adf_heading_display_variants(node) else {
        return false;
    };
    plain == target_trimmed || markdown == target_trimmed || plain == target_normalized
}

/// True for a `layoutSection` with a column that directly holds a heading of `level` or above.
///
/// Such a layout starts sibling sections (`section_body_range` resolves those headings), so it
/// is not part of the section before it.
fn layout_contains_heading_at_or_above(node: &Value, level: u64) -> bool {
    node["type"] == "layoutSection"
        && node["content"].as_array().is_some_and(|columns| {
            columns.iter().any(|column| {
                column["type"] == "layoutColumn"
                    && column["content"].as_array().is_some_and(|children| {
                        children
                            .iter()
                            .any(|child| heading_level(child).is_some_and(|next| next <= level))
                    })
            })
        })
}

fn section_range_in_container(
    content: &[Value],
    heading_index: usize,
    level: u64,
) -> Range<usize> {
    let start = heading_index + 1;
    let end = content[start..]
        .iter()
        .position(|node| {
            heading_level(node).is_some_and(|next| next <= level)
                || layout_contains_heading_at_or_above(node, level)
        })
        .map_or(content.len(), |next| start + next);
    start..end
}

fn section_body_range(content: &[Value], heading: &str) -> Result<SectionTarget> {
    let target_trimmed = heading.trim();
    let target_normalized = normalize_markdown_heading_text(target_trimmed);
    let mut matched: Option<SectionTarget> = None;
    let mut record = |candidate: SectionTarget| -> Result<()> {
        if matched.is_some() {
            return Err(BodyEditError("heading matches more than one section").into());
        }
        matched = Some(candidate);
        Ok(())
    };

    for (index, node) in content.iter().enumerate() {
        if let Some(level) = heading_level(node) {
            if heading_matches(node, target_trimmed, &target_normalized) {
                record(SectionTarget::TopLevel(section_range_in_container(
                    content, index, level,
                )))?;
            }
        } else if node["type"] == "layoutSection" {
            if let Some(columns) = node["content"].as_array() {
                for (column_index, column) in columns.iter().enumerate() {
                    if column["type"] != "layoutColumn" {
                        continue;
                    }
                    let Some(column_content) = column["content"].as_array() else {
                        continue;
                    };
                    for (node_index, child) in column_content.iter().enumerate() {
                        if let Some(level) = heading_level(child) {
                            if heading_matches(child, target_trimmed, &target_normalized) {
                                record(SectionTarget::LayoutColumn {
                                    section_index: index,
                                    column_index,
                                    range: section_range_in_container(
                                        column_content,
                                        node_index,
                                        level,
                                    ),
                                })?;
                            }
                        }
                    }
                }
            }
        }
    }

    matched.ok_or_else(|| BodyEditError("heading was not found").into())
}

/// Return the contiguous portion of the native body replaced by a write.
pub(crate) fn body_change(representation: &str, before: &Value, after: &Value) -> Value {
    let same_adf_attributes = representation == "atlas_doc_format"
        && before
            .as_object()
            .zip(after.as_object())
            .is_some_and(|(a, b)| {
                a.iter()
                    .filter(|(key, _)| *key != "content")
                    .eq(b.iter().filter(|(key, _)| *key != "content"))
            });
    match (
        before["content"].as_array(),
        after["content"].as_array(),
        before.as_str(),
        after.as_str(),
    ) {
        (Some(before), Some(after), _, _) if same_adf_attributes => {
            let start = before.iter().zip(after).take_while(|(a, b)| a == b).count();
            let unchanged_end = before[start..]
                .iter()
                .rev()
                .zip(after[start..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            json!({
                "representation": representation,
                "start": start,
                "before": before[start..before.len() - unchanged_end],
                "after": after[start..after.len() - unchanged_end],
            })
        }
        (_, _, Some(before), Some(after)) => {
            let start: usize = before
                .chars()
                .zip(after.chars())
                .take_while(|(a, b)| a == b)
                .map(|(character, _)| character.len_utf8())
                .sum();
            let unchanged_end: usize = before[start..]
                .chars()
                .rev()
                .zip(after[start..].chars().rev())
                .take_while(|(a, b)| a == b)
                .map(|(character, _)| character.len_utf8())
                .sum();
            json!({
                "representation": representation,
                "start": start,
                "before": &before[start..before.len() - unchanged_end],
                "after": &after[start..after.len() - unchanged_end],
            })
        }
        _ => json!({ "representation": representation, "before": before, "after": after }),
    }
}

#[derive(Default)]
struct TextRun<T> {
    text: String,
    segments: Vec<(Range<usize>, T)>,
}

impl<T> TextRun<T> {
    fn push(&mut self, text: &str, source: T) {
        let start = self.text.len();
        self.text.push_str(text);
        self.segments.push((start..self.text.len(), source));
    }
}

/// Locate a single occurrence, including overlapping matches, within one source text node.
fn unique_match<'a, T>(runs: &'a [TextRun<T>], needle: &str) -> Result<(&'a T, Range<usize>)> {
    let mut found = None;
    for run in runs {
        if let Some(start) = run.text.find(needle) {
            let next = start + needle.chars().next().unwrap().len_utf8();
            if found.is_some() || run.text[next..].contains(needle) {
                return Err(BodyEditError("old_text matches more than one occurrence").into());
            }
            found = Some((run, start..start + needle.len()));
        }
    }
    let (run, target) = found.ok_or(BodyEditError("old_text was not found in body text"))?;
    run.segments
        .iter()
        .find(|(span, _)| span.start <= target.start && target.end <= span.end)
        .map(|(span, source)| (source, target.start - span.start..target.end - span.start))
        .ok_or_else(|| BodyEditError("old_text spans multiple formatted text nodes").into())
}

fn adf_text_runs(node: &Value, path: &mut Vec<usize>, runs: &mut Vec<TextRun<Vec<usize>>>) {
    if node["type"] == "codeBlock" {
        return;
    }
    let Some(children) = node["content"].as_array() else {
        return;
    };
    let mut run = TextRun::default();
    for (index, child) in children.iter().enumerate() {
        path.push(index);
        if let Some(text) = child["text"].as_str().filter(|_| child["type"] == "text") {
            run.push(text, path.clone());
        } else {
            if !run.text.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
            adf_text_runs(child, path, runs);
        }
        path.pop();
    }
    if !run.text.is_empty() {
        runs.push(run);
    }
}

// Token offsets refer to the original XHTML so attributes, namespace prefixes and
// formatting are preserved byte-for-byte. XML quoted attributes can contain '>'.
static XHTML_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<!--.*?-->|<!\[CDATA\[.*?\]\]>|<(?:[^"'>]|"[^"]*"|'[^']*')*>|[^<]+"#).unwrap()
});
static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(?:#[xX][0-9a-fA-F]+|#[0-9]+|[a-zA-Z][a-zA-Z0-9]*);").unwrap());

fn decode_entities(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut decoded = String::new();
    let mut offsets = vec![(0, 0)];
    let mut previous = 0;
    for entity in ENTITY.find_iter(text) {
        let name = &entity.as_str()[1..entity.len() - 1];
        let value = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => name.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        decoded.push_str(&text[previous..entity.start()]);
        offsets.push((decoded.len(), entity.start()));
        decoded.push(value.unwrap_or('\u{fffc}'));
        offsets.push((decoded.len(), entity.end()));
        previous = entity.end();
    }
    decoded.push_str(&text[previous..]);
    (decoded, offsets)
}

fn replace_storage_text(original: &str, edit: &TextReplacement) -> Result<Option<String>> {
    let mut runs = Vec::new();
    let mut run = TextRun::default();
    let mut skipped = Vec::new();
    let mut end = 0;
    for token in XHTML_TOKEN.find_iter(original) {
        if token.start() != end {
            return Err(BodyEditError("the original body has malformed XHTML").into());
        }
        end = token.end();
        let raw = token.as_str();
        if let Some(tag) = raw.strip_prefix('<') {
            let name = tag
                .trim_start_matches('/')
                .split([' ', '\t', '\r', '\n', '/', '>'])
                .next()
                .unwrap_or("");
            let closing = tag.starts_with('/');
            if closing && skipped.last() == Some(&name) {
                skipped.pop();
            } else if !closing
                && !tag.ends_with("/>")
                && matches!(
                    name,
                    "ac:parameter" | "ac:plain-text-body" | "script" | "style" | "pre"
                )
            {
                skipped.push(name);
            }
            let inline = matches!(
                name,
                "a" | "span"
                    | "strong"
                    | "b"
                    | "em"
                    | "i"
                    | "u"
                    | "s"
                    | "strike"
                    | "sub"
                    | "sup"
                    | "code"
                    | "mark"
                    | "del"
                    | "ins"
                    | "small"
                    | "font"
            );
            if !inline && !run.text.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
        } else if skipped.is_empty() {
            let (decoded, offsets) = decode_entities(raw);
            run.push(&decoded, (token.start(), offsets));
        }
    }
    if end != original.len() || !skipped.is_empty() {
        return Err(BodyEditError("the original body has malformed XHTML").into());
    }
    runs.push(run);
    let ((start, offsets), range) = unique_match(&runs, &edit.old_text)?;
    if edit.old_text == edit.new_text {
        return Ok(None);
    }
    let to_offset = |pos| {
        let &(decoded, raw) = offsets
            .iter()
            .rev()
            .find(|(decoded, _)| *decoded <= pos)
            .unwrap();
        start + raw + pos - decoded
    };
    Ok(replace_range(
        original,
        to_offset(range.start)..to_offset(range.end),
        &xml_escape(&edit.new_text),
    ))
}

fn find_wiki_opaque_token(slice: &str) -> Option<usize> {
    // Multiline {code...}...{code} or {noformat}...{noformat}
    if slice.starts_with("{code") {
        if let Some(open_end) = slice.find('}') {
            if let Some(close_rel) = slice[open_end + 1..].find("{code}") {
                return Some(open_end + 1 + close_rel + "{code}".len());
            }
        }
    }
    if slice.starts_with("{noformat}") {
        if let Some(close_rel) = slice["{noformat}".len()..].find("{noformat}") {
            return Some("{noformat}".len() + close_rel + "{noformat}".len());
        }
    }
    // Inline monospace {{...}} on the same line
    if slice.starts_with("{{") {
        let line_end = slice.find('\n').unwrap_or(slice.len());
        if let Some(close_rel) = slice[2..line_end].find("}}") {
            return Some(2 + close_rel + 2);
        }
    }
    // Single-line macro tag {name...}
    if slice.starts_with('{') {
        let line_end = slice.find('\n').unwrap_or(slice.len());
        if let Some(close_rel) = slice[1..line_end].find('}') {
            let inner = &slice[1..1 + close_rel];
            if !inner.is_empty() && inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                return Some(1 + close_rel + 1);
            }
        }
    }
    // Wiki link or mention: [title|url], [url], [~user]
    if slice.starts_with('[') {
        let line_end = slice.find('\n').unwrap_or(slice.len());
        if let Some(close_rel) = slice[1..line_end].find(']') {
            let inner = &slice[1..1 + close_rel];
            if !inner.is_empty() {
                return Some(1 + close_rel + 1);
            }
        }
    }
    // Wiki image token: !image.png! or !https://...! or !file.png|width=200!
    // Never consumes ordinary prose sentences between exclamation marks (`Alert! Watch now!`).
    if slice.starts_with('!') {
        let line_end = slice.find('\n').unwrap_or(slice.len());
        let line = &slice[..line_end];
        if let Some(close_rel) = find_unescaped_char(&line[1..], '!') {
            let inner = &line[1..1 + close_rel];
            if is_wiki_image_target(inner) {
                return Some(1 + close_rel + 1);
            }
        }
    }
    // Bare HTTP(S) URL
    if slice.starts_with("http://") || slice.starts_with("https://") {
        let end = slice
            .find(|c: char| c.is_whitespace() || c == ']')
            .unwrap_or(slice.len());
        if end > "http://".len() {
            return Some(end);
        }
    }
    // Newline boundary
    if slice.starts_with("\r\n") {
        return Some(2);
    }
    if slice.starts_with('\n') {
        return Some(1);
    }
    None
}

fn find_unescaped_char(s: &str, target: char) -> Option<usize> {
    let mut iter = s.char_indices();
    while let Some((idx, ch)) = iter.next() {
        if ch == '\\' {
            let _ = iter.next();
            continue;
        }
        if ch == target {
            return Some(idx);
        }
    }
    None
}

fn is_wiki_image_target(inner: &str) -> bool {
    if inner.is_empty()
        || inner.starts_with(char::is_whitespace)
        || inner.ends_with(char::is_whitespace)
    {
        return false;
    }
    let (target, params) = match inner.split_once('|') {
        Some((t, p)) => (t, Some(p)),
        None => (inner, None),
    };
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return false;
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return true;
    }
    if let Some(p) = params {
        let lower = p.to_ascii_lowercase();
        if lower.contains("thumbnail")
            || lower.contains("width=")
            || lower.contains("height=")
            || lower.contains("alt=")
        {
            return true;
        }
    }
    if let Some((_, ext)) = target.rsplit_once('.') {
        !ext.is_empty()
            && ext.len() <= 8
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
            && !target.starts_with('.')
    } else {
        false
    }
}

fn find_wiki_inline_delim_close(line: &str, open_end: usize, delim: char) -> Option<usize> {
    let after = &line[open_end..];
    let first = after.chars().next()?;
    if first.is_whitespace() || first == delim {
        return None;
    }
    let mut iter = after.char_indices().peekable();
    let mut prev_char: Option<char> = None;
    while let Some((rel, ch)) = iter.next() {
        if ch == '\\' {
            if let Some(&(_, next_ch)) = iter.peek() {
                if is_wiki_escapable(next_ch) {
                    iter.next();
                    prev_char = Some(next_ch);
                    continue;
                }
            }
        }
        if ch == delim
            && rel > 0
            && prev_char.is_some_and(|p| !p.is_whitespace())
            && iter
                .peek()
                .map_or(true, |&(_, next_ch)| !next_ch.is_alphanumeric())
        {
            return Some(open_end + rel);
        }
        prev_char = Some(ch);
    }
    None
}

fn push_wiki_decoded_segment(
    run: &mut TextRun<(usize, Vec<(usize, usize)>)>,
    original: &str,
    seg_start: usize,
    seg_end: usize,
) {
    if seg_start >= seg_end {
        return;
    }
    let raw = &original[seg_start..seg_end];
    let mut decoded = String::with_capacity(raw.len());
    let mut offsets = vec![(0usize, 0usize)];
    let mut iter = raw.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if ch == '\\' {
            if let Some(&(next_idx, next_ch)) = iter.peek() {
                if is_wiki_escapable(next_ch) {
                    iter.next();
                    offsets.push((decoded.len(), idx));
                    decoded.push(next_ch);
                    offsets.push((decoded.len(), next_idx + next_ch.len_utf8()));
                    continue;
                }
            }
        }
        decoded.push(ch);
    }
    run.push(&decoded, (seg_start, offsets));
}

fn replace_wiki_text(original: &str, edit: &TextReplacement) -> Result<Option<String>> {
    // Parse wiki markup into display-text runs with exact source byte offset maps.
    // Words with internal `_` / `+` and prose `!` stay literal; actual formatting pairs,
    // images, links, macros, and code blocks remain protected boundaries.
    let mut runs: Vec<TextRun<(usize, Vec<(usize, usize)>)>> = Vec::new();
    let mut run = TextRun::default();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    let mut active_close: Option<usize> = None;

    let flush_seg = |run: &mut TextRun<(usize, Vec<(usize, usize)>)>,
                     seg_start: &mut usize,
                     end: usize| {
        if *seg_start < end {
            push_wiki_decoded_segment(run, original, *seg_start, end);
            *seg_start = end;
        }
    };

    while i < original.len() {
        if active_close == Some(i) {
            flush_seg(&mut run, &mut seg_start, i);
            let ch_len = original[i..].chars().next().unwrap().len_utf8();
            i += ch_len;
            seg_start = i;
            active_close = None;
            continue;
        }

        let rest = &original[i..];

        // Backslash escape: keep inside the current literal segment
        if let Some(after_bs) = rest.strip_prefix('\\') {
            if let Some(next_ch) = after_bs.chars().next() {
                if is_wiki_escapable(next_ch) {
                    i += 1 + next_ch.len_utf8();
                    continue;
                }
            }
        }

        // Opaque boundaries (macros, code, links, real images, URLs, newlines)
        if let Some(tok_len) = find_wiki_opaque_token(rest) {
            flush_seg(&mut run, &mut seg_start, i);
            if !run.text.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
            i += tok_len;
            seg_start = i;
            if active_close.is_some_and(|c| c < i) {
                active_close = None;
            }
            continue;
        }

        // Real inline formatting pairs (*bold*, _italic_, +underline+, ^sup^, ~sub~)
        let ch = rest.chars().next().unwrap();
        if active_close.is_none() && matches!(ch, '*' | '_' | '+' | '^' | '~') {
            let prev_ok = original[..i]
                .chars()
                .next_back()
                .map_or(true, |p| !p.is_alphanumeric() && p != '\\');
            if prev_ok {
                let line_end = i + rest.find('\n').unwrap_or(rest.len());
                let open_end = i + ch.len_utf8();
                if let Some(close_pos) = find_wiki_inline_delim_close(&original[..line_end], open_end, ch) {
                    flush_seg(&mut run, &mut seg_start, i);
                    i = open_end;
                    seg_start = i;
                    active_close = Some(close_pos);
                    continue;
                }
            }
        }

        i += ch.len_utf8();
    }

    flush_seg(&mut run, &mut seg_start, original.len());
    if !run.text.is_empty() {
        runs.push(run);
    }

    let ((seg_base, offsets), range) = unique_match(&runs, &edit.old_text)?;
    if edit.old_text == edit.new_text {
        return Ok(None);
    }
    let to_offset = |pos: usize| {
        let &(decoded, raw) = offsets
            .iter()
            .rev()
            .find(|(decoded, _)| *decoded <= pos)
            .unwrap();
        seg_base + raw + pos - decoded
    };
    // Escape wiki metacharacters once so the replacement stays literal in Jira Wiki.
    let literal: String = edit
        .new_text
        .chars()
        .flat_map(|c| {
            let escape = "\\*_+~^{}[]!|#".contains(c);
            escape.then_some('\\').into_iter().chain(Some(c))
        })
        .collect();
    Ok(replace_range(
        original,
        to_offset(range.start)..to_offset(range.end),
        &literal,
    ))
}

fn replace_range(original: &str, range: Range<usize>, replacement: &str) -> Option<String> {
    if original[range.clone()] == *replacement {
        return None;
    }
    let mut result = original.to_owned();
    result.replace_range(range, replacement);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replacement(old: &str, new: &str) -> TextReplacement {
        TextReplacement {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn adf_append_retains_native_nodes_and_root_properties() {
        let source = json!({
            "version": 1, "type": "doc", "extra": "keep",
            "content": [{"type": "paragraph", "content": [
                {"type": "mention", "attrs": {"id": "account-01", "text": "@Alice"}},
                {"type": "status", "attrs": {"text": "READY", "color": "green", "localId": "state"}}
            ]}]
        });
        let edited = BodyEdit::Append("Next paragraph.")
            .adf(source.clone())
            .unwrap()
            .unwrap();
        let mut expected = source.clone();
        expected["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"paragraph","content":[{"type":"text","text":"Next paragraph."}]}));
        assert_eq!(edited, expected);
        assert!(BodyEdit::Append(" \r\n ").adf(source).unwrap().is_none());
    }

    #[test]
    fn adf_replacement_keeps_marks_and_refuses_split_or_ambiguous_text() {
        let doc = json!({"type":"doc", "version":1, "content":[
            {"type":"panel","attrs":{"localId":"panel-01"},"content":[
                {"type":"paragraph","content":[
                    {"type":"text","text":"canary","marks":[{"type":"strong"}]},
                    {"type":"text","text":" rollout"}
                ]}
            ]}
        ]});
        let edited = BodyEdit::Replace(&replacement("canary", "canary-verified"))
            .adf(doc.clone())
            .unwrap()
            .unwrap();
        let mut expected = doc.clone();
        expected["content"][0]["content"][0]["content"][0]["text"] = json!("canary-verified");
        assert_eq!(edited, expected);
        for (old, reason) in [
            ("canary rollout", "spans multiple"),
            ("missing", "not found"),
            ("a", "more than one"),
        ] {
            let error = BodyEdit::Replace(&replacement(old, "next"))
                .adf(doc.clone())
                .unwrap_err();
            assert!(error.to_string().contains(reason), "{error:#}");
        }
    }

    #[test]
    fn storage_edits_only_selected_text_or_appends_after_native_xhtml() {
        let source = r#"<ac:layout><ac:layout-section ac:type="two_equal">
<ac:layout-cell><p><strong data-note="2 > 1">canary &amp; stable</strong> next</p>
<ac:structured-macro ac:name="status" ac:macro-id="id"><ac:parameter ac:name="title">canary</ac:parameter></ac:structured-macro>
</ac:layout-cell><ac:layout-cell><ac:image><ri:attachment ri:filename="id.png"/></ac:image></ac:layout-cell>
</ac:layout-section></ac:layout>"#;
        let edited = BodyEdit::Replace(&replacement("canary & stable", "verified < stable"))
            .storage(source)
            .unwrap()
            .unwrap();
        assert_eq!(
            edited,
            source.replacen("canary &amp; stable", "verified &lt; stable", 1)
        );
        assert!(BodyEdit::Replace(&replacement("stable next", "x"))
            .storage(source)
            .unwrap_err()
            .to_string()
            .contains("spans multiple"));
        assert_eq!(
            BodyEdit::Append("Appended.")
                .storage(source)
                .unwrap()
                .unwrap(),
            format!("{source}<p>Appended.</p>")
        );
    }

    #[test]
    fn wiki_append_preserves_markup_and_separates_paragraphs() {
        let source = "h2. Rollout\r\n\r\n[~person] {status:colour=Green|title=Ready}";
        assert_eq!(
            BodyEdit::Append("Confirmed.")
                .wiki(source)
                .unwrap()
                .unwrap(),
            format!("{source}\n\nConfirmed.")
        );
        assert!(BodyEdit::Append("\n ").wiki(source).unwrap().is_none());
    }

    #[test]
    fn adf_section_edit_supports_layout_columns_and_rejects_duplicates() {
        let heading = |text: &str, level: u64| {
            json!({
                "type": "heading",
                "attrs": { "level": level },
                "content": [{ "type": "text", "text": text }]
            })
        };
        let paragraph = |text: &str| {
            json!({
                "type": "paragraph",
                "content": [{ "type": "text", "text": text }]
            })
        };
        let panel = json!({
            "type": "panel",
            "attrs": { "panelType": "info", "localId": "p-1" },
            "content": [heading("Hidden In Panel", 2), paragraph("Keep panel")]
        });
        let table = json!({
            "type": "table",
            "attrs": { "localId": "t-1" },
            "content": [{
                "type": "tableRow",
                "content": [{ "type": "tableCell", "content": [paragraph("Cell")] }]
            }]
        });
        let doc = json!({
            "version": 1,
            "type": "doc",
            "extra": "keep-root",
            "content": [
                panel,
                {
                    "type": "layoutSection",
                    "attrs": { "localId": "ls-1" },
                    "content": [
                        {
                            "type": "layoutColumn",
                            "attrs": { "width": 50 },
                            "content": [
                                paragraph("Before heading"),
                                heading("Column Target", 2),
                                paragraph("Old column body"),
                                heading("Nested Sub", 3),
                                paragraph("Old sub body"),
                                heading("Next Column Section", 2),
                                paragraph("Keep after next")
                            ]
                        },
                        {
                            "type": "layoutColumn",
                            "attrs": { "width": 50 },
                            "content": [
                                heading("Side Column", 2),
                                paragraph("Side body")
                            ]
                        }
                    ]
                },
                table
            ]
        });

        let edit = SectionReplacement {
            heading: "Column Target".into(),
            markdown: "- item 1\n- item 2".into(),
        };
        let updated = BodyEdit::Section(&edit).adf(doc.clone()).unwrap().unwrap();
        assert_eq!(updated["extra"], "keep-root");
        assert_eq!(updated["content"][0], doc["content"][0]);
        assert_eq!(updated["content"][2], doc["content"][2]);
        assert_eq!(updated["content"][1]["attrs"], doc["content"][1]["attrs"]);
        assert_eq!(
            updated["content"][1]["content"][1],
            doc["content"][1]["content"][1]
        );
        let col0 = updated["content"][1]["content"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(col0[0], paragraph("Before heading"));
        assert_eq!(col0[1], heading("Column Target", 2));
        assert_eq!(col0[2]["type"], "bulletList");
        assert_eq!(col0[3], heading("Next Column Section", 2));
        assert_eq!(col0[4], paragraph("Keep after next"));
        assert_eq!(col0.len(), 5);

        // No-op edit returns None.
        assert!(BodyEdit::Section(&edit).adf(updated).unwrap().is_none());

        // Column-end boundary: heading with no following heading stops at end of column.
        let side_edit = SectionReplacement {
            heading: "Side Column".into(),
            markdown: "Updated side".into(),
        };
        let side_updated = BodyEdit::Section(&side_edit)
            .adf(doc.clone())
            .unwrap()
            .unwrap();
        assert_eq!(
            side_updated["content"][1]["content"][0],
            doc["content"][1]["content"][0]
        );
        assert_eq!(
            side_updated["content"][1]["content"][1]["content"],
            json!([heading("Side Column", 2), paragraph("Updated side")])
        );
        assert_eq!(side_updated["content"][2], doc["content"][2]);

        // Empty markdown removes section content while keeping heading and sibling nodes.
        let clear_edit = SectionReplacement {
            heading: "Side Column".into(),
            markdown: "".into(),
        };
        let cleared = BodyEdit::Section(&clear_edit)
            .adf(doc.clone())
            .unwrap()
            .unwrap();
        assert_eq!(
            cleared["content"][1]["content"][1]["content"],
            json!([heading("Side Column", 2)])
        );

        // Heading inside a panel is NOT matched.
        let panel_err = BodyEdit::Section(&SectionReplacement {
            heading: "Hidden In Panel".into(),
            markdown: "x".into(),
        })
        .adf(doc.clone())
        .unwrap_err();
        assert!(panel_err.to_string().contains("was not found"), "{panel_err:#}");

        // Same heading at top-level and inside a layout column is rejected as ambiguous.
        let mut dup_top_and_col = doc.clone();
        dup_top_and_col["content"]
            .as_array_mut()
            .unwrap()
            .push(heading("Column Target", 2));
        let dup_err = BodyEdit::Section(&edit)
            .adf(dup_top_and_col)
            .unwrap_err();
        assert!(
            dup_err.to_string().contains("more than one section"),
            "{dup_err:#}"
        );

        // Same heading in two different layout columns is also rejected as ambiguous.
        let mut dup_two_cols = doc;
        dup_two_cols["content"][1]["content"][1]["content"]
            .as_array_mut()
            .unwrap()
            .push(heading("Column Target", 3));
        let dup_col_err = BodyEdit::Section(&edit).adf(dup_two_cols).unwrap_err();
        assert!(
            dup_col_err.to_string().contains("more than one section"),
            "{dup_col_err:#}"
        );
    }

    fn heading_node(text: &str, level: u64) -> Value {
        json!({
            "type": "heading",
            "attrs": { "level": level },
            "content": [{ "type": "text", "text": text }]
        })
    }

    fn paragraph_node(text: &str) -> Value {
        json!({
            "type": "paragraph",
            "content": [{ "type": "text", "text": text }]
        })
    }

    fn layout_node(columns: Vec<Vec<Value>>) -> Value {
        let columns: Vec<Value> = columns
            .into_iter()
            .map(|content| {
                json!({
                    "type": "layoutColumn",
                    "attrs": { "width": 50 },
                    "content": content
                })
            })
            .collect();
        json!({
            "type": "layoutSection",
            "attrs": { "localId": "ls-1" },
            "content": columns
        })
    }

    /// `H2 Overview`, `Intro`, then `layout`, then `H2 End` and its paragraph.
    fn overview_layout_end_doc(layout: Value) -> Value {
        json!({
            "version": 1,
            "type": "doc",
            "content": [
                heading_node("Overview", 2),
                paragraph_node("Intro"),
                layout,
                heading_node("End", 2),
                paragraph_node("Tail")
            ]
        })
    }

    fn layout_with_sibling_sections() -> Value {
        layout_node(vec![
            vec![heading_node("Details", 2), paragraph_node("Details body")],
            vec![heading_node("Risks", 2), paragraph_node("Risks body")],
        ])
    }

    fn replace_overview(doc: &Value) -> Value {
        let edit = SectionReplacement {
            heading: "Overview".into(),
            markdown: "New intro".into(),
        };
        BodyEdit::Section(&edit).adf(doc.clone()).unwrap().unwrap()
    }

    fn new_intro_nodes() -> Vec<Value> {
        markdown_to_adf("New intro")["content"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn adf_replace_section_stops_before_layout_with_same_level_headings() {
        let doc = overview_layout_end_doc(layout_with_sibling_sections());
        let original = doc["content"].as_array().unwrap();

        let updated = replace_overview(&doc);
        let content = updated["content"].as_array().unwrap();
        let intro = new_intro_nodes();
        assert_eq!(content.len(), original.len() - 1 + intro.len());
        assert_eq!(content[0], original[0]);
        assert_eq!(content[1..1 + intro.len()], intro[..]);
        // The layout and both of its columns survive untouched, and so does the next section.
        assert_eq!(content[1 + intro.len()..], original[2..]);
        assert!(!updated.to_string().contains("\"Intro\""));

        // The same edit is now a no-op.
        let edit = SectionReplacement {
            heading: "Overview".into(),
            markdown: "New intro".into(),
        };
        assert!(BodyEdit::Section(&edit).adf(updated).unwrap().is_none());
    }

    #[test]
    fn adf_replace_section_keeps_layout_without_headings_inside_section() {
        // A layout holding only paragraphs is content of the section, so it is replaced with it.
        let doc = overview_layout_end_doc(layout_node(vec![
            vec![paragraph_node("Left")],
            vec![paragraph_node("Right")],
        ]));
        let original = doc["content"].as_array().unwrap();

        let updated = replace_overview(&doc);
        let content = updated["content"].as_array().unwrap();
        let intro = new_intro_nodes();
        assert_eq!(content[0], original[0]);
        assert_eq!(content[1..1 + intro.len()], intro[..]);
        assert_eq!(content[1 + intro.len()..], original[3..]);
        assert!(content.iter().all(|node| node["type"] != "layoutSection"));
    }

    #[test]
    fn adf_replace_section_keeps_layout_with_deeper_headings_inside_section() {
        // H3 headings in the columns are subsections of the H2 section, so the layout goes with it.
        let doc = overview_layout_end_doc(layout_node(vec![
            vec![heading_node("Sub A", 3), paragraph_node("A body")],
            vec![heading_node("Sub B", 3), paragraph_node("B body")],
        ]));
        let original = doc["content"].as_array().unwrap();

        let updated = replace_overview(&doc);
        let content = updated["content"].as_array().unwrap();
        let intro = new_intro_nodes();
        assert_eq!(content[0], original[0]);
        assert_eq!(content[1..1 + intro.len()], intro[..]);
        assert_eq!(content[1 + intro.len()..], original[3..]);
        assert!(content.iter().all(|node| node["type"] != "layoutSection"));
    }

    #[test]
    fn adf_read_section_matches_replace_boundary() {
        let doc = overview_layout_end_doc(layout_with_sibling_sections());
        let content = doc["content"].as_array().unwrap();

        // The resolver that every section operation shares returns only `Intro` for `Overview`.
        let SectionTarget::TopLevel(range) = section_body_range(content, "Overview").unwrap()
        else {
            panic!("Overview is a top-level section");
        };
        assert_eq!(content[range], [paragraph_node("Intro")]);

        // A heading inside the layout still resolves to its own column section.
        let SectionTarget::LayoutColumn {
            section_index,
            column_index,
            range,
        } = section_body_range(content, "Risks").unwrap()
        else {
            panic!("Risks is a layout-column section");
        };
        assert_eq!((section_index, column_index, range), (2, 1, 1..2));
    }
}
