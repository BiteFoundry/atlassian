//! Dual-persona (`atlassian` / `confluence` / `conf`) CLI parser and command dispatcher.

mod args;
mod confluence;
mod format;
mod jira;
mod login;
mod ops;
mod parse;
mod sites;
#[cfg(test)]
mod tests;

use std::{ffi::OsString, path::Path};

use anyhow::{bail, Result};
use serde_json::json;

use crate::{
    atlassian::{client::error_payload, AtlassianClient, SiteClientPool},
    auth::AuthManager,
    config::{self, AppConfig, Persona},
    storage::Storage,
};

pub use args::{
    Cli, Command, ConfluenceCli, ConfluenceCommand, ConfluenceWorkflow, DaemonCommand,
    DeploymentArg, EditBodyArgs, JiraSubcommand, JiraWorkflow, PageBodyArgs, SharedCommand,
    SiteAddArgs, SitesCommand, WikiCommand,
};
#[allow(unused_imports)]
pub(crate) use format::{delete_gate, format_kanban_board, looks_like_jira_key};
#[allow(unused_imports)]
pub(crate) use parse::{argv_requests_json, try_parse_cli_from};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct JsonReported(pub(super) String);

pub fn report_error(err: &anyhow::Error) {
    eprintln!("Error: {err:#}");
    if err.downcast_ref::<JsonReported>().is_some() {
        return;
    }
    let argv: Vec<OsString> = std::env::args_os().collect();
    if argv_requests_json(&argv) {
        let payload = json!({
            "ok": false,
            "error": error_payload(err),
        });
        if let Ok(text) = serde_json::to_string_pretty(&payload) {
            println!("{text}");
        }
    }
}

pub async fn run() -> Result<()> {
    let mut cfg = AppConfig::load()?;
    let persona = Persona::detect();
    match persona {
        Persona::Jira => {
            let cli: Cli = parse::parse_cli_with_stack("atlassian");
            let site_override = cli.site.clone();
            Box::pin(dispatch_jira_cli(&mut cfg, persona, site_override, cli.command)).await
        }
        Persona::Confluence => {
            let launcher = std::env::var("ATLASSIAN_COMMAND_NAME").ok();
            let invoked_as_conf = std::env::args_os().next().is_some_and(|arg| {
                Path::new(&arg)
                    .file_stem()
                    .is_some_and(|stem| stem.eq_ignore_ascii_case("conf"))
            });
            let name =
                if launcher.as_deref() == Some("conf") || (launcher.is_none() && invoked_as_conf) {
                    "conf"
                } else {
                    "confluence"
                };
            let cli: ConfluenceCli = parse::parse_cli_with_stack(name);
            let site_override = cli.site.clone();
            Box::pin(dispatch_confluence_cli(
                &mut cfg,
                persona,
                site_override,
                cli.command,
            ))
            .await
        }
    }
}

pub(super) fn apply_site_override(cfg: &AppConfig, site_override: Option<&str>) -> Result<AppConfig> {
    let Some(raw) = site_override.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(cfg.clone());
    };
    let mut scoped = cfg.clone();
    if raw.eq_ignore_ascii_case("all") {
        scoped.active_site = None;
        return Ok(scoped);
    }
    let Some(found) = cfg.find_effective_site(raw) else {
        let available: Vec<String> = cfg
            .effective_sites()
            .into_iter()
            .map(|s| s.id)
            .collect();
        if available.is_empty() {
            bail!(
                "Site '{raw}' not found (no sites configured). Run `atlassian sites add <URL>` or `atlassian login --oauth` first."
            );
        }
        bail!(
            "Site '{raw}' not found. Available sites: {}.",
            available.join(", ")
        );
    };
    scoped.active_site = Some(found.id.clone());
    scoped.default_site = Some(found.id.clone());
    scoped.site_url = found.site_url.clone();
    scoped.confluence_url = found.confluence_url.clone();
    scoped.deployment = found.deployment;
    Ok(scoped)
}

async fn dispatch_jira_cli(
    cfg: &mut AppConfig,
    persona: Persona,
    site_override: Option<String>,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(ops::cmd_tui(&mut run_cfg, persona)).await
        }
        Some(Command::Shared(cmd)) => {
            Box::pin(ops::dispatch_shared_command(cfg, persona, site_override, cmd)).await
        }
        Some(Command::Jira(cmd)) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(jira::dispatch_jira_subcommand(&scoped, cmd)).await
        }
        Some(Command::Wiki { command }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(confluence::dispatch_wiki_subcommand(&scoped, command)).await
        }
    }
}

async fn dispatch_confluence_cli(
    cfg: &mut AppConfig,
    persona: Persona,
    site_override: Option<String>,
    command: Option<ConfluenceCommand>,
) -> Result<()> {
    match command {
        None => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(ops::cmd_tui(&mut run_cfg, persona)).await
        }
        Some(ConfluenceCommand::Shared(cmd)) => {
            Box::pin(ops::dispatch_shared_command(cfg, persona, site_override, cmd)).await
        }
        Some(ConfluenceCommand::Wiki(cmd)) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(confluence::dispatch_wiki_subcommand(&scoped, cmd)).await
        }
        Some(ConfluenceCommand::Jira { command }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(jira::dispatch_jira_subcommand(&scoped, command)).await
        }
    }
}

pub(super) fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, AtlassianClient, Storage)> {
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let (auth, client) = open_client(cfg, require_login)?;
    Ok((auth, client, storage))
}

pub(super) fn open_client(cfg: &AppConfig, require_login: bool) -> Result<(AuthManager, AtlassianClient)> {
    if let Ok(pool) = SiteClientPool::from_config(cfg) {
        if let Some(target_site) = cfg.resolved_site_profile() {
            if let Ok(client) = pool.client_for(&target_site.id) {
                let auth = client.auth().clone();
                if require_login && !auth.is_logged_in() {
                    bail!(
                        "Authentication required for site '{}' (account '{}'). Run `{} login --oauth` or `{} login --url {} --email <email>`.",
                        target_site.id,
                        target_site.account_id,
                        Persona::detect().command_name(),
                        Persona::detect().command_name(),
                        target_site.site_url,
                    );
                }
                return Ok((auth, client));
            }
        }
    }

    let auth = AuthManager::new(&cfg.profile);
    if require_login && !auth.is_logged_in() {
        bail!(
            "Login required. Run `{} login --url https://<site>.atlassian.net --email <email>` first.",
            Persona::detect().command_name()
        );
    }
    let client = AtlassianClient::new(cfg.clone(), auth.clone())?;
    Ok((auth, client))
}

