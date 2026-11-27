//! Terminal formatting, Kanban board renderer, and interactive confirmation helpers.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    atlassian::AtlassianClient,
    img_preview,
    models::{AttachmentItem, BoardSummary, IssueDetail, IssueSummary, PageDetail, SprintSummary, StatusCategory},
};

pub(super) const DELETE_JSON_REQUIRES_YES: &str = "Deleting in --json mode requires passing --yes.";

pub(super) fn print_edit_preview(site_id: &str, preview: Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({ "ok": true, "site_id": site_id, "dry_run": true, "preview": preview })
        )?
    );
    Ok(())
}


pub(crate) fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation received; item was not deleted.")?;
    let trimmed = answer.trim();
    if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
        bail!("Cancelled; item was not deleted.");
    }
    Ok(())
}

pub(super) fn read_body_arg(body: Option<&str>, body_file: Option<&Path>) -> Result<Option<String>> {
    match (body, body_file) {
        (Some(_), Some(_)) => bail!("`--body` and `--body-file` cannot be used together."),
        (Some(b), None) => Ok(Some(b.to_string())),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read body file: {}", path.display()))?;
            Ok(Some(content))
        }
        (None, None) => Ok(None),
    }
}

pub(super) fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line)?;
    if n == 0 {
        bail!("Standard input is closed.");
    }
    Ok(line.trim().to_string())
}

pub(super) fn select_attachment<'a>(
    attachments: &'a [AttachmentItem],
    attachment_id: Option<&str>,
) -> Option<&'a AttachmentItem> {
    if let Some(id) = attachment_id {
        attachments
            .iter()
            .find(|a| a.id == id || a.filename == id)
    } else {
        attachments.first()
    }
}

pub(crate) fn looks_like_jira_key(raw: &str) -> bool {
    let trimmed = raw.trim();
    let Some((prefix, num)) = trimmed.split_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

pub(super) fn print_issue_detail(detail: &IssueDetail) {
    let s = &detail.summary;
    println!("{} — {}", s.key, s.summary);
    println!(
        "Status: [{}] ({}) · Type: {} · Priority: {}",
        s.status.name,
        s.status.category.badge_label(),
        s.issue_type.name,
        s.priority.as_ref().map(|p| p.name.as_str()).unwrap_or("-")
    );
    println!(
        "Assignee: {} · Reporter: {} · Sprint: {}",
        s.assignee
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("Unassigned"),
        s.reporter
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("-"),
        s.sprint_name.as_deref().unwrap_or("-")
    );
    if !s.labels.is_empty() {
        println!("Labels: {}", s.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }

    if !detail.description_markdown.trim().is_empty() {
        println!("\n--- Description ---\n{}", detail.description_markdown.trim());
    }
    if !detail.subtasks.is_empty() {
        println!("\n--- Subtasks ({}) ---", detail.subtasks.len());
        for sub in &detail.subtasks {
            println!("  {} [{}] {}", sub.key, sub.status.name, sub.summary);
        }
    }
    if !detail.linked_pages.is_empty() {
        println!("\n--- Linked Confluence Pages ({}) ---", detail.linked_pages.len());
        for page in &detail.linked_pages {
            println!("  [{}] {} ({})", page.page_id, page.title, page.web_url);
        }
    }
    if !detail.attachments.is_empty() {
        println!("\n--- Attachments ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- Comments ({}) ---", detail.comments.len());
        for c in &detail.comments {
            println!("  [{}] {}:", c.created_at, c.author.display_name);
            for line in c.body_markdown.lines() {
                println!("    {line}");
            }
        }
    }
}

pub(super) fn print_page_detail(detail: &PageDetail) {
    let s = &detail.summary;
    println!("[{}] {} (v{})", s.id, s.title, s.version_number);
    println!(
        "Space: {} · Author: {} · Updated: {}",
        s.space_key,
        s.author_name.as_deref().unwrap_or("-"),
        s.updated_at
    );
    if !detail.ancestors.is_empty() {
        let trail: Vec<&str> = detail.ancestors.iter().map(|a| a.title.as_str()).collect();
        println!("Path: {}", trail.join(" > "));
    }
    if !detail.labels.is_empty() {
        println!("Labels: {}", detail.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }
    if !detail.body_markdown.trim().is_empty() {
        println!("\n--- Body ---\n{}", detail.body_markdown.trim());
    }
    if !detail.linked_issue_keys.is_empty() {
        println!("\n--- Linked Jira Issues ---\n  {}", detail.linked_issue_keys.join(", "));
    }
    if !detail.attachments.is_empty() {
        println!("\n--- Attachments ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- Comments ({}) ---", detail.comments.len());
        for c in &detail.comments {
            println!("  [{}] {}:", c.created_at, c.author_name);
            for line in c.body_markdown.lines() {
                println!("    {line}");
            }
        }
    }
}

pub(super) async fn render_attachments_preview(
    client: &AtlassianClient,
    attachments: &[AttachmentItem],
    cols: Option<u16>,
    rows: Option<u16>,
) {
    for att in attachments {
        if !img_preview::is_image_attachment(&att.mime_type, &att.filename) {
            continue;
        }
        match client.fetch_attachment_bytes(att).await {
            Ok(bytes) => {
                match img_preview::render_for_stdout(
                    &bytes,
                    cols.unwrap_or(64),
                    rows.unwrap_or(18),
                ) {
                    Ok(rendered) => {
                        println!("\n[Image preview: {}]", att.filename);
                        println!("{rendered}");
                    }
                    Err(err) => {
                        eprintln!("Skipped image rendering ({}): {err:#}", att.filename);
                    }
                }
            }
            Err(err) => {
                eprintln!("Skipped image download ({}): {err:#}", att.filename);
            }
        }
    }
}

pub(crate) fn format_kanban_board(
    board: &BoardSummary,
    sprint: Option<&SprintSummary>,
    issues: &[IssueSummary],
) -> String {
    let mut columns: Vec<(String, Vec<&IssueSummary>)> = if !board.columns.is_empty() {
        board
            .columns
            .iter()
            .map(|c| (c.name.clone(), Vec::new()))
            .collect()
    } else {
        vec![
            ("TO DO".to_string(), Vec::new()),
            ("IN PROGRESS".to_string(), Vec::new()),
            ("DONE".to_string(), Vec::new()),
        ]
    };

    for issue in issues {
        let idx = if !board.columns.is_empty() {
            board
                .columns
                .iter()
                .position(|c| {
                    c.status_ids.iter().any(|id| id == &issue.status.id)
                        || c.status_names
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(&issue.status.name))
                })
                .or_else(|| {
                    board
                        .columns
                        .iter()
                        .position(|c| c.category_hint == issue.status.category)
                })
                .unwrap_or(0)
        } else {
            match issue.status.category {
                StatusCategory::Todo => 0,
                StatusCategory::InProgress => 1,
                StatusCategory::Done => 2,
            }
        };
        if let Some((_, list)) = columns.get_mut(idx) {
            list.push(issue);
        }
    }

    let col_width: usize = 28;
    let mut out = String::new();
    let sprint_label = sprint
        .map(|s| format!(" · Sprint: {} [{}]", s.name, s.state))
        .unwrap_or_default();
    out.push_str(&format!(
        "Board #{}: {} ({}){sprint_label}\n",
        board.id, board.name, board.board_type
    ));

    let col_count = columns.len().max(1);
    let top = format!(
        "┌{}┐\n",
        vec!["─".repeat(col_width + 2); col_count].join("┬")
    );
    let mid = format!(
        "├{}┤\n",
        vec!["─".repeat(col_width + 2); col_count].join("┼")
    );
    let bot = format!(
        "└{}┘\n",
        vec!["─".repeat(col_width + 2); col_count].join("┴")
    );

    out.push_str(&top);
    let header_cells: Vec<String> = columns
        .iter()
        .map(|(name, list)| {
            let label = format!("{name} ({})", list.len());
            format!(" {} ", pad_width(&label, col_width))
        })
        .collect();
    out.push_str(&format!("│{}│\n", header_cells.join("│")));
    out.push_str(&mid);

    let max_cards = columns.iter().map(|(_, l)| l.len()).max().unwrap_or(0);
    if max_cards == 0 {
        let empty_cells: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let txt = if i == 0 { "(no issues)" } else { "" };
                format!(" {} ", pad_width(txt, col_width))
            })
            .collect();
        out.push_str(&format!("│{}│\n", empty_cells.join("│")));
    } else {
        for row in 0..max_cards {
            let line1: Vec<String> = columns
                .iter()
                .map(|(_, list)| {
                    let text = list
                        .get(row)
                        .map(|i| {
                            let assignee = i
                                .assignee
                                .as_ref()
                                .map(|u| u.display_name.as_str())
                                .unwrap_or("-");
                            format!("{} ({assignee})", i.key)
                        })
                        .unwrap_or_default();
                    format!(" {} ", pad_width(&text, col_width))
                })
                .collect();
            let line2: Vec<String> = columns
                .iter()
                .map(|(_, list)| {
                    let text = list
                        .get(row)
                        .map(|i| i.summary.clone())
                        .unwrap_or_default();
                    format!(" {} ", pad_width(&text, col_width))
                })
                .collect();
            out.push_str(&format!("│{}│\n", line1.join("│")));
            out.push_str(&format!("│{}│\n", line2.join("│")));
        }
    }
    out.push_str(&bot);
    out
}

fn pad_width(raw: &str, max_width: usize) -> String {
    let mut acc = String::new();
    let mut width = 0;
    for ch in raw.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max_width {
            if max_width >= 1 {
                while width + 1 > max_width && !acc.is_empty() {
                    if let Some(popped) = acc.pop() {
                        width = width.saturating_sub(popped.width().unwrap_or(0));
                    }
                }
                acc.push('…');
                width += 1;
            }
            break;
        }
        acc.push(ch);
        width += cw;
    }
    if width < max_width {
        acc.push_str(&" ".repeat(max_width - width));
    }
    let _ = acc.width();
    acc
}

