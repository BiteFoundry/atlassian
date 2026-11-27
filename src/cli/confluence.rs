//! Confluence CLI subcommand dispatcher and handlers.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    atlassian::{
        body::{BodyEdit, SectionReplacement},
        client::is_not_found_error,
        AtlassianClient,
    },
    config::{self, AppConfig},
    img_preview,
    models::{parse_offset_cursor, slice_page, OutgoingPage},
    serve,
    storage::Storage,
};

use super::{
    args::{ConfluenceWorkflow, PageBodyArgs, WikiCommand},
    format::{
        delete_gate, print_edit_preview, print_page_detail, prompt_line, read_body_arg,
        render_attachments_preview, select_attachment,
    },
    open_client, open_runtime,
};

pub(super) async fn dispatch_wiki_subcommand(cfg: &AppConfig, command: WikiCommand) -> Result<()> {
    match command {
        WikiCommand::Spaces {
            refresh,
            limit,
            cursor,
            json,
        } => Box::pin(cmd_spaces(cfg, refresh, limit, cursor.as_deref(), json)).await,
        WikiCommand::Pages {
            space,
            parent,
            limit,
            cursor,
            offline,
            json,
        } => {
            Box::pin(cmd_pages(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                limit,
                cursor.as_deref(),
                offline,
                json,
            ))
            .await
        }
        WikiCommand::Workflow(command) => {
            Box::pin(dispatch_confluence_workflow(cfg, command)).await
        }
        WikiCommand::View {
            page_id,
            preview_images,
            offline,
            json,
            compact: _,
            full,
        } => {
            Box::pin(cmd_page_view(
                cfg,
                &page_id,
                preview_images,
                offline,
                json,
                !full,
            ))
            .await
        }
        WikiCommand::Create {
            title_pos,
            space,
            parent,
            title,
            body,
            body_file,
            json,
            full,
        } => {
            let resolved_title = title.or(title_pos).unwrap_or_default();
            Box::pin(cmd_page_create(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                &resolved_title,
                body.as_deref(),
                body_file.as_deref(),
                json,
                full,
            ))
            .await
        }
        WikiCommand::Edit {
            page_id,
            title,
            body,
            prev_version,
            message,
            json,
            full,
        } => {
            Box::pin(cmd_page_edit(
                cfg,
                &page_id,
                title,
                body,
                prev_version,
                message,
                json,
                full,
            ))
            .await
        }
        WikiCommand::Comment {
            page_id,
            body,
            json,
        } => Box::pin(cmd_page_comment(cfg, &page_id, &body, json)).await,
        WikiCommand::Attach {
            page_id,
            file,
            json,
        } => Box::pin(cmd_page_attach(cfg, &page_id, &file, json)).await,
        WikiCommand::Download {
            page_id,
            attachment_id,
            dir,
            json,
        } => {
            Box::pin(cmd_page_download(
                cfg,
                &page_id,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        WikiCommand::Delete { page_id, yes, json } => {
            Box::pin(cmd_page_delete(cfg, &page_id, yes, json)).await
        }
        WikiCommand::Search {
            query,
            cql,
            remote,
            limit,
            json,
        } => Box::pin(cmd_page_search(cfg, &query, cql, remote, limit, json)).await,
        WikiCommand::Open { page_id } => Box::pin(cmd_page_open(cfg, &page_id)).await,
    }
}

async fn refresh_cli_page_cache(client: &AtlassianClient, storage: &Storage, page_id: &str) {
    let site_id = client.site_id();
    if let Ok((mut detail, meta)) = client
        .get_page_with_meta_in_cache(page_id, site_id, storage)
        .await
    {
        let _ = storage.upsert_page_detail_for_site_with_meta(site_id, &mut detail, meta);
    }
}


async fn cmd_spaces(
    cfg: &AppConfig,
    refresh: bool,
    limit: usize,
    cursor: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let offset = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
    let limit = serve::clamp_limit(limit);
    let (auth, client, storage) = open_runtime(cfg, refresh)?;
    let site_id = client.site_id();
    let cached = storage.list_spaces_for_site(site_id)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && !auth.is_logged_in() {
            cached
        } else {
            let fetched = client.list_all_spaces().await?;
            let _ = storage.upsert_spaces_for_site(site_id, &fetched);
            let stored = storage.list_spaces_for_site(site_id)?;
            if stored.is_empty() {
                fetched
            } else {
                stored
            }
        }
    } else {
        cached
    };
    let page = slice_page(all, offset, limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "spaces": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No spaces found.");
    } else {
        for s in &page.items {
            println!("{:<12} {:<32} [{}]", s.key, s.name, s.space_type);
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_pages(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let clean_cursor = cursor.map(str::trim).filter(|s| !s.is_empty());
    let first_page = clean_cursor.is_none();
    let offline_offset = if offline {
        parse_offset_cursor(clean_cursor).map_err(anyhow::Error::msg)?
    } else {
        0
    };
    let limit = serve::clamp_limit(limit);
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();

    let page = if let Some(parent_id) = parent {
        if offline {
            let rows = storage.list_child_pages_for_site(site_id, parent_id)?;
            slice_page(rows, offline_offset, limit)
        } else {
            let mut fetched = client
                .list_child_pages_page(parent_id, limit, clean_cursor)
                .await?;
            let _ = storage.record_child_pages_page_for_site(
                site_id,
                parent_id,
                &fetched,
                first_page,
            );
            let _ = storage.overlay_children_state_for_site(site_id, &mut fetched.items);
            fetched
        }
    } else if let Some(sp) = space.or(cfg.default_space_key.as_deref()) {
        if offline {
            let rows = storage.list_space_root_pages_for_site(site_id, sp)?;
            slice_page(rows, offline_offset, limit)
        } else {
            let mut fetched = client
                .list_space_root_pages_page(sp, limit, clean_cursor)
                .await?;
            let _ = storage.record_root_pages_page_for_site(site_id, sp, &fetched, first_page);
            let _ = storage.overlay_children_state_for_site(site_id, &mut fetched.items);
            fetched
        }
    } else if offline {
        let rows = storage.list_recent_pages_for_site(
            site_id,
            offline_offset.saturating_add(limit).saturating_add(1),
        )?;
        slice_page(rows, offline_offset, limit)
    } else {
        bail!("Specify a space key or `--parent <PAGE_ID>`.");
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "pages": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No pages found.");
    } else {
        for p in &page.items {
            let children_mark = if p.has_children == Some(true) {
                "▸"
            } else {
                "·"
            };
            println!(
                "{children_mark} {:<12} [{}] v{} {}",
                p.id, p.space_key, p.version_number, p.title
            );
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_page_view(
    cfg: &AppConfig,
    page_id: &str,
    preview_images: bool,
    offline: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();
    let (detail, detail_stale) = if offline {
        let d = storage
            .get_page_detail_for_site(site_id, page_id)?
            .with_context(|| format!("Page '{page_id}' not found in offline cache."))?;
        let stale = storage
            .is_page_detail_stale_for_site(site_id, page_id)
            .unwrap_or(false);
        (d, stale)
    } else {
        let (mut fetched, meta) = match client
            .get_page_with_meta_in_cache(page_id, site_id, &storage)
            .await
        {
            Ok(res) => res,
            Err(err) => {
                if is_not_found_error(&err) {
                    let _ = storage.delete_page_for_site(site_id, page_id);
                }
                return Err(err);
            }
        };
        let _ = storage.upsert_page_detail_for_site_with_meta(site_id, &mut fetched, meta);
        (fetched, false)
    };

    if json_out {
        let body = if compact {
            let mut hint = format!("atlassian wiki view {page_id} --json --full");
            if offline {
                hint.push_str(" --offline");
            }
            detail.compact_output(&hint)
        } else {
            detail.output(false)
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "site_id": site_id,
                "detail_stale": detail_stale,
                "page": body,
            }))?
        );
        return Ok(());
    }

    print_page_detail(&detail);
    if detail_stale {
        println!("\n[Note] Cached page body is from an earlier version. Fetch online to refresh.");
    }

    if preview_images && !offline {
        render_attachments_preview(&client, &detail.attachments, None, None).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_page_create(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    title: &str,
    body: Option<&str>,
    body_file: Option<&Path>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let space_id_or_key = space
        .or(cfg.default_space_key.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Space key or ID is required (`--space <KEY_OR_ID>`).")?;
    let body_markdown = read_body_arg(body, body_file)?.unwrap_or_default();

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let req = OutgoingPage {
        space_id_or_key: space_id_or_key.to_string(),
        parent_id: parent.map(str::to_string),
        title: title.to_string(),
        body_markdown,
    };
    let created = client.create_page(&req).await?;
    let _ = storage.upsert_page_detail_for_site(site_id, &created);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "site_id": site_id,
                "page": if full { created.output(false) } else { created.write_output() },
            }))?
        );
    } else {
        println!(
            "Page created: [{}] {} ({})",
            created.summary.id, created.summary.title, created.summary.web_url
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_confluence_workflow(cfg: &AppConfig, command: ConfluenceWorkflow) -> Result<()> {
    let (_, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let (result, json_out, message) = match command {
        ConfluenceWorkflow::Move {
            page_id,
            parent_id,
            prev_version,
            json,
        } => {
            let page = client.move_page(&page_id, &parent_id, prev_version).await?;
            refresh_cli_page_cache(&client, &storage, &page_id).await;
            (
                json!({"ok": true, "site_id": site_id, "page": page}),
                json,
                format!("Page {page_id}: moved under parent {parent_id}"),
            )
        }
        ConfluenceWorkflow::Labels {
            page_id,
            add,
            remove,
            json,
        } => {
            let result = if let Some(label) = remove {
                client.remove_page_label(&page_id, &label).await?;
                json!({"ok": true, "site_id": site_id, "page_id": page_id, "removed": label.trim()})
            } else {
                let added = client.add_page_labels(&page_id, &add).await?;
                json!({"ok": true, "site_id": site_id, "page_id": page_id, "added": added})
            };
            refresh_cli_page_cache(&client, &storage, &page_id).await;
            (result, json, format!("Page {page_id}: labels updated"))
        }
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{message}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_page_edit(
    cfg: &AppConfig,
    page_id: &str,
    title: Option<String>,
    body: PageBodyArgs,
    prev_version: Option<i32>,
    message: Option<String>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let dry_run = body.body.dry_run;
    let (full_body, append, replacement) = body.body.read()?;
    let section = body.replace_section.map(|values| {
        let mut values = values.into_iter();
        SectionReplacement {
            heading: values.next().expect("clap requires HEADING"),
            markdown: values.next().expect("clap requires MARKDOWN"),
        }
    });
    let edit = BodyEdit::from_inputs(
        full_body.as_deref(),
        append.as_deref(),
        replacement.as_ref(),
        section.as_ref(),
    )?;
    let (_auth, client) = open_client(cfg, true)?;
    let site_id = client.site_id();
    if dry_run {
        let preview = client
            .preview_page_edit(page_id, title, edit, prev_version, message)
            .await?;
        return print_edit_preview(site_id, preview);
    }
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let updated = client
        .edit_page(page_id, title, edit, prev_version, message)
        .await?;
    let _ = storage.upsert_page_detail_for_site(site_id, &updated);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "site_id": site_id,
                "page": if full { updated.output(false) } else { updated.write_output() },
            }))?
        );
    } else {
        println!(
            "Page updated: [{}] {} (v{})",
            updated.summary.id, updated.summary.title, updated.summary.version_number
        );
    }
    Ok(())
}


async fn cmd_page_comment(
    cfg: &AppConfig,
    page_id: &str,
    body: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let comment = client.add_page_comment(page_id, body).await?;
    let site_id = client.site_id();
    if storage
        .get_page_detail_for_site(site_id, page_id)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = storage.append_page_comment_for_site(site_id, page_id, &comment);
    } else {
        refresh_cli_page_cache(&client, &storage, page_id).await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "comment": comment }))?
        );
    } else {
        println!("Page {page_id}: comment added (id={}).", comment.id);
    }
    Ok(())
}

async fn cmd_page_attach(
    cfg: &AppConfig,
    page_id: &str,
    file: &Path,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let uploaded = client.upload_page_attachment(page_id, file).await?;
    let site_id = client.site_id();
    if !uploaded.is_empty()
        && storage
            .get_page_detail_for_site(site_id, page_id)
            .ok()
            .flatten()
            .is_some()
    {
        let _ = storage.append_page_attachments_for_site(site_id, page_id, &uploaded);
    } else {
        refresh_cli_page_cache(&client, &storage, page_id).await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page_id": page_id, "attachments": uploaded }))?
        );
    } else {
        println!(
            "Page {page_id}: attached '{}' ({} file(s)).",
            file.display(),
            uploaded.len()
        );
    }
    Ok(())
}

async fn cmd_page_download(
    cfg: &AppConfig,
    page_id: &str,
    attachment_id: Option<&str>,
    dir: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let attachments = client.list_page_attachments(page_id).await?;
    let attachment = select_attachment(&attachments, attachment_id)
        .with_context(|| format!("No attachment found to download on page {page_id}."))?;
    let dest_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = client.download_attachment(attachment, &dest_dir).await?;

    if json_out {
        let abs_path = crate::mcp::normalize_abs_path(&downloaded.path);
        let is_image = img_preview::is_image_attachment(&downloaded.mime_type, &downloaded.filename);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": abs_path,
                "file_name": downloaded.filename,
                "mime_type": downloaded.mime_type,
                "size_bytes": downloaded.size,
                "is_image": is_image,
                "file": downloaded,
            }))?
        );
    } else {
        println!(
            "Downloaded: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.size
        );
    }
    Ok(())
}

async fn cmd_page_delete(cfg: &AppConfig, page_id: &str, yes: bool, json_out: bool) -> Result<()> {
    let answer = if !yes && !json_out {
        Some(prompt_line(&format!(
            "Delete Confluence page '{page_id}'? [y/N]: "
        ))?)
    } else {
        None
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    client.delete_page(page_id).await?;
    let _ = storage.delete_page_for_site(site_id, page_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "site_id": site_id, "page_id": page_id }))?
        );
    } else {
        println!("Deleted Confluence page '{page_id}'.");
    }
    Ok(())
}

async fn cmd_page_search(
    cfg: &AppConfig,
    query: &str,
    cql: bool,
    remote: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let limit = serve::clamp_limit(limit);
    let use_remote = remote || cql;
    let (_auth, client, storage) = open_runtime(cfg, use_remote)?;
    let site_id = client.site_id();

    if use_remote {
        let effective_cql = if cql || serve::looks_like_cql_expr(query) {
            query.to_string()
        } else {
            format!("text ~ \"{}\"", serve::escape_jql_str(query))
        };
        let pages = client.search_confluence_cql(&effective_cql, limit).await?;
        let _ = storage.upsert_pages_for_site(site_id, &pages);

        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": true,
                    "site_id": site_id,
                    "cql": effective_cql,
                    "pages": pages,
                }))?
            );
        } else if pages.is_empty() {
            println!("No results found.");
        } else {
            for p in &pages {
                println!("{:<12} [{}] {}", p.id, p.space_key, p.title);
            }
        }
    } else {
        let scope_site = if cfg.active_site.is_some() || cfg.effective_sites().len() <= 1 {
            Some(site_id)
        } else {
            None
        };
        let hits: Vec<_> = storage
            .search_pages_scoped(query, scope_site, limit)?
            .into_iter()
            .map(|(_, h)| h)
            .collect();
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": false,
                    "query": query,
                    "hits": hits,
                }))?
            );
        } else if hits.is_empty() {
            println!("No results in local cache. (Use `--remote` to search Confluence server)");
        } else {
            for h in &hits {
                println!(
                    "{:<12} [{} · {}] {} — {}",
                    h.id_or_key, h.container, h.status_or_version, h.title, h.snippet
                );
            }
        }
    }
    Ok(())
}

async fn cmd_page_open(cfg: &AppConfig, page_id: &str) -> Result<()> {
    let (auth, client, storage) = open_runtime(cfg, false)?;
    let site_id = client.site_id();
    let url = if let Ok(Some(detail)) = storage.get_page_detail_for_site(site_id, page_id) {
        detail.summary.web_url
    } else if auth.is_logged_in() {
        client.get_page(page_id).await?.summary.web_url
    } else {
        let root = cfg.effective_confluence_root();
        if root.is_empty() {
            bail!("Cannot build web URL. Run `confluence login` first.");
        }
        format!("{root}/pages/{page_id}")
    };
    open::that(&url).with_context(|| format!("Failed to open browser: {url}"))?;
    println!("Opened in browser: {url}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared utilities
// ---------------------------------------------------------------------------


