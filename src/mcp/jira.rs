//! Jira MCP tool handlers (projects, boards, sprints, issues, transitions, comments, worklogs, attachments).

use std::path::Path;

use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::{
    atlassian::{
        body::BodyEdit,
        client::is_not_found_error,
        AtlassianClient,
    },
    models::{parse_offset_cursor, slice_page, FederatedSearchHit, IssueUpdate, OutgoingIssue},
    resolver::resolve_rejected_assignee,
    serve::{clamp_limit, escape_jql_str, looks_like_jql_expr},
};

use super::{
    format_error, refresh_mcp_issue_cache, resolve_mcp_client, resolve_mcp_single_site_id,
    McpContext, ASSIGNEE_REQUIRED, OFFLINE_JQL_ERROR,
};

pub(super) async fn list_projects(ctx: &McpContext<'_>) -> Result<Value, String> {
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
        .list_projects_for_site(&target_site)
        .map_err(format_error)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
            cached
        } else {
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let fetched = target_client.list_projects().await.map_err(format_error)?;
            let _ = storage.upsert_projects_for_site(&target_site, &fetched);
            let stored = storage
                .list_projects_for_site(&target_site)
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
        "projects": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn list_boards(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
    let project = args.optional_str("project")?;
    let refresh = args.optional_bool("refresh")?.unwrap_or(false);
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
    let cached = storage
        .list_boards_for_site(&target_site, project.as_deref())
        .map_err(format_error)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && config.site_url.trim().is_empty() && config.sites.is_empty() {
            cached
        } else {
            let (target_site, target_client) =
                resolve_mcp_client(config, client, site_arg.as_deref())?;
            let fetched = target_client
                .list_boards(project.as_deref())
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_boards_for_site(&target_site, &fetched);
            let stored = storage
                .list_boards_for_site(&target_site, project.as_deref())
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
        "boards": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn list_sprints(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let board_id = args.required_i64("board_id")?;
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let offset = parse_offset_cursor(args.optional_str("cursor")?.as_deref())?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
    let page = if offline {
        let rows = storage
            .list_sprints_for_site(&target_site, board_id)
            .map_err(format_error)?;
        slice_page(rows, offset, limit)
    } else {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let fetched = target_client
            .list_board_sprints_page(board_id, limit, offset)
            .await
            .map_err(format_error)?;
        let _ = storage.upsert_sprints_for_site(&target_site, &fetched.items);
        fetched
    };
    Ok(json!({
        "site_id": target_site,
        "board_id": board_id,
        "sprints": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn get_board_issues(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let board_id = args.required_i64("board_id")?;
    let sprint_id = args.optional_i64("sprint_id")?;
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let cursor = args.optional_str("cursor")?;
    let offset = parse_offset_cursor(cursor.as_deref())?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let target_site = resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;

    let page = if offline {
        let rows = storage
            .list_board_issues_for_site(
                &target_site,
                board_id,
                sprint_id,
                offset.saturating_add(limit).saturating_add(1),
            )
            .map_err(format_error)?;
        slice_page(rows, offset, limit)
    } else {
        let (target_site, target_client) =
            resolve_mcp_client(config, client, site_arg.as_deref())?;
        let fetched = target_client
            .list_board_issues_page(board_id, sprint_id, limit, offset)
            .await
            .map_err(format_error)?;
        let _ = storage.record_board_issues_page_for_site(
            &target_site,
            board_id,
            sprint_id,
            &fetched,
            cursor.is_none(),
        );
        fetched
    };
    Ok(json!({
        "site_id": target_site,
        "board_id": board_id,
        "sprint_id": sprint_id,
        "issues": page.items,
        "returned": page.items.len(),
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    }))
}

pub(super) async fn search_jira_issues(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let scoped_site = &ctx.scoped_site;
    let jql = args.optional_str("jql")?;
    let query = args.optional_str("query")?;
    let project = args.optional_str("project")?;
    let limit = clamp_limit(args.optional_usize("limit")?.unwrap_or(50));
    let page_token = args.optional_str("page_token")?;
    let start_at = args.optional_nonneg_usize("start_at")?;
    if page_token.is_some() && start_at.is_some() {
        return Err("specify either 'page_token' or 'start_at', not both".to_string());
    }
    let offline = args.optional_bool("offline")?.unwrap_or(false);

    if offline {
        if let Some(q) = query.as_deref() {
            // Only the text reaches the full-text index; project narrows its hits, so
            // ask for the most hits a call allows and cut to limit after filtering.
            let key_prefix = project
                .as_deref()
                .map(|p| format!("{}-", p.to_ascii_lowercase()));
            let fetch = if key_prefix.is_some() { clamp_limit(usize::MAX) } else { limit };
            let hits: Vec<FederatedSearchHit> = storage
                .search_issues_scoped(q, scoped_site.as_deref(), fetch)
                .map_err(format_error)?
                .into_iter()
                .filter(|(_, hit)| {
                    key_prefix.as_deref().is_none_or(|prefix| {
                        hit.id_or_key.to_ascii_lowercase().starts_with(prefix)
                    })
                })
                .take(limit)
                .map(|(sid, hit)| {
                    let sname = config
                        .find_effective_site(&sid)
                        .map(|s| s.display_name().to_string());
                    FederatedSearchHit::new(sid, sname, hit)
                })
                .collect();
            return Ok(json!({
                "offline": true,
                "site_id": scoped_site,
                "query": q,
                "hits": hits,
            }));
        }
        if jql.is_some() {
            return Err(OFFLINE_JQL_ERROR.to_string());
        }
        let target_site =
            resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
        let issues = storage
            .list_issues_for_site(&target_site, project.as_deref(), limit)
            .map_err(format_error)?;
        return Ok(json!({
            "offline": true,
            "site_id": target_site,
            "issues": issues,
        }));
    }

    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let effective_jql = if let Some(raw_jql) = jql {
        raw_jql
    } else if let Some(q) = query {
        if looks_like_jql_expr(&q) {
            q
        } else if let Some(proj) = project.as_deref() {
            format!(
                "project = \"{}\" AND text ~ \"{}\" ORDER BY updated DESC",
                escape_jql_str(proj),
                escape_jql_str(&q)
            )
        } else {
            format!("text ~ \"{}\" ORDER BY updated DESC", escape_jql_str(&q))
        }
    } else if let Some(proj) = project.as_deref().or(config.default_project.as_deref()) {
        format!(
            "project = \"{}\" ORDER BY updated DESC",
            escape_jql_str(proj.trim())
        )
    } else {
        AtlassianClient::my_open_issues_jql().to_string()
    };

    let page = target_client
        .search_issues_jql(&effective_jql, limit, page_token.as_deref(), start_at)
        .await
        .map_err(format_error)?;
    let _ = storage.upsert_issues_for_site(&target_site, &page.issues);

    Ok(json!({
        "site_id": target_site,
        "jql": effective_jql,
        "issues": page.issues,
        "next_page_token": page.next_page_token,
        "next_start_at": page.next_start_at,
        "total": page.total,
    }))
}

pub(super) async fn get_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let compact = args.optional_bool("compact")?.unwrap_or(true);
    let fields = args.optional_str_array("fields")?;
    let hint = "get_jira_issue with fields:[...] or compact:false";
    let format_issue = |detail: &crate::models::IssueDetail| -> Value {
        if !fields.is_empty() {
            detail.projected_output(&fields)
        } else if compact {
            detail.compact_output(hint)
        } else {
            detail.output(false)
        }
    };
    if offline {
        let target_site =
            resolve_mcp_single_site_id(config, client, site_arg.as_deref())?;
        let detail = storage
            .get_issue_detail_for_site(&target_site, &key)
            .map_err(format_error)?
            .ok_or_else(|| format!("issue not found in cache: {key}"))?;
        let stale = storage
            .is_issue_detail_stale_for_site(&target_site, &key)
            .unwrap_or(false);
        return Ok(json!({
            "site_id": target_site,
            "detail_stale": stale,
            "issue": format_issue(&detail),
        }));
    }
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let (mut fetched, meta) = match target_client.get_issue_with_meta(&key).await {
        Ok(res) => res,
        Err(err) => {
            if is_not_found_error(&err) {
                let _ = storage.delete_issue_for_site(&target_site, &key);
            }
            return Err(format_error(err));
        }
    };
    let _ = storage.upsert_issue_detail_for_site_merging(
        &target_site,
        &mut fetched,
        !meta.remote_links_ok,
    );
    Ok(json!({
        "site_id": target_site,
        "detail_stale": false,
        "issue": format_issue(&fetched),
    }))
}

pub(super) async fn create_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let project_key = args.required_str("project_key")?;
    let summary = args.required_str("summary")?;
    let issue_type = args
        .optional_str("issue_type")?
        .unwrap_or_else(|| "Task".to_string());
    let description_markdown = args.optional_text("description_markdown")?.unwrap_or_default();
    let priority = args.optional_str("priority")?;
    let raw_assignee = args.optional_str("assignee_id")?;
    let labels = args.optional_str_array("labels")?;
    let parent_key = args.optional_str("parent_key")?;
    let compact = args.optional_bool("compact")?.unwrap_or(true);

    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let (assignee_id, raw_for_fallback) = match raw_assignee.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("me") => {
            let me = target_client
                .get_myself_cached()
                .await
                .map_err(format_error)?;
            (Some(me.id), None)
        }
        Some(v) => (Some(v.to_string()), Some(v.to_string())),
        None => (None, None),
    };
    let mut req = OutgoingIssue {
        project_key,
        issue_type,
        summary,
        description_markdown,
        priority,
        assignee_id,
        labels,
        parent_key,
    };
    let mut resolved_assignee = None;
    let detail = match Box::pin(target_client.create_issue(&req)).await {
        Ok(detail) => detail,
        Err(err) => {
            if let Some(raw) = raw_for_fallback.as_deref() {
                match Box::pin(resolve_rejected_assignee(
                    &target_client,
                    &req.project_key,
                    raw,
                    &err,
                    true,
                ))
                .await
                {
                    Some(Ok(user)) => {
                        req.assignee_id = Some(user.id.clone());
                        resolved_assignee = Some(user);
                        Box::pin(target_client.create_issue(&req))
                            .await
                            .map_err(format_error)?
                    }
                    Some(Err(ambig)) => return Err(format_error(ambig)),
                    None => return Err(format_error(err)),
                }
            } else {
                return Err(format_error(err));
            }
        }
    };
    let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
    let mut payload = json!({
        "ok": true,
        "site_id": target_site,
        "issue": if compact { detail.write_output() } else { detail.output(false) },
    });
    if let Some(user) = resolved_assignee {
        payload["resolved_assignee"] = json!({
            "id": user.id,
            "display_name": user.display_name,
        });
    }
    Ok(payload)
}

pub(super) async fn update_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let summary = args.optional_str("summary")?;
    let (full, append, replacement) = args.body_inputs("description_markdown")?;
    let edit = BodyEdit::from_inputs(
        full.as_deref(),
        append.as_deref(),
        replacement.as_ref(),
        None,
    )
    .map_err(|e| format_error(e.into()))?;
    let priority = args.optional_str("priority")?;
    let raw_assignee = args.optional_nullable_str("assignee_id")?;
    let labels = args.optional_str_array_opt("labels")?;
    let compact = args.optional_bool("compact")?.unwrap_or(true);

    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let (assignee_id, raw_for_fallback) = match raw_assignee {
        Some(Some(v)) if v.eq_ignore_ascii_case("me") => {
            let me = target_client
                .get_myself_cached()
                .await
                .map_err(format_error)?;
            (Some(Some(me.id)), None)
        }
        Some(Some(v)) => (Some(Some(v.clone())), Some(v)),
        other => (other, None),
    };
    let mut patch = IssueUpdate {
        summary,
        description_markdown: None,
        priority,
        assignee_id,
        labels,
    };
    if args.optional_bool("dry_run")?.unwrap_or(false) {
        let preview = target_client
            .preview_issue_edit(&key, &patch, edit)
            .await
            .map_err(format_error)?;
        return Ok(
            json!({ "ok": true, "site_id": target_site, "dry_run": true, "preview": preview }),
        );
    }
    let mut resolved_assignee = None;
    let detail = match Box::pin(target_client.edit_issue(&key, &patch, edit)).await {
        Ok(detail) => detail,
        Err(err) => {
            if let Some(raw) = raw_for_fallback.as_deref() {
                match Box::pin(resolve_rejected_assignee(
                    &target_client,
                    &key,
                    raw,
                    &err,
                    true,
                ))
                .await
                {
                    Some(Ok(user)) => {
                        patch.assignee_id = Some(Some(user.id.clone()));
                        resolved_assignee = Some(user);
                        Box::pin(target_client.edit_issue(&key, &patch, edit))
                            .await
                            .map_err(format_error)?
                    }
                    Some(Err(ambig)) => return Err(format_error(ambig)),
                    None => return Err(format_error(err)),
                }
            } else {
                return Err(format_error(err));
            }
        }
    };
    let _ = storage.upsert_issue_detail_for_site(&target_site, &detail);
    let mut payload = json!({
        "ok": true,
        "site_id": target_site,
        "issue": if compact { detail.write_output() } else { detail.output(false) },
    });
    if let Some(user) = resolved_assignee {
        payload["resolved_assignee"] = json!({
            "id": user.id,
            "display_name": user.display_name,
        });
    }
    Ok(payload)
}

pub(super) async fn list_jira_transitions(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let transitions = target_client
        .list_transitions(&key)
        .await
        .map_err(format_error)?;
    Ok(json!({ "site_id": target_site, "key": key, "transitions": transitions }))
}

pub(super) async fn transition_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let transition = args.required_str("transition")?;
    let comment = args.optional_text("comment_markdown")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let executed = target_client
        .transition_issue(&key, &transition, comment.as_deref())
        .await
        .map_err(format_error)?;
    refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
    let transition_id = executed
        .as_ref()
        .map(|t| t.id.as_str())
        .unwrap_or(transition.as_str());
    let mut payload = json!({
        "ok": true,
        "site_id": target_site,
        "key": key,
        "transition": transition,
        "transition_id": transition_id,
    });
    if let Some(tr) = executed {
        payload["to_status"] = json!(tr.to_status.name);
    }
    Ok(payload)
}

pub(super) async fn assign_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    // Absent is an error, not "unassign": only an explicit null unassigns.
    let raw_assignee = args
        .optional_nullable_str("assignee_id")?
        .ok_or_else(|| ASSIGNEE_REQUIRED.to_string())?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let (mut resolved, raw_for_fallback) = match raw_assignee.as_deref() {
        None => (None, None),
        Some(v) if v.eq_ignore_ascii_case("me") => {
            let me = target_client
                .get_myself_cached()
                .await
                .map_err(format_error)?;
            (Some(me.id), None)
        }
        Some(v) => (Some(v.to_string()), Some(v.to_string())),
    };
    let mut resolved_assignee = None;
    if let Err(err) = Box::pin(target_client.assign_issue(&key, resolved.as_deref())).await
    {
        if let Some(raw) = raw_for_fallback.as_deref() {
            match Box::pin(resolve_rejected_assignee(
                &target_client,
                &key,
                raw,
                &err,
                false,
            ))
            .await
            {
                Some(Ok(user)) => {
                    resolved = Some(user.id.clone());
                    resolved_assignee = Some(user);
                    Box::pin(target_client.assign_issue(&key, resolved.as_deref()))
                        .await
                        .map_err(format_error)?;
                }
                Some(Err(ambig)) => return Err(format_error(ambig)),
                None => return Err(format_error(err)),
            }
        } else {
            return Err(format_error(err));
        }
    }
    refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
    let mut payload = json!({
        "ok": true,
        "site_id": target_site,
        "key": key,
        "assignee_id": resolved,
    });
    if let Some(user) = resolved_assignee {
        payload["resolved_assignee"] = json!({
            "id": user.id,
            "display_name": user.display_name,
        });
    }
    Ok(payload)
}

pub(super) async fn add_jira_comment(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let body_markdown = args.required_text("body_markdown")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let comment = target_client
        .add_issue_comment(&key, &body_markdown)
        .await
        .map_err(format_error)?;
    if storage
        .get_issue_detail_for_site(&target_site, &key)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = storage.append_issue_comment_for_site(&target_site, &key, &comment);
    } else {
        refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
    }
    Ok(json!({ "ok": true, "site_id": target_site, "comment": comment }))
}

pub(super) async fn set_jira_parent(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let parent = args.required_str("parent_key")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let parent = client
        .set_issue_parent(&key, &parent)
        .await
        .map_err(format_error)?;
    refresh_mcp_issue_cache(&client, storage, &site, &key).await;
    Ok(json!({"ok": true, "site_id": site, "key": key, "parent_key": parent}))
}

pub(super) async fn update_jira_custom_fields(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let fields = args.custom_fields()?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let updated = client
        .edit_issue_custom_fields(&key, fields)
        .await
        .map_err(format_error)?;
    refresh_mcp_issue_cache(&client, storage, &site, &key).await;
    Ok(json!({"ok": true, "site_id": site, "key": key, "custom_fields": updated}))
}

pub(super) async fn create_jira_issue_link(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let inward = args.required_str("inward_key")?;
    let outward = args.required_str("outward_key")?;
    let link_type = args.required_str("link_type")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let link = client
        .create_issue_link(&inward, &outward, &link_type)
        .await
        .map_err(format_error)?;
    Box::pin(async {
        tokio::join!(
            refresh_mcp_issue_cache(&client, storage, &site, &inward),
            refresh_mcp_issue_cache(&client, storage, &site, &outward),
        );
    })
    .await;
    Ok(json!({"ok": true, "site_id": site, "link": link}))
}

pub(super) async fn move_jira_issues_to_sprint(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let sprint_id = args.required_i64("sprint_id")?;
    let keys = args.required_str_array("issue_keys")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let confirmed = client
        .move_issues_to_sprint(sprint_id, &keys)
        .await
        .map_err(format_error)?;
    Box::pin(
        stream::iter(&confirmed)
            .map(|key| refresh_mcp_issue_cache(&client, storage, &site, key))
            .buffer_unordered(4)
            .for_each(|()| async {}),
    )
    .await;
    Ok(
        json!({"ok": true, "site_id": site, "sprint_id": sprint_id, "issue_keys": confirmed}),
    )
}

pub(super) async fn add_jira_worklog(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let seconds = args.worklog_seconds()?;
    let started = args.optional_str("started")?;
    let comment = args.optional_text("comment_markdown")?;
    let (site, client) = resolve_mcp_client(config, client, site_arg.as_deref())?;
    let worklog = client
        .log_issue_work(&key, seconds, started.as_deref(), comment.as_deref())
        .await
        .map_err(format_error)?;
    Ok(json!({"ok": true, "site_id": site, "key": key, "worklog": worklog}))
}

pub(super) async fn upload_jira_attachment(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let path = args.required_str("path")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    let uploaded = target_client
        .upload_issue_attachment(&key, Path::new(&path))
        .await
        .map_err(format_error)?;
    refresh_mcp_issue_cache(&target_client, storage, &target_site, &key).await;
    Ok(json!({ "ok": true, "site_id": target_site, "key": key, "attachments": uploaded }))
}

pub(super) async fn delete_jira_issue(ctx: &McpContext<'_>) -> Result<Value, String> {
    let config = ctx.config;
    let client = ctx.client;
    let storage = ctx.storage;
    let args = &ctx.args;
    let site_arg = &ctx.site_arg;
    let key = args.required_str("key")?;
    let (target_site, target_client) =
        resolve_mcp_client(config, client, site_arg.as_deref())?;
    target_client
        .delete_issue(&key)
        .await
        .map_err(format_error)?;
    let _ = storage.delete_issue_for_site(&target_site, &key);
    Ok(json!({ "ok": true, "site_id": target_site, "key": key }))
}
