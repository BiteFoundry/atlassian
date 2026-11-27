//! Cross-product and multi-site MCP tool handlers (sites, identity, federated search, sync, attachments).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{
    atlassian::SiteClientPool,
    img_preview,
    models::FederatedSearchHit,
    serve::{
        clamp_limit, escape_jql_str, issue_to_hit, looks_like_cql_expr, looks_like_jql_expr,
        page_to_hit,
    },
    sync,
};

use super::{format_error, normalize_abs_path, resolve_mcp_client, McpContext};

pub(super) async fn list_sites(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let storage = ctx.storage;
    let mut sites_out = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for site in config.effective_sites() {
        seen_ids.insert(site.id.to_ascii_lowercase());
        let counts = storage
            .cache_counts_for_site(&site.id)
            .unwrap_or_default();
        let db_rec = storage.get_site(&site.id).ok().flatten();
        sites_out.push(json!({
            "id": site.id,
            "name": site.display_name(),
            "site_url": site.site_url,
            "confluence_url": site.effective_confluence_root(),
            "deployment": site.deployment.as_str(),
            "cloud_id": site.cloud_id,
            "account_id": site.account_id,
            "enabled": site.enabled,
            "issue_count": counts.issues,
            "page_count": counts.pages,
            "last_synced_at": db_rec.as_ref().and_then(|r| r.last_synced_at.clone()),
            "last_error": db_rec.as_ref().and_then(|r| r.last_error.clone()),
        }));
    }
    if let Ok(db_sites) = storage.list_sites() {
        for rec in db_sites {
            if seen_ids.insert(rec.site_id.to_ascii_lowercase()) {
                let counts = storage
                    .cache_counts_for_site(&rec.site_id)
                    .unwrap_or_default();
                sites_out.push(json!({
                    "id": rec.site_id,
                    "name": rec.name,
                    "site_url": rec.site_url,
                    "confluence_url": "",
                    "deployment": rec.deployment,
                    "cloud_id": null,
                    "account_id": "default",
                    "enabled": rec.enabled,
                    "issue_count": counts.issues,
                    "page_count": counts.pages,
                    "last_synced_at": rec.last_synced_at,
                    "last_error": rec.last_error,
                }));
            }
        }
    }
    Ok(json!({
        "active_site": config.active_site,
        "default_site": config.default_site,
        "sites": sites_out,
    }))
}

pub(super) async fn get_myself(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let site_arg = &ctx.site_arg;
    let (site_id, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let me = target_client.get_myself().await.map_err(format_error)?;
    target_client.set_cached_me(me.clone());
    Ok(json!({ "site_id": site_id, "user": me }))
}

pub(super) async fn search_all(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let scoped_site = &ctx.scoped_site;
    let query = args.required_str("query")?;
    let scope = args
        .optional_str("scope")?
        .unwrap_or_else(|| "all".to_string())
        .to_ascii_lowercase();
    let remote = args.optional_bool("remote")?.unwrap_or(false);
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(30));

    let hits: Vec<FederatedSearchHit> = if !remote {
        let raw_hits = match scope.as_str() {
            "jira" => storage
                .search_issues_scoped(&query, scoped_site.as_deref(), limit)
                .map_err(format_error)?,
            "confluence" => storage
                .search_pages_scoped(&query, scoped_site.as_deref(), limit)
                .map_err(format_error)?,
            _ => storage
                .unified_search_scoped(&query, scoped_site.as_deref(), limit)
                .map_err(format_error)?,
        };
        raw_hits
            .into_iter()
            .map(|(sid, hit)| {
                let sname = config
                    .find_effective_site(&sid)
                    .map(|s| s.display_name().to_string());
                FederatedSearchHit::new(sid, sname, hit)
            })
            .collect()
    } else {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let target_name = config
            .find_effective_site(&target_site)
            .map(|s| s.display_name().to_string());
        let mut combined = Vec::new();
        let want_jira = scope == "all" || scope == "jira";
        let want_conf = scope == "all" || scope == "confluence";
        let (jira_res, conf_res) = Box::pin(async {
            let jira_fut = async {
                if !want_jira {
                    return Ok(None);
                }
                let jql = if looks_like_jql_expr(&query) {
                    query.clone()
                } else {
                    format!("text ~ \"{}\"", escape_jql_str(&query))
                };
                target_client
                    .search_issues_jql(&jql, limit, None, None)
                    .await
                    .map(Some)
                    .map_err(format_error)
            };
            let conf_fut = async {
                if !want_conf {
                    return Ok(None);
                }
                let cql = if looks_like_cql_expr(&query) {
                    query.clone()
                } else {
                    format!("text ~ \"{}\"", escape_jql_str(&query))
                };
                target_client
                    .search_confluence_cql(&cql, limit)
                    .await
                    .map(Some)
                    .map_err(format_error)
            };
            tokio::join!(jira_fut, conf_fut)
        })
        .await;
        if let Some(page) = jira_res? {
            let _ = storage.upsert_issues_for_site(&target_site, &page.issues);
            combined.extend(page.issues.iter().map(|i| {
                FederatedSearchHit::new(
                    target_site.clone(),
                    target_name.clone(),
                    issue_to_hit(i),
                )
            }));
        }
        if let Some(pages) = conf_res? {
            let _ = storage.upsert_pages_for_site(&target_site, &pages);
            combined.extend(pages.iter().map(|p| {
                FederatedSearchHit::new(
                    target_site.clone(),
                    target_name.clone(),
                    page_to_hit(p),
                )
            }));
        }
        crate::storage::sort_federated_hits(&mut combined);
        combined.truncate(limit);
        combined
    };

    Ok(json!({
        "query": query,
        "scope": scope,
        "site_id": scoped_site,
        "remote": remote,
        "hits": hits,
    }))
}

pub(super) async fn sync_workspace(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let scoped_site = &ctx.scoped_site;
    // No site (and no --site) or "site=all": every enabled site; a named site: that one.
    if scoped_site.is_none() {
        if let Ok(pool) = SiteClientPool::from_config(config) {
            let targets = sync::sync_targets(&pool);
            if targets.len() > 1 {
                let mut breakers = sync::CircuitBreakerMap::new();
                let outcome = Box::pin(sync::run_multi_site_sync(
                    &targets,
                    storage,
                    &mut breakers,
                    sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                    None,
                ))
                .await;
                let counts = storage.cache_counts().map_err(format_error)?;
                let payload = json!({
                    "ok": outcome.succeeded_sites > 0,
                    "summary": outcome.summary,
                    "sites": outcome.per_site,
                    "counts": counts,
                });
                if outcome.total_sites > 0 && outcome.succeeded_sites == 0 {
                    let text = serde_json::to_string_pretty(&payload)
                        .unwrap_or_else(|_| payload.to_string());
                    return Err(text);
                }
                return Ok(payload);
            }
        }
    }
    let (target_site, target_client) =
        resolve_mcp_client(config, client, scoped_site.as_deref())?;
    let summary = Box::pin(sync::bootstrap_workspace_for_site(
        &target_site,
        &target_client,
        storage,
    ))
    .await
    .map_err(format_error)?;
    let counts = storage.cache_counts().map_err(format_error)?;
    Ok(json!({
        "ok": true,
        "site_id": target_site,
        "summary": summary,
        "counts": counts,
    }))
}

pub(super) async fn download_attachment(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let issue_key = args.optional_str("issue_key")?;
    let page_id = args.optional_str("page_id")?;
    let attachment_id = args.optional_str("attachment_id")?;
    let dest_dir_opt = args.optional_str("dest_dir")?;

    let (_target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;

    let (owner_label, attachments) = if let Some(key) = issue_key {
        let atts = target_client
            .list_issue_attachments(&key)
            .await
            .map_err(format_error)?;
        (format!("issue {key}"), atts)
    } else if let Some(pid) = page_id {
        let atts = target_client
            .list_page_attachments(&pid)
            .await
            .map_err(format_error)?;
        (format!("page {pid}"), atts)
    } else {
        return Err("either issue_key or page_id is required".to_string());
    };

    let selected = if let Some(aid) = attachment_id.as_deref() {
        attachments
            .iter()
            .find(|a| a.id == aid || a.filename.eq_ignore_ascii_case(aid))
    } else {
        attachments.first()
    }
    .ok_or_else(|| format!("attachment not found on {owner_label}"))?;

    let dest_dir = dest_dir_opt
        .map(PathBuf::from)
        .unwrap_or_else(|| config.resolved_download_dir());
    let downloaded = target_client
        .download_attachment(selected, &dest_dir)
        .await
        .map_err(format_error)?;

    let abs_path = normalize_abs_path(&downloaded.path);
    let is_image =
        img_preview::is_image_attachment(&downloaded.mime_type, &downloaded.filename);

    Ok(json!({
        "ok": true,
        "path": abs_path,
        "file_name": downloaded.filename,
        "mime_type": downloaded.mime_type,
        "size_bytes": downloaded.size,
        "is_image": is_image,
    }))
}
