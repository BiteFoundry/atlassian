//! Jira CLI subcommand dispatcher and handlers.

use std::path::Path;

use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt};
use serde_json::{json, Map, Value};

use crate::{
    atlassian::{
        body::BodyEdit,
        client::is_not_found_error,
        AtlassianClient,
    },
    config::{self, AppConfig},
    img_preview,
    models::{parse_offset_cursor, slice_page, BoardSummary, IssueUpdate, OutgoingIssue},
    resolver::resolve_rejected_assignee,
    safe_text::sanitize_terminal_inline,
    serve,
    storage::{IssueListFilter, Storage},
    sync,
};

use super::{
    args::{EditBodyArgs, JiraSubcommand, JiraWorkflow},
    format::{
        delete_gate, format_kanban_board, print_edit_preview, print_issue_detail, prompt_line,
        read_body_arg, render_attachments_preview, select_attachment,
    },
    open_client, open_runtime,
};

pub(super) async fn dispatch_jira_subcommand(cfg: &AppConfig, command: JiraSubcommand) -> Result<()> {
    match command {
        JiraSubcommand::Projects {
            refresh,
            limit,
            cursor,
            json,
        } => Box::pin(cmd_projects(cfg, refresh, limit, cursor.as_deref(), json)).await,
        JiraSubcommand::Boards {
            project,
            refresh,
            limit,
            cursor,
            json,
        } => {
            Box::pin(cmd_boards(
                cfg,
                project.as_deref(),
                refresh,
                limit,
                cursor.as_deref(),
                json,
            ))
            .await
        }
        JiraSubcommand::Sprints {
            board_id,
            limit,
            cursor,
            json,
        } => Box::pin(cmd_sprints(cfg, board_id, limit, cursor.as_deref(), json)).await,
        JiraSubcommand::Board {
            board_id,
            sprint,
            json,
        } => Box::pin(cmd_board(cfg, board_id, sprint, json)).await,
        JiraSubcommand::Issues {
            project,
            jql,
            mine,
            status,
            limit,
            page_token,
            start_at,
            offline,
            json,
        } => {
            Box::pin(cmd_issues(
                cfg,
                project.as_deref(),
                jql.as_deref(),
                mine,
                status.as_deref(),
                limit,
                page_token.as_deref(),
                start_at,
                offline,
                json,
            ))
            .await
        }
        JiraSubcommand::Workflow(command) => Box::pin(dispatch_jira_workflow(cfg, command)).await,
        JiraSubcommand::View {
            key,
            preview_images,
            offline,
            json,
            compact: _,
            full,
            fields,
        } => {
            Box::pin(cmd_issue_view(
                cfg,
                &key,
                preview_images,
                offline,
                json,
                !full,
                &fields,
            ))
            .await
        }
        JiraSubcommand::Create {
            summary_pos,
            project,
            issue_type,
            summary,
            body,
            body_file,
            priority,
            assignee,
            label,
            parent,
            json,
            full,
        } => {
            let resolved_summary = summary.or(summary_pos).unwrap_or_default();
            Box::pin(cmd_issue_create(
                cfg,
                project.as_deref(),
                &issue_type,
                &resolved_summary,
                body.as_deref(),
                body_file.as_deref(),
                priority.as_deref(),
                assignee.as_deref(),
                label,
                parent.as_deref(),
                json,
                full,
            ))
            .await
        }
        JiraSubcommand::Edit {
            key,
            summary,
            body,
            priority,
            assignee,
            label,
            json,
            full,
        } => {
            Box::pin(cmd_issue_edit(
                cfg, &key, summary, body, priority, assignee, label, json, full,
            ))
            .await
        }
        JiraSubcommand::Transition {
            key,
            status,
            comment,
            list,
            json,
        } => {
            Box::pin(cmd_issue_transition(
                cfg,
                &key,
                status.as_deref(),
                comment.as_deref(),
                list,
                json,
            ))
            .await
        }
        JiraSubcommand::Assign {
            key,
            assignee_pos,
            to,
            unassign,
            json,
        } => {
            let resolved_to = to.or(assignee_pos);
            Box::pin(cmd_issue_assign(
                cfg,
                &key,
                resolved_to.as_deref(),
                unassign,
                json,
            ))
            .await
        }
        JiraSubcommand::Comment { key, body, json } => {
            Box::pin(cmd_issue_comment(cfg, &key, &body, json)).await
        }
        JiraSubcommand::Attach { key, file, json } => {
            Box::pin(cmd_issue_attach(cfg, &key, &file, json)).await
        }
        JiraSubcommand::Download {
            key,
            attachment_id,
            dir,
            json,
        } => {
            Box::pin(cmd_issue_download(
                cfg,
                &key,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        JiraSubcommand::Delete { key, yes, json } => {
            Box::pin(cmd_issue_delete(cfg, &key, yes, json)).await
        }
        JiraSubcommand::Search {
            query,
            jql,
            remote,
            limit,
            page_token,
            start_at,
            json,
        } => {
            Box::pin(cmd_issue_search(
                cfg,
                &query,
                jql,
                remote,
                limit,
                page_token.as_deref(),
                start_at,
                json,
            ))
            .await
        }
        JiraSubcommand::Open { key } => Box::pin(cmd_issue_open(cfg, &key)).await,
    }
}

async fn refresh_cli_issue_cache(client: &AtlassianClient, storage: &Storage, key: &str) {
    let site_id = client.site_id();
    if let Ok((mut detail, meta)) = client.get_issue_with_meta(key).await {
        let _ = storage.upsert_issue_detail_for_site_merging(
            site_id,
            &mut detail,
            !meta.remote_links_ok,
        );
    }
}


async fn cmd_projects(
    cfg: &AppConfig,
    refresh: bool,
    limit: Option<usize>,
    cursor: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let offset = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
    let eff_limit = limit
        .map(serve::clamp_limit)
        .unwrap_or(if json_out { 50 } else { usize::MAX });
    let (auth, client, storage) = open_runtime(cfg, refresh)?;
    let site_id = client.site_id();
    let cached = storage.list_projects_for_site(site_id)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && !auth.is_logged_in() {
            cached
        } else {
            let fetched = client.list_projects().await?;
            let _ = storage.upsert_projects_for_site(site_id, &fetched);
            let stored = storage.list_projects_for_site(site_id)?;
            if stored.is_empty() {
                fetched
            } else {
                stored
            }
        }
    } else {
        cached
    };
    let page = slice_page(all, offset, eff_limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "projects": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No projects found.");
    } else {
        for p in &page.items {
            println!(
                "{:<10} {:<28} [{}] lead={}",
                sanitize_terminal_inline(&p.key),
                sanitize_terminal_inline(&p.name),
                sanitize_terminal_inline(&p.project_type),
                sanitize_terminal_inline(p.lead_name.as_deref().unwrap_or("-"))
            );
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_boards(
    cfg: &AppConfig,
    project: Option<&str>,
    refresh: bool,
    limit: Option<usize>,
    cursor: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let offset = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
    let eff_limit = limit
        .map(serve::clamp_limit)
        .unwrap_or(if json_out { 50 } else { usize::MAX });
    let (auth, client, storage) = open_runtime(cfg, refresh)?;
    let site_id = client.site_id();
    let cached = storage.list_boards_for_site(site_id, project)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && !auth.is_logged_in() {
            cached
        } else {
            let fetched = client.list_boards(project).await?;
            let _ = storage.upsert_boards_for_site(site_id, &fetched);
            let stored = storage.list_boards_for_site(site_id, project)?;
            if stored.is_empty() {
                fetched
            } else {
                stored
            }
        }
    } else {
        cached
    };
    let page = slice_page(all, offset, eff_limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "boards": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No boards found.");
    } else {
        for b in &page.items {
            println!(
                "#{:<6} {:<32} ({}) project={}",
                b.id,
                sanitize_terminal_inline(&b.name),
                sanitize_terminal_inline(&b.board_type),
                sanitize_terminal_inline(b.project_key.as_deref().unwrap_or("-"))
            );
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_sprints(
    cfg: &AppConfig,
    board_id: i64,
    limit: usize,
    cursor: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let start = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let page = client
        .list_board_sprints_page(board_id, serve::clamp_limit(limit), start)
        .await?;
    let _ = storage.upsert_sprints_for_site(site_id, &page.items);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "board_id": board_id,
                "sprints": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No sprints found for board #{board_id}.");
    } else {
        for s in &page.items {
            println!(
                "#{:<6} [{}] {:<28} {} ~ {}",
                s.id,
                sanitize_terminal_inline(&s.state),
                sanitize_terminal_inline(&s.name),
                sanitize_terminal_inline(s.start_date.as_deref().unwrap_or("-")),
                sanitize_terminal_inline(s.end_date.as_deref().unwrap_or("-"))
            );
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_board(
    cfg: &AppConfig,
    board_id: Option<i64>,
    sprint_id: Option<i64>,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();

    let mut boards = storage.list_boards_for_site(site_id, cfg.default_project.as_deref())?;
    if boards.is_empty() {
        boards = client.list_boards(cfg.default_project.as_deref()).await?;
        let _ = storage.upsert_boards_for_site(site_id, &boards);
    }

    let mut board = match board_id.or(cfg.default_board_id) {
        Some(id) => boards
            .into_iter()
            .find(|b| b.id == id)
            .unwrap_or_else(|| BoardSummary {
                id,
                name: format!("Board #{id}"),
                board_type: "kanban".to_string(),
                ..Default::default()
            }),
        None => boards
            .into_iter()
            .next()
            .context("No available Jira boards found. Specify `board <ID>`.")?,
    };

    let need_cols = board.columns.is_empty();
    let (cols_opt, sprints, issues) = if let Some(sid) = sprint_id {
        let (cols_res, sprints_res, issues_res) = tokio::join!(
            async {
                if need_cols {
                    client.get_board_columns(board.id).await.ok()
                } else {
                    None
                }
            },
            client.list_board_sprints(board.id),
            sync::load_board_issues_for_site(&client, &storage, site_id, board.id, Some(sid)),
        );
        let sprints = sprints_res.unwrap_or_default();
        let _ = storage.upsert_sprints_for_site(site_id, &sprints);
        (cols_res, sprints, issues_res?)
    } else {
        let (cols_res, sprints_res) = tokio::join!(
            async {
                if need_cols {
                    client.get_board_columns(board.id).await.ok()
                } else {
                    None
                }
            },
            client.list_board_sprints(board.id),
        );
        let sprints = sprints_res.unwrap_or_default();
        let _ = storage.upsert_sprints_for_site(site_id, &sprints);
        let active_id = sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .map(|s| s.id);
        let issues =
            sync::load_board_issues_for_site(&client, &storage, site_id, board.id, active_id)
                .await?;
        (cols_res, sprints, issues)
    };
    if let Some(cols) = cols_opt {
        board.columns = cols;
    }

    let active_sprint = if let Some(sid) = sprint_id {
        sprints.iter().find(|s| s.id == sid).cloned()
    } else {
        sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .cloned()
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "board": board,
                "sprint": active_sprint,
                "issues": issues,
            }))?
        );
    } else {
        print!(
            "{}",
            format_kanban_board(&board, active_sprint.as_ref(), &issues)
        );
    }
    Ok(())
}

pub(super) const OFFLINE_JQL_ERROR: &str =
    "--jql cannot be evaluated offline; use --project/--status/--mine or \"atlassian search <text>\"";
pub(super) const OFFLINE_MINE_ERROR: &str =
    "--mine --offline needs a cached identity; run \"atlassian whoami\" once online";

/// The cache filter for `issues --offline`: project (or the default project), status and, for
/// `--mine`, the signed-in user `whoami` saved. JQL needs a server, so it is refused.
pub(super) fn offline_issue_filter<'a>(
    cfg: &'a AppConfig,
    project: Option<&'a str>,
    jql: Option<&str>,
    mine: bool,
    status: Option<&'a str>,
    me: Option<&'a str>,
) -> Result<IssueListFilter<'a>> {
    if jql.is_some_and(|raw| !raw.trim().is_empty()) {
        bail!(OFFLINE_JQL_ERROR);
    }
    let nonempty = |s: &'a str| Some(s.trim()).filter(|s| !s.is_empty());
    let assignee_id = if mine {
        Some(me.and_then(nonempty).context(OFFLINE_MINE_ERROR)?)
    } else {
        None
    };
    Ok(IssueListFilter {
        project: project.or(cfg.default_project.as_deref()).and_then(nonempty),
        status_contains: status.and_then(nonempty),
        assignee_id,
        open_only: false,
    })
}

#[allow(clippy::too_many_arguments)]
async fn cmd_issues(
    cfg: &AppConfig,
    project: Option<&str>,
    jql: Option<&str>,
    mine: bool,
    status: Option<&str>,
    limit: usize,
    page_token: Option<&str>,
    start_at: Option<usize>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();
    let limit = serve::clamp_limit(limit);

    let (issues, remote_page) = if offline {
        let me = if mine {
            client
                .cached_me()
                .or_else(|| storage.identity_for_site(site_id).ok().flatten())
                .map(|user| user.id)
        } else {
            None
        };
        let filter = offline_issue_filter(cfg, project, jql, mine, status, me.as_deref())?;
        (storage.list_issues_filtered_for_site(site_id, &filter, limit)?, None)
    } else {
        let effective_jql = if let Some(raw) = jql {
            raw.to_string()
        } else {
            let mut clauses = Vec::new();
            if let Some(p) = project.or(cfg.default_project.as_deref()) {
                if !p.trim().is_empty() {
                    clauses.push(format!("project = \"{}\"", serve::escape_jql_str(p.trim())));
                }
            }
            if mine {
                clauses.push("assignee = currentUser()".to_string());
            }
            if let Some(st) = status {
                clauses.push(format!("status = \"{}\"", serve::escape_jql_str(st.trim())));
            }
            if clauses.is_empty() {
                AtlassianClient::my_open_issues_jql().to_string()
            } else {
                format!("{} ORDER BY updated DESC", clauses.join(" AND "))
            }
        };
        let page = client
            .search_issues_jql(&effective_jql, limit, page_token, start_at)
            .await?;
        let _ = storage.upsert_issues_for_site(site_id, &page.issues);
        let issues = page.issues.clone();
        (issues, Some(page))
    };

    if json_out {
        if let Some(page) = remote_page {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "issues": page.issues,
                    "next_page_token": page.next_page_token,
                    "next_start_at": page.next_start_at,
                    "total": page.total,
                }))?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&json!({ "issues": issues }))?);
        }
    } else if issues.is_empty() {
        println!("No issues found.");
    } else {
        for issue in &issues {
            let assignee = issue
                .assignee
                .as_ref()
                .map(|u| u.display_name.as_str())
                .unwrap_or("Unassigned");
            let prio = issue
                .priority
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("-");
            println!(
                "{:<12} [{:<12}] ({:<6}) {:<12} {}",
                sanitize_terminal_inline(&issue.key),
                sanitize_terminal_inline(&issue.status.name),
                sanitize_terminal_inline(prio),
                sanitize_terminal_inline(assignee),
                sanitize_terminal_inline(&issue.summary)
            );
        }
        if let Some(page) = remote_page {
            if let Some(tok) = page.next_page_token.as_deref() {
                eprintln!("Next page: --page-token {tok}");
            } else if let Some(next_start) = page.next_start_at {
                eprintln!("Next page: --start-at {next_start}");
            }
        }
    }
    Ok(())
}

async fn cmd_issue_view(
    cfg: &AppConfig,
    key: &str,
    preview_images: bool,
    offline: bool,
    json_out: bool,
    compact: bool,
    fields: &[String],
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();
    let (detail, detail_stale) = if offline {
        let d = storage
            .get_issue_detail_for_site(site_id, key)?
            .with_context(|| format!("Issue '{key}' not found in offline cache."))?;
        let stale = storage
            .is_issue_detail_stale_for_site(site_id, key)
            .unwrap_or(false);
        (d, stale)
    } else {
        let (mut fetched, meta) = match client.get_issue_with_meta(key).await {
            Ok(res) => res,
            Err(err) => {
                if is_not_found_error(&err) {
                    let _ = storage.delete_issue_for_site(site_id, key);
                }
                return Err(err);
            }
        };
        let _ = storage.upsert_issue_detail_for_site_merging(
            site_id,
            &mut fetched,
            !meta.remote_links_ok,
        );
        (fetched, false)
    };

    if json_out {
        let body = if !fields.is_empty() {
            detail.projected_output(fields)
        } else if compact {
            let mut hint = format!("atlassian view {key} --json --fields <field> (or --full)");
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
                "issue": body,
            }))?
        );
        return Ok(());
    }

    print_issue_detail(&detail);
    if detail_stale {
        println!("\n[Note] Cached issue details are from an earlier revision. Fetch online to refresh.");
    }

    if preview_images && !offline {
        render_attachments_preview(&client, &detail.attachments, None, None).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_issue_create(
    cfg: &AppConfig,
    project: Option<&str>,
    issue_type: &str,
    summary: &str,
    body: Option<&str>,
    body_file: Option<&Path>,
    priority: Option<&str>,
    assignee: Option<&str>,
    labels: Vec<String>,
    parent: Option<&str>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let project_key = project
        .or(cfg.default_project.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Project key is required (`--project <KEY>` or config `default_project`).")?;
    let description_markdown = read_body_arg(body, body_file)?.unwrap_or_default();

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let (assignee_id, raw_assignee) = match assignee.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("me") => (Some(client.get_myself().await?.id), None),
        Some(v) if !v.is_empty() => (Some(v.to_string()), Some(v.to_string())),
        _ => (None, None),
    };

    let mut req = OutgoingIssue {
        project_key: project_key.to_string(),
        issue_type: issue_type.to_string(),
        summary: summary.to_string(),
        description_markdown,
        priority: priority.map(str::to_string),
        assignee_id,
        labels,
        parent_key: parent.map(str::to_string),
    };
    let mut resolved_assignee = None;
    let created = match client.create_issue(&req).await {
        Ok(created) => created,
        Err(err) => {
            if let Some(raw) = raw_assignee.as_deref() {
                match resolve_rejected_assignee(&client, project_key, raw, &err, true).await {
                    Some(Ok(user)) => {
                        req.assignee_id = Some(user.id.clone());
                        resolved_assignee = Some(user);
                        client.create_issue(&req).await?
                    }
                    Some(Err(ambig)) => return Err(ambig),
                    None => return Err(err),
                }
            } else {
                return Err(err);
            }
        }
    };
    let _ = storage.upsert_issue_detail_for_site(site_id, &created);

    if json_out {
        let mut payload = json!({
            "ok": true,
            "site_id": site_id,
            "issue": if full { created.output(false) } else { created.write_output() },
        });
        if let Some(user) = resolved_assignee {
            payload["resolved_assignee"] = json!({
                "id": user.id,
                "display_name": user.display_name,
            });
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Issue created: {} — {} ({})",
            created.summary.key, created.summary.summary, created.summary.web_url
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_jira_workflow(cfg: &AppConfig, command: JiraWorkflow) -> Result<()> {
    let (_, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let (result, json_out, message) = match command {
        JiraWorkflow::Parent {
            key,
            parent_key,
            json,
        } => {
            let parent = client.set_issue_parent(&key, &parent_key).await?;
            refresh_cli_issue_cache(&client, &storage, &key).await;
            (
                json!({"ok": true, "site_id": site_id, "key": key, "parent_key": parent}),
                json,
                format!("{key}: set parent to {parent}"),
            )
        }
        JiraWorkflow::CustomFields {
            key,
            fields_json,
            fields_file,
            json,
        } => {
            let text = read_body_arg(fields_json.as_deref(), fields_file.as_deref())?
                .context("provide --fields-json or --fields-file")?;
            let fields: Map<String, Value> =
                serde_json::from_str(&text).context("custom fields must be a JSON object")?;
            let updated = client.edit_issue_custom_fields(&key, &fields).await?;
            refresh_cli_issue_cache(&client, &storage, &key).await;
            (
                json!({"ok": true, "site_id": site_id, "key": key, "custom_fields": updated}),
                json,
                format!("{key}: updated {} custom field(s)", updated.len()),
            )
        }
        JiraWorkflow::Link {
            inward_key,
            outward_key,
            link_type,
            json,
        } => {
            let link = client
                .create_issue_link(&inward_key, &outward_key, &link_type)
                .await?;
            tokio::join!(
                refresh_cli_issue_cache(&client, &storage, &inward_key),
                refresh_cli_issue_cache(&client, &storage, &outward_key),
            );
            (
                json!({"ok": true, "site_id": site_id, "link": link}),
                json,
                format!(
                    "{inward_key} ↔ {outward_key}: link created ({})",
                    link["id"]
                ),
            )
        }
        JiraWorkflow::MoveToSprint {
            sprint_id,
            keys,
            json,
        } => {
            let confirmed = client.move_issues_to_sprint(sprint_id, &keys).await?;
            stream::iter(&confirmed)
                .map(|key| refresh_cli_issue_cache(&client, &storage, key))
                .buffer_unordered(4)
                .for_each(|()| async {})
                .await;
            (
                json!({"ok": true, "site_id": site_id, "sprint_id": sprint_id, "issue_keys": confirmed}),
                json,
                format!("Sprint {sprint_id}: moved {} issue(s)", confirmed.len()),
            )
        }
        JiraWorkflow::LogWork {
            key,
            seconds,
            started,
            comment,
            json,
        } => {
            let log = client
                .log_issue_work(&key, seconds, started.as_deref(), comment.as_deref())
                .await?;
            (
                json!({"ok": true, "site_id": site_id, "key": key, "worklog": log}),
                json,
                format!("{key}: logged {seconds}s of work ({})", log["id"]),
            )
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
async fn cmd_issue_edit(
    cfg: &AppConfig,
    key: &str,
    summary: Option<String>,
    body: EditBodyArgs,
    priority: Option<String>,
    assignee: Option<String>,
    labels: Option<Vec<String>>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let dry_run = body.dry_run;
    let (full_body, append, replacement) = body.read()?;
    let edit = BodyEdit::from_inputs(
        full_body.as_deref(),
        append.as_deref(),
        replacement.as_ref(),
        None,
    )?;
    if summary.is_none()
        && edit.is_none()
        && priority.is_none()
        && assignee.is_none()
        && labels.is_none()
    {
        bail!("Specify at least one field to update (`--summary`, `--body`, `--append`, `--replace`, etc.).");
    }

    let (_auth, client) = open_client(cfg, true)?;
    let site_id = client.site_id();
    let (assignee_id, raw_assignee) = match assignee.as_deref().map(str::trim) {
        None => (None, None),
        Some("") | Some("none") | Some("unassign") => (Some(None), None),
        Some(v) if v.eq_ignore_ascii_case("me") => {
            (Some(Some(client.get_myself().await?.id)), None)
        }
        Some(v) => (Some(Some(v.to_string())), Some(v.to_string())),
    };

    let mut patch = IssueUpdate {
        summary,
        description_markdown: None,
        priority,
        assignee_id,
        labels,
    };
    if dry_run {
        let preview = client.preview_issue_edit(key, &patch, edit).await?;
        return print_edit_preview(site_id, preview);
    }
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let mut resolved_assignee = None;
    let updated = match client.edit_issue(key, &patch, edit).await {
        Ok(updated) => updated,
        Err(err) => {
            if let Some(raw) = raw_assignee.as_deref() {
                match resolve_rejected_assignee(&client, key, raw, &err, true).await {
                    Some(Ok(user)) => {
                        patch.assignee_id = Some(Some(user.id.clone()));
                        resolved_assignee = Some(user);
                        client.edit_issue(key, &patch, edit).await?
                    }
                    Some(Err(ambig)) => return Err(ambig),
                    None => return Err(err),
                }
            } else {
                return Err(err);
            }
        }
    };
    let _ = storage.upsert_issue_detail_for_site(site_id, &updated);

    if json_out {
        let mut payload = json!({
            "ok": true,
            "site_id": site_id,
            "issue": if full { updated.output(false) } else { updated.write_output() },
        });
        if let Some(user) = resolved_assignee {
            payload["resolved_assignee"] = json!({
                "id": user.id,
                "display_name": user.display_name,
            });
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Issue updated: {} — {}",
            updated.summary.key, updated.summary.summary
        );
    }
    Ok(())
}

async fn cmd_issue_transition(
    cfg: &AppConfig,
    key: &str,
    status: Option<&str>,
    comment: Option<&str>,
    list: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();

    let Some(target) = status.filter(|_| !list) else {
        let transitions = client.list_transitions(key).await?;
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "key": key, "transitions": transitions }))?
            );
        } else if transitions.is_empty() {
            println!("{key}: no available transitions.");
        } else {
            println!("{key} available transitions:");
            for t in transitions {
                println!("  [{}] {} -> {}", t.id, t.name, t.to_status.name);
            }
        }
        return Ok(());
    };

    let executed = client.transition_issue(key, target, comment).await?;
    refresh_cli_issue_cache(&client, &storage, key).await;

    if json_out {
        let transition_id = executed
            .as_ref()
            .map(|t| t.id.as_str())
            .unwrap_or_else(|| target.trim());
        let mut payload = json!({
            "ok": true,
            "site_id": site_id,
            "key": key,
            "transition": target,
            "transition_id": transition_id,
        });
        if let Some(tr) = executed {
            payload["to_status"] = json!(tr.to_status.name);
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("{key}: transitioned to '{target}'.");
    }
    Ok(())
}

async fn cmd_issue_assign(
    cfg: &AppConfig,
    key: &str,
    to: Option<&str>,
    unassign: bool,
    json_out: bool,
) -> Result<()> {
    if !unassign && to.is_none() {
        bail!("Specify `--to <USER_OR_ME>` or `--unassign`.");
    }
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let (mut target_id, raw_assignee) = if unassign {
        (None, None)
    } else {
        let Some(raw) = to.map(str::trim) else {
            bail!("Specify `--to <USER_OR_ME>` or `--unassign`.");
        };
        if raw.eq_ignore_ascii_case("me") {
            (Some(client.get_myself().await?.id), None)
        } else {
            (
                Some(raw.to_string()),
                (!raw.is_empty()).then(|| raw.to_string()),
            )
        }
    };

    let mut resolved_assignee = None;
    if let Err(err) = client.assign_issue(key, target_id.as_deref()).await {
        if let Some(raw) = raw_assignee.as_deref() {
            match resolve_rejected_assignee(&client, key, raw, &err, false).await {
                Some(Ok(user)) => {
                    target_id = Some(user.id.clone());
                    resolved_assignee = Some(user);
                    client.assign_issue(key, target_id.as_deref()).await?;
                }
                Some(Err(ambig)) => return Err(ambig),
                None => return Err(err),
            }
        } else {
            return Err(err);
        }
    }
    refresh_cli_issue_cache(&client, &storage, key).await;

    if json_out {
        let mut payload = json!({
            "ok": true,
            "site_id": site_id,
            "key": key,
            "assignee_id": target_id,
        });
        if let Some(user) = resolved_assignee {
            payload["resolved_assignee"] = json!({
                "id": user.id,
                "display_name": user.display_name,
            });
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if let Some(user) = resolved_assignee {
        println!(
            "{key}: assigned to '{}' ({}).",
            user.display_name, user.id
        );
    } else if let Some(uid) = target_id {
        println!("{key}: assigned to '{uid}'.");
    } else {
        println!("{key}: unassigned.");
    }
    Ok(())
}

async fn cmd_issue_comment(cfg: &AppConfig, key: &str, body: &str, json_out: bool) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let comment = client.add_issue_comment(key, body).await?;
    let site_id = client.site_id();
    if storage
        .get_issue_detail_for_site(site_id, key)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = storage.append_issue_comment_for_site(site_id, key, &comment);
    } else {
        refresh_cli_issue_cache(&client, &storage, key).await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "comment": comment }))?
        );
    } else {
        println!("{key}: comment added (id={}).", comment.id);
    }
    Ok(())
}

async fn cmd_issue_attach(cfg: &AppConfig, key: &str, file: &Path, json_out: bool) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let uploaded = client.upload_issue_attachment(key, file).await?;
    let site_id = client.site_id();
    if !uploaded.is_empty()
        && storage
            .get_issue_detail_for_site(site_id, key)
            .ok()
            .flatten()
            .is_some()
    {
        let _ = storage.append_issue_attachments_for_site(site_id, key, &uploaded);
    } else {
        refresh_cli_issue_cache(&client, &storage, key).await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "key": key, "attachments": uploaded }))?
        );
    } else {
        println!("{key}: attached '{}' ({} file(s)).", file.display(), uploaded.len());
    }
    Ok(())
}

async fn cmd_issue_download(
    cfg: &AppConfig,
    key: &str,
    attachment_id: Option<&str>,
    dir: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let attachments = client.list_issue_attachments(key).await?;
    let attachment = select_attachment(&attachments, attachment_id)
        .with_context(|| format!("No attachment found to download on issue {key}."))?;
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

async fn cmd_issue_delete(cfg: &AppConfig, key: &str, yes: bool, json_out: bool) -> Result<()> {
    let answer = if !yes && !json_out {
        Some(prompt_line(&format!(
            "Delete Jira issue '{key}'? [y/N]: "
        ))?)
    } else {
        None
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    client.delete_issue(key).await?;
    let _ = storage.delete_issue_for_site(site_id, key);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "site_id": site_id, "key": key }))?
        );
    } else {
        println!("Deleted Jira issue '{key}'.");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_issue_search(
    cfg: &AppConfig,
    query: &str,
    jql: bool,
    remote: bool,
    limit: usize,
    page_token: Option<&str>,
    start_at: Option<usize>,
    json_out: bool,
) -> Result<()> {
    let limit = serve::clamp_limit(limit);
    let use_remote = remote || jql;
    let clean_token = page_token.map(str::trim).filter(|s| !s.is_empty());
    if !use_remote && (clean_token.is_some() || start_at.is_some()) {
        bail!("--page-token and --start-at require --remote or --jql");
    }
    let (_auth, client, storage) = open_runtime(cfg, use_remote)?;
    let site_id = client.site_id();

    if use_remote {
        let effective_jql = if jql || serve::looks_like_jql_expr(query) {
            query.to_string()
        } else {
            format!(
                "text ~ \"{}\" ORDER BY updated DESC",
                serve::escape_jql_str(query)
            )
        };
        let page = client
            .search_issues_jql(&effective_jql, limit, clean_token, start_at)
            .await?;
        let _ = storage.upsert_issues_for_site(site_id, &page.issues);

        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": true,
                    "site_id": site_id,
                    "jql": effective_jql,
                    "issues": page.issues,
                    "next_page_token": page.next_page_token,
                    "next_start_at": page.next_start_at,
                    "total": page.total,
                }))?
            );
        } else if page.issues.is_empty() {
            println!("No results found.");
        } else {
            for i in &page.issues {
                println!("{:<12} [{:<12}] {}", i.key, i.status.name, i.summary);
            }
            if let Some(tok) = page.next_page_token.as_deref() {
                eprintln!("Next page: --page-token {tok}");
            } else if let Some(next_start) = page.next_start_at {
                eprintln!("Next page: --start-at {next_start}");
            }
        }
    } else {
        let scope_site = if cfg.active_site.is_some() || cfg.effective_sites().len() <= 1 {
            Some(site_id)
        } else {
            None
        };
        let hits: Vec<_> = storage
            .search_issues_scoped(query, scope_site, limit)?
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
            println!("No results in local cache. (Use `--remote` to search Jira server)");
        } else {
            for h in &hits {
                println!(
                    "{:<12} [{:<12}] {} — {}",
                    h.id_or_key, h.status_or_version, h.title, h.snippet
                );
            }
        }
    }
    Ok(())
}

async fn cmd_issue_open(cfg: &AppConfig, key: &str) -> Result<()> {
    let (auth, client, storage) = open_runtime(cfg, false)?;
    let site_id = client.site_id();
    let url = if let Ok(Some(detail)) = storage.get_issue_detail_for_site(site_id, key) {
        detail.summary.web_url
    } else if auth.is_logged_in() {
        client.get_issue(key).await?.summary.web_url
    } else if !cfg.site_url.trim().is_empty() {
        format!("{}/browse/{key}", cfg.jira_base_url())
    } else {
        bail!("Cannot build web URL. Run `atlassian login` first.");
    };
    open::that(&url).with_context(|| format!("Failed to open browser: {url}"))?;
    println!("Opened in browser: {url}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Confluence commands
// ---------------------------------------------------------------------------


