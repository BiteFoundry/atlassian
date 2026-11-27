//! Shared operational commands (`tui`, `preview`, `sync`, `daemon`, `serve`, `mcp`, `update`) and shared dispatcher.

use std::{
    io::{self, IsTerminal},
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    atlassian::SiteClientPool,
    auth::AuthManager,
    config::{self, AppConfig, Persona},
    daemon, img_preview,
    serve::{self, ServeOptions},
    shell_alias,
    storage::Storage,
    sync::{self, SiteSyncTarget},
    tui, updater,
};

use super::{
    apply_site_override,
    args::{DaemonCommand, SharedCommand},
    format::{looks_like_jira_key, select_attachment},
    login::{cmd_login, cmd_logout, cmd_whoami, LoginParams},
    open_client, open_runtime,
    sites::{cmd_doctor, cmd_sites},
    JsonReported,
};

pub(super) async fn dispatch_shared_command(
    cfg: &mut AppConfig,
    persona: Persona,
    site_override: Option<String>,
    command: SharedCommand,
) -> Result<()> {
    match command {
        SharedCommand::Login {
            url,
            confluence_url,
            email,
            token,
            pat,
            confluence_pat,
            confluence_account,
            deployment,
            oauth,
            cloud_account,
            client_id,
            client_secret,
            account,
            name,
            json,
        } => {
            Box::pin(cmd_login(
                cfg,
                site_override.as_deref(),
                LoginParams {
                    url,
                    confluence_url,
                    email,
                    token,
                    pat,
                    confluence_pat,
                    confluence_account,
                    deployment: deployment.map(Into::into),
                    oauth,
                    cloud_account,
                    client_id,
                    client_secret,
                    account,
                    name,
                },
                json,
            ))
            .await
        }
        SharedCommand::Sites { action } => Box::pin(cmd_sites(cfg, *action)).await,
        SharedCommand::Logout { json } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            cmd_logout(&scoped, json)
        }
        SharedCommand::Whoami { json } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_whoami(&scoped, persona, json)).await
        }
        SharedCommand::Doctor { probe, json } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_doctor(&scoped, persona, probe, json)).await
        }
        SharedCommand::Preview {
            target_id_or_key,
            attachment_id,
            cols,
            rows,
        } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_preview(
                &scoped,
                &target_id_or_key,
                attachment_id.as_deref(),
                cols,
                rows,
            ))
            .await
        }
        SharedCommand::PreviewFile { path, cols, rows } => cmd_preview_file(&path, cols, rows),
        SharedCommand::Sync { json } => {
            Box::pin(cmd_sync(cfg, site_override.as_deref(), json)).await
        }
        SharedCommand::Daemon { action } => Box::pin(cmd_daemon(cfg, action)).await,
        SharedCommand::Alias { action } => shell_alias::cmd_alias(action),
        SharedCommand::Init {
            shell,
            jira,
            conf,
            force,
        } => shell_alias::cmd_init(shell, &jira, &conf, force),
        SharedCommand::Update {
            check,
            force,
            repo,
            json,
        } => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        SharedCommand::Tui => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_tui(&mut run_cfg, persona)).await
        }
        SharedCommand::Serve {
            bind,
            port,
            token,
            cors_origin,
        } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            let (_auth, client, storage) = open_runtime(&scoped, false)?;
            Box::pin(serve::run(
                scoped,
                client,
                storage,
                ServeOptions {
                    bind,
                    port,
                    token,
                    cors_origins: cors_origin,
                },
            ))
            .await
        }
        SharedCommand::Mcp { read_only } => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            let (_auth, client) = open_client(&scoped, false)?;
            Box::pin(crate::mcp::run_deferred(
                scoped,
                client,
                crate::mcp::McpOptions { read_only },
            ))
            .await
        }
    }
}

pub(super) async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let has_any_site = !cfg.effective_sites().is_empty();
    let has_any_auth = if has_any_site {
        SiteClientPool::from_config(cfg)
            .ok()
            .map(|pool| {
                pool.enabled_clients()
                    .iter()
                    .any(|(_, c)| c.auth().is_logged_in())
            })
            .unwrap_or(false)
            || AuthManager::new(&cfg.profile).is_logged_in()
    } else {
        AuthManager::new(&cfg.profile).is_logged_in()
    };

    if !has_any_auth || !has_any_site {
        let bin = persona.command_name();
        println!("No saved Atlassian credentials found.");
        println!("Quick login: {bin} login --oauth  or  {bin} login --url https://<site>.atlassian.net --email <email>");
        if !io::stdin().is_terminal() {
            bail!("Non-interactive terminal. Log in first with `{bin} login --url ... --email ... --token ...`.");
        }
        println!();
        cmd_login(
            cfg,
            None,
            LoginParams {
                url: None,
                confluence_url: None,
                email: None,
                token: None,
                pat: None,
                confluence_pat: None,
                confluence_account: None,
                deployment: None,
                oauth: false,
                cloud_account: false,
                client_id: None,
                client_secret: None,
                account: None,
                name: None,
            },
            false,
        )
        .await?;
        println!();
    }

    let (auth, client, storage) = open_runtime(cfg, true)?;
    tui::run_with_persona(cfg.clone(), persona, auth, client, storage).await
}


pub(super) async fn cmd_preview(
    cfg: &AppConfig,
    target_id_or_key: &str,
    attachment_id: Option<&str>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let attachments = if looks_like_jira_key(target_id_or_key) {
        client.list_issue_attachments(target_id_or_key).await?
    } else {
        client.list_page_attachments(target_id_or_key).await?
    };

    let selected = if let Some(aid) = attachment_id {
        select_attachment(&attachments, Some(aid))
    } else {
        attachments
            .iter()
            .find(|a| img_preview::is_image_attachment(&a.mime_type, &a.filename))
    }
    .with_context(|| format!("No image attachment found in '{target_id_or_key}'."))?;

    let bytes = client.fetch_attachment_bytes(selected).await?;
    let rendered = img_preview::render_for_stdout(
        &bytes,
        cols.unwrap_or(80),
        rows.unwrap_or(24),
    )?;
    println!("=== {} ({}) ===", selected.filename, selected.mime_type);
    println!("{rendered}");
    Ok(())
}

pub(super) fn cmd_preview_file(path: &Path, cols: Option<u16>, rows: Option<u16>) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read image file: {}", path.display()))?;
    let rendered = img_preview::render_for_stdout(
        &bytes,
        cols.unwrap_or(80),
        rows.unwrap_or(24),
    )?;
    println!("{rendered}");
    Ok(())
}

pub(super) async fn cmd_sync(cfg: &AppConfig, site_filter: Option<&str>, json_out: bool) -> Result<()> {
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let pool = SiteClientPool::from_config(cfg)?;
    let targets: Vec<SiteSyncTarget> = if let Some(filter) =
        site_filter.map(str::trim).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
    {
        let site = cfg
            .find_effective_site(filter)
            .ok_or_else(|| anyhow::anyhow!("Site '{filter}' not found."))?;
        let client = pool.client_for(&site.id)?;
        vec![SiteSyncTarget {
            site_id: site.id.clone(),
            site_name: site.display_name().to_string(),
            client,
        }]
    } else {
        pool.enabled_clients()
            .into_iter()
            .map(|(site, client)| SiteSyncTarget {
                site_id: site.id.clone(),
                site_name: site.display_name().to_string(),
                client,
            })
            .collect()
    };

    if targets.len() > 1 || (!cfg.sites.is_empty() && !targets.is_empty()) {
        let mut breakers = sync::CircuitBreakerMap::new();
        let outcome = sync::run_multi_site_sync(
            &targets,
            &storage,
            &mut breakers,
            sync::DEFAULT_MULTI_SITE_CONCURRENCY,
            None,
        )
        .await;
        let counts = storage.cache_counts()?;
        let all_failed = outcome.total_sites > 0 && outcome.succeeded_sites == 0;
        let all_failed_msg = if all_failed {
            let reasons: Vec<String> = outcome
                .per_site
                .iter()
                .map(|s| {
                    let reason = s.error.as_deref().unwrap_or(s.summary.as_str());
                    format!("{}: {reason}", s.site_id)
                })
                .collect();
            Some(format!(
                "Sync failed for all sites ({}): {}",
                outcome.summary,
                reasons.join("; ")
            ))
        } else {
            None
        };
        if json_out {
            let mut payload = json!({
                "ok": outcome.succeeded_sites > 0,
                "summary": outcome.summary,
                "sites": outcome.per_site,
                "counts": counts,
            });
            if let Some(ref msg) = all_failed_msg {
                payload["error"] = json!({
                    "code": "sync_failed",
                    "message": msg,
                });
            }
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            if all_failed {
                println!("Sync failed: {}", outcome.summary);
            } else {
                println!("Sync complete: {}", outcome.summary);
            }
            for s in &outcome.per_site {
                if let Some(ref err) = s.error {
                    println!("  - {} ({}): FAILED - {err}", s.site_id, s.site_url);
                } else {
                    println!("  - {} ({}): {}", s.site_id, s.site_url, s.summary);
                }
            }
            println!(
                "Cache counts: projects={}, boards={}, issues={}, spaces={}, pages={}",
                counts.projects, counts.boards, counts.issues, counts.spaces, counts.pages
            );
        }
        if let Some(msg) = all_failed_msg {
            if json_out {
                return Err(JsonReported(msg).into());
            }
            bail!("{msg}");
        }
        return Ok(());
    }

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let summary = sync::bootstrap_workspace(&client, &storage).await?;
    let counts = storage.cache_counts()?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "summary": summary,
                "counts": counts,
            }))?
        );
    } else {
        println!("Sync complete: {summary}");
        println!(
            "Cache counts: projects={}, boards={}, issues={}, spaces={}, pages={}",
            counts.projects, counts.boards, counts.issues, counts.spaces, counts.pages
        );
    }
    Ok(())
}

pub(super) async fn cmd_daemon(cfg: &AppConfig, action: DaemonCommand) -> Result<()> {
    match action {
        DaemonCommand::Start { json } => {
            let st = daemon::start(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "Atlassian sync daemon started (pid={}, transport={}, endpoint={}).",
                    st.pid.map_or_else(|| "-".to_string(), |p| p.to_string()),
                    st.transport.as_deref().unwrap_or("-"),
                    st.endpoint.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        DaemonCommand::Stop { json } => {
            let stopped = daemon::stop(cfg).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "stopped": stopped }))?
                );
            } else if stopped {
                println!("Atlassian sync daemon stopped.");
            } else {
                println!("Atlassian sync daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Status { json } => {
            let st = daemon::status(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else if st.running {
                println!(
                    "Atlassian sync daemon is running (pid={}, version={}, transport={}, endpoint={})",
                    st.pid.map_or_else(|| "-".to_string(), |p| p.to_string()),
                    st.version.as_deref().unwrap_or("-"),
                    st.transport.as_deref().unwrap_or("-"),
                    st.endpoint.as_deref().unwrap_or("-"),
                );
                println!(
                    "Profile: {} · Uptime: {}s · Attached clients: {} · Syncing: {}",
                    st.profile.as_deref().unwrap_or("-"),
                    st.uptime_secs.unwrap_or(0),
                    st.attached_clients,
                    if st.sync_busy { "yes" } else { "no" },
                );
                if let Some(last) = st.last_sync_at.as_deref() {
                    println!("Last sync: {last} ({})", st.sync_summary);
                }
                if let Some(err) = st.last_error.as_deref() {
                    println!("Last error: {err}");
                }
            } else {
                println!("Atlassian sync daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Restart { json } => {
            let st = daemon::restart(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "Atlassian sync daemon restarted (pid={}, transport={}, endpoint={}).",
                    st.pid.map_or_else(|| "-".to_string(), |p| p.to_string()),
                    st.transport.as_deref().unwrap_or("-"),
                    st.endpoint.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        DaemonCommand::Run => daemon::run_daemon_foreground(cfg.clone()).await,
    }
}

pub(super) async fn cmd_update(
    cfg: &AppConfig,
    check_only: bool,
    force: bool,
    repo: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let target_repo = repo.unwrap_or(&cfg.github_repo);
    let result = updater::check_or_apply_update(target_repo, check_only, force).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "Current version: {} · Latest version: {}",
            result.current_version, result.latest_version
        );
        if result.updated {
            println!("Update applied: {}", result.binary_path);
        } else if result.update_available {
            println!("New version available: {}", result.release_url);
        } else {
            println!("Already up to date.");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting & validation helpers

