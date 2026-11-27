//! Confluence MCP tool handlers (spaces, pages, tree navigation, comments, labels, attachments, search).

use std::path::Path;

use serde_json::{json, Value};

use crate::{
    atlassian::{body::BodyEdit, client::is_not_found_error},
    models::{parse_offset_cursor, slice_page, FederatedSearchHit, OutgoingPage},
    serve::{clamp_limit, escape_jql_str, looks_like_cql_expr},
};

use super::{
    format_error, refresh_mcp_page_cache, resolve_mcp_client, resolve_mcp_single_site_id,
    McpContext,
};

pub(super) async fn list_confluence_spaces(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
    let refresh = args.optional_bool("refresh")?.unwrap_or(false);
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
    let cached = storage
        .list_spaces_for_site(&target_site)
        .map_err(format_error)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
            cached
        } else {
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let fetched = target_client
                .list_all_spaces()
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_spaces_for_site(&target_site, &fetched);
            let stored = storage
                .list_spaces_for_site(&target_site)
                .map_err(format_error)?;
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
    Ok(json!({
        "site_id": target_site,
        "spaces": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn list_space_pages(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let space_key = args.required_str("space_key")?;
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let cursor = args.optional_str("cursor")?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
    let page = if offline {
        let offset = parse_offset_cursor(cursor.as_deref())?;
        let rows = storage
            .list_space_root_pages_for_site(&target_site, &space_key)
            .map_err(format_error)?;
        slice_page(rows, offset, limit)
    } else {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let mut fetched = target_client
            .list_space_root_pages_page(&space_key, limit, cursor.as_deref())
            .await
            .map_err(format_error)?;
        let _ = storage.record_root_pages_page_for_site(
            &target_site,
            &space_key,
            &fetched,
            cursor.is_none(),
        );
        let _ = storage.overlay_children_state_for_site(&target_site, &mut fetched.items);
        fetched
    };
    Ok(json!({
        "site_id": target_site,
        "space_key": space_key,
        "pages": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn list_child_pages(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(100));
    let cursor = args.optional_str("cursor")?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
    let page = if offline {
        let offset = parse_offset_cursor(cursor.as_deref())?;
        let rows = storage
            .list_child_pages_for_site(&target_site, &page_id)
            .map_err(format_error)?;
        slice_page(rows, offset, limit)
    } else {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let mut fetched = target_client
            .list_child_pages_page(&page_id, limit, cursor.as_deref())
            .await
            .map_err(format_error)?;
        let _ = storage.record_child_pages_page_for_site(
            &target_site,
            &page_id,
            &fetched,
            cursor.is_none(),
        );
        let _ = storage.overlay_children_state_for_site(&target_site, &mut fetched.items);
        fetched
    };
    Ok(json!({
        "site_id": target_site,
        "parent_id": page_id,
        "pages": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn get_confluence_page(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let compact = args.optional_bool("compact")?.unwrap_or(true);
    if offline {
        let target_site =
            resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
        let detail = storage
            .get_page_detail_for_site(&target_site, &page_id)
            .map_err(format_error)?
            .ok_or_else(|| format!("page not found in cache: {page_id}"))?;
        let stale = storage
            .is_page_detail_stale_for_site(&target_site, &page_id)
            .unwrap_or(false);
        return Ok(json!({
            "site_id": target_site,
            "detail_stale": stale,
            "page": if compact {
                detail.compact_output("get_confluence_page with compact:false")
            } else {
                detail.output(false)
            },
        }));
    }
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let (mut fetched, meta) = match Box::pin(
        target_client.get_page_with_meta_in_cache(&page_id, &target_site, storage),
    )
    .await
    {
        Ok(res) => res,
        Err(err) => {
            if is_not_found_error(&err) {
                let _ = storage.delete_page_for_site(&target_site, &page_id);
            }
            return Err(format_error(err));
        }
    };
    let _ = storage.upsert_page_detail_for_site_with_meta(
        &target_site,
        &mut fetched,
        meta,
    );
    Ok(json!({
        "site_id": target_site,
        "detail_stale": false,
        "page": if compact {
            fetched.compact_output("get_confluence_page with compact:false")
        } else {
            fetched.output(false)
        },
    }))
}

pub(super) async fn create_confluence_page(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let space_id_or_key = args.required_str("space_id_or_key")?;
    let parent_id = args.optional_str("parent_id")?;
    let title = args.required_str("title")?;
    let body_markdown = args.optional_text("body_markdown")?.unwrap_or_default();
    let compact = args.optional_bool("compact")?.unwrap_or(true);

    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let req = OutgoingPage {
        space_id_or_key,
        parent_id,
        title,
        body_markdown,
    };
    let detail = Box::pin(target_client.create_page(&req))
        .await
        .map_err(format_error)?;
    let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
    Ok(json!({
        "ok": true,
        "site_id": target_site,
        "page": if compact { detail.write_output() } else { detail.output(false) },
    }))
}

pub(super) async fn update_confluence_page(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let title = args.optional_str("title")?;
    let (full, append, replacement) = args.body_inputs("body_markdown")?;
    let section = args.section_replacement()?;
    let edit = BodyEdit::from_inputs(
        full.as_deref(),
        append.as_deref(),
        replacement.as_ref(),
        section.as_ref(),
    )
    .map_err(|e| format_error(e.into()))?;
    let prev_version = args.page_version()?;
    let version_message = args.optional_str("version_message")?;
    let compact = args.optional_bool("compact")?.unwrap_or(true);

    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    if args.optional_bool("dry_run")?.unwrap_or(false) {
        let preview = Box::pin(target_client.preview_page_edit(
            &page_id,
            title,
            edit,
            prev_version,
            version_message,
        ))
        .await
        .map_err(format_error)?;
        return Ok(
            json!({ "ok": true, "site_id": target_site, "dry_run": true, "preview": preview }),
        );
    }
    let detail = Box::pin(target_client.edit_page(
        &page_id,
        title,
        edit,
        prev_version,
        version_message,
    ))
        .await
        .map_err(format_error)?;
    let _ = storage.upsert_page_detail_for_site(&target_site, &detail);
    Ok(json!({
        "ok": true,
        "site_id": target_site,
        "page": if compact { detail.write_output() } else { detail.output(false) },
    }))
}

pub(super) async fn add_confluence_comment(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let body_markdown = args.required_text("body_markdown")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let comment = target_client
        .add_page_comment(&page_id, &body_markdown)
        .await
        .map_err(format_error)?;
    if storage
        .get_page_detail_for_site(&target_site, &page_id)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = storage.append_page_comment_for_site(&target_site, &page_id, &comment);
    } else {
        refresh_mcp_page_cache(&target_client, storage, &target_site, &page_id).await;
    }
    Ok(json!({ "ok": true, "site_id": target_site, "comment": comment }))
}

pub(super) async fn search_confluence_pages(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let scoped_site = &ctx.scoped_site;
    let query = args.optional_str("query")?;
    let cql = args.optional_str("cql")?;
    let remote = args.optional_bool("remote")?.unwrap_or(false);
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(30));

    if remote || cql.is_some() {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let effective_cql = match cql {
            Some(raw) => raw,
            None => {
                let q = query.as_deref().unwrap_or_default();
                if looks_like_cql_expr(q) {
                    q.to_string()
                } else {
                    format!("text ~ \"{}\"", escape_jql_str(q))
                }
            }
        };
        let pages = target_client
            .search_confluence_cql(&effective_cql, limit)
            .await
            .map_err(format_error)?;
        let _ = storage.upsert_pages_for_site(&target_site, &pages);
        return Ok(json!({
            "site_id": target_site,
            "remote": true,
            "cql": effective_cql,
            "pages": pages,
        }));
    }

    let q = query.unwrap_or_default();
    let hits: Vec<FederatedSearchHit> = storage
        .search_pages_scoped(&q, scoped_site.as_deref(), limit)
        .map_err(format_error)?
        .into_iter()
        .map(|(sid, hit)| {
            let sname = config
                .find_effective_site(&sid)
                .map(|s| s.display_name().to_string());
            FederatedSearchHit::new(sid, sname, hit)
        })
        .collect();
    Ok(json!({
        "site_id": scoped_site,
        "remote": false,
        "query": q,
        "hits": hits,
    }))
}

pub(super) async fn upload_confluence_attachment(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let path = args.required_str("path")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let attachments = client
        .upload_page_attachment(&page_id, Path::new(&path))
        .await
        .map_err(format_error)?;
    if !attachments.is_empty()
        && storage
            .get_page_detail_for_site(&site, &page_id)
            .ok()
            .flatten()
            .is_some()
    {
        let _ = storage.append_page_attachments_for_site(&site, &page_id, &attachments);
    } else {
        refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
    }
    Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "attachments": attachments}))
}

pub(super) async fn move_confluence_page(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let parent_id = args.required_str("parent_id")?;
    let version = args.page_version()?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let page = client
        .move_page(&page_id, &parent_id, version)
        .await
        .map_err(format_error)?;
    refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
    Ok(json!({"ok": true, "site_id": site, "page": page}))
}

pub(super) async fn add_confluence_labels(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let labels = args.required_str_array("labels")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let added = client
        .add_page_labels(&page_id, &labels)
        .await
        .map_err(format_error)?;
    refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
    Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "added": added}))
}

pub(super) async fn remove_confluence_label(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let label = args.required_str("label")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    client
        .remove_page_label(&page_id, &label)
        .await
        .map_err(format_error)?;
    refresh_mcp_page_cache(&client, storage, &site, &page_id).await;
    Ok(json!({"ok": true, "site_id": site, "page_id": page_id, "removed": label}))
}

pub(super) async fn delete_confluence_page(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let page_id = args.required_str("page_id")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    target_client
        .delete_page(&page_id)
        .await
        .map_err(format_error)?;
    let _ = storage.delete_page_for_site(&target_site, &page_id);
    Ok(json!({ "ok": true, "site_id": target_site, "page_id": page_id }))
}
