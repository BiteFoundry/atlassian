//! Partial edits of native Atlassian bodies.

use std::{ops::Range, sync::LazyLock};

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::adf::{markdown_to_adf, markdown_to_jira_wiki, markdown_to_storage_xhtml, xml_escape};

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

#[derive(Clone, Copy)]
pub(crate) enum BodyEdit<'a> {
    Full(&'a str),
    Append(&'a str),
    Replace(&'a TextReplacement),
}

impl<'a> BodyEdit<'a> {
    pub(crate) fn from_inputs(
        full: Option<&'a str>,
        append: Option<&'a str>,
        replace: Option<&'a TextReplacement>,
    ) -> Result<Option<Self>, BodyEditError> {
        match (full, append, replace) {
            (None, None, None) => Ok(None),
            (Some(text), None, None) => Ok(Some(Self::Full(text))),
            (None, Some(text), None) => Ok(Some(Self::Append(text))),
            (None, None, Some(edit)) if edit.old_text.trim().is_empty() => {
                Err(BodyEditError("old_text must contain non-whitespace text"))
            }
            (None, None, Some(edit)) => Ok(Some(Self::Replace(edit))),
            _ => Err(BodyEditError(
                "specify one body operation: full replacement, append, or text replacement",
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
        }
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

fn replace_wiki_text(original: &str, edit: &TextReplacement) -> Result<Option<String>> {
    // Wiki link destinations, macros, and images have no standalone text nodes.
    // Treat their source tokens as boundaries and preserve wiki formatting.
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)\{code[^}]*\}.*?\{code\}|\{noformat\}.*?\{noformat\}|\[[^\]\n]*\]|\{[^{}\n]*\}|![^!\n]*!|https?://[^\s\]]+|[*_+~^]+|\r?\n").unwrap()
    });
    let mut runs = Vec::new();
    let mut run = TextRun::default();
    let mut start = 0;
    for token in TOKEN.find_iter(original) {
        run.push(&original[start..token.start()], start);
        start = token.end();
        if !token.as_str().chars().all(|c| "*_+~^".contains(c)) {
            runs.push(std::mem::take(&mut run));
        }
    }
    run.push(&original[start..], start);
    runs.push(run);
    let (start, range) = unique_match(&runs, &edit.old_text)?;
    if edit.old_text == edit.new_text {
        return Ok(None);
    }
    // Escape wiki metacharacters so the new phrase stays literal.
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
        start + range.start..start + range.end,
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
}
