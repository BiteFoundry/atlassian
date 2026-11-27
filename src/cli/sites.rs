//! Multi-site management (`sites`) and diagnostics (`doctor`) commands.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{
    atlassian::client::IdentityProbe,
    auth::{
        fetch_accessible_resources, run_oauth_loopback_login, AuthManager,
        OAuthLoopbackOptions, StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{self, AppConfig, DeploymentKind, Persona, SiteProfile},
    storage::{SiteRecord, Storage},
};

use super::{
    args::SitesCommand,
    format::{delete_gate, prompt_line},
    open_client,
};

pub(super) async fn cmd_sites(cfg: &mut AppConfig, action: SitesCommand) -> Result<()> {
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    match action {
        SitesCommand::List { json } => {
            let sites = cfg.effective_sites();
            let mut rows = Vec::with_capacity(sites.len());
            for site in &sites {
                let counts = storage.cache_counts_for_site(&site.id).unwrap_or_default();
                let db_rec = storage.get_site(&site.id).ok().flatten();
                let auth_kind = AuthManager::for_account(&site.account_id)
                    .load_credentials()
                    .ok()
                    .flatten()
                    .map(|c| c.auth_kind_label().to_string())
                    .unwrap_or_else(|| "none".to_string());
                let is_default = cfg
                    .default_site
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case(&site.id));
                rows.push(json!({
                    "id": site.id,
                    "name": site.display_name(),
                    "site_url": site.site_url,
                    "confluence_url": site.effective_confluence_root(),
                    "deployment": site.deployment.as_str(),
                    "cloud_id": site.cloud_id,
                    "account_id": site.account_id,
                    "auth_kind": auth_kind,
                    "enabled": site.enabled,
                    "is_default": is_default,
                    "issue_count": counts.issues,
                    "page_count": counts.pages,
                    "last_synced_at": db_rec.as_ref().and_then(|r| r.last_synced_at.clone()),
                    "last_error": db_rec.as_ref().and_then(|r| r.last_error.clone()),
                }));
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "active_site": cfg.active_site,
                        "default_site": cfg.default_site,
                        "sites": rows,
                    }))?
                );
            } else if sites.is_empty() {
                println!("No Atlassian sites registered. Run `atlassian login --oauth` or `atlassian sites add <URL>`.");
            } else {
                println!(
                    "{:<16} {:<18} {:<12} {:<10} {:<8} {:<8} URL",
                    "ID", "NAME", "DEPLOYMENT", "ENABLED", "ISSUES", "PAGES"
                );
                for (site, row) in sites.iter().zip(rows.iter()) {
                    let mark = if row["is_default"].as_bool().unwrap_or(false) {
                        "*"
                    } else {
                        " "
                    };
                    println!(
                        "{mark}{:<15} {:<18} {:<12} {:<10} {:<8} {:<8} {}",
                        site.id,
                        site.display_name(),
                        site.deployment.as_str(),
                        if site.enabled { "yes" } else { "no" },
                        row["issue_count"].as_u64().unwrap_or(0),
                        row["page_count"].as_u64().unwrap_or(0),
                        site.site_url,
                    );
                }
            }
            Ok(())
        }
        SitesCommand::Add(args) => {
            let site_url = config::normalize_base_url(&args.url);
            if site_url.is_empty() {
                bail!("Site URL cannot be empty.");
            }
            let deployment = args
                .deployment
                .map(Into::into)
                .unwrap_or_else(|| DeploymentKind::infer_from_url(&site_url));
            let site_id = args
                .id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| config::slugify_site_id(&site_url));
            let name = args
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| site_id.clone());
            let has_inline_creds = args.pat.is_some() || args.token.is_some();
            let account_id = args
                .account
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if has_inline_creds {
                        site_id.clone()
                    } else {
                        cfg.profile.clone()
                    }
                });

            if let Some(pat) = args.pat.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let auth = AuthManager::for_account(&account_id);
                auth.save_credentials(&StoredCredentials::BearerPat {
                    site_url: site_url.clone(),
                    token: pat.to_string(),
                    deployment,
                })?;
            } else if let Some(token) =
                args.token.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                let email = args
                    .email
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("`--email` is required when passing `--token`."))?;
                let auth = AuthManager::for_account(&account_id);
                auth.save_credentials(&StoredCredentials::ApiToken {
                    site_url: site_url.clone(),
                    email: email.to_string(),
                    api_token: token.to_string(),
                    deployment,
                })?;
            }

            cfg.materialize_legacy_site();

            let profile = SiteProfile {
                id: site_id.clone(),
                name: name.clone(),
                site_url: site_url.clone(),
                confluence_url: args
                    .confluence_url
                    .as_deref()
                    .map(config::normalize_base_url)
                    .unwrap_or_default(),
                deployment,
                cloud_id: None,
                account_id: account_id.clone(),
                enabled: true,
                scopes: Vec::new(),
                confluence_account_id: None,
            };
            storage.upsert_site(&SiteRecord {
                site_id: profile.id.clone(),
                name: profile.display_name().to_string(),
                site_url: profile.site_url.clone(),
                deployment: profile.deployment.as_str().to_string(),
                enabled: true,
                last_synced_at: None,
                last_error: None,
            })?;
            cfg.upsert_site(profile.clone());
            if cfg.default_site.is_none() {
                cfg.default_site = Some(site_id.clone());
            }
            if cfg.site_url.trim().is_empty() {
                cfg.site_url = site_url.clone();
                cfg.deployment = deployment;
            }
            cfg.save()?;

            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "ok": true, "site": profile }))?
                );
            } else {
                println!(
                    "Added site '{}' ({}) [deployment={}, account={}]",
                    profile.id,
                    profile.site_url,
                    profile.deployment.as_str(),
                    profile.account_id
                );
            }
            Ok(())
        }
        SitesCommand::Discover {
            account,
            client_id,
            client_secret,
            json,
        } => {
            let http = reqwest::Client::new();
            let candidate_account = account
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    cfg.effective_sites()
                        .into_iter()
                        .find(|s| s.is_oauth_cloud())
                        .map(|s| s.account_id)
                })
                .unwrap_or_else(|| "cloud-oauth".to_string());

            let auth = AuthManager::for_account(&candidate_account);
            let existing_oauth = matches!(
                auth.load_credentials().ok().flatten(),
                Some(StoredCredentials::CloudOAuth { .. })
            );

            let resources = if existing_oauth {
                let fresh = auth
                    .ensure_fresh_credentials(&http, DEFAULT_OAUTH_AUTH_BASE)
                    .await?;
                let token = fresh
                    .oauth_access_token()
                    .ok_or_else(|| anyhow::anyhow!("OAuth access token missing"))?;
                fetch_accessible_resources(&http, DEFAULT_OAUTH_API_BASE, token).await?
            } else {
                let resolved_client_id = client_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| cfg.effective_oauth_client_id())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "No OAuth credentials found for account '{candidate_account}'. Run `atlassian login --oauth --client-id <ID>` first."
                        )
                    })?;
                let resolved_secret = client_secret
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| cfg.effective_oauth_client_secret());
                let opts = OAuthLoopbackOptions {
                    client_id: resolved_client_id,
                    client_secret: resolved_secret,
                    ..Default::default()
                };
                let (creds, resources) = run_oauth_loopback_login(&http, &opts).await?;
                auth.save_credentials(&creds)?;
                resources
            };

            cfg.materialize_legacy_site();
            let mut discovered = Vec::with_capacity(resources.len());
            for res in &resources {
                let profile = res.to_site_profile(&candidate_account);
                let _ = storage.upsert_site(&SiteRecord {
                    site_id: profile.id.clone(),
                    name: profile.display_name().to_string(),
                    site_url: profile.site_url.clone(),
                    deployment: profile.deployment.as_str().to_string(),
                    enabled: true,
                    last_synced_at: None,
                    last_error: None,
                });
                cfg.upsert_site(profile.clone());
                discovered.push(profile);
            }
            if cfg.default_site.is_none() {
                if let Some(first) = discovered.first() {
                    cfg.default_site = Some(first.id.clone());
                }
            }
            cfg.save()?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "account_id": candidate_account,
                        "discovered": discovered,
                    }))?
                );
            } else {
                println!(
                    "Discovered {} Atlassian Cloud site(s) for account '{candidate_account}':",
                    discovered.len()
                );
                for site in &discovered {
                    println!(
                        "  - {} ({}) [cloudId={}]",
                        site.id,
                        site.site_url,
                        site.cloud_id.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(())
        }
        SitesCommand::Default { id, json } => {
            // "cfg" here is the configuration as loaded from disk (no --site override), so a
            // cleared active_site is the persisted TUI selection.
            let cleared_active_site = cfg.set_default_site(&id)?;
            let found = cfg
                .default_site_profile()
                .context("default site vanished after it was set")?;
            cfg.save()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "default_site": found.id,
                        "cleared_active_site": cleared_active_site,
                    }))?
                );
            } else {
                println!("Default site set to '{}' ({}).", found.id, found.site_url);
                if let Some(previous) = cleared_active_site {
                    println!("Cleared the site selected in the TUI ('{previous}'), which took precedence over the default.");
                }
            }
            Ok(())
        }
        SitesCommand::Enable { id, json } => {
            if cfg.sites.is_empty() {
                cfg.sites = cfg.effective_sites();
            }
            let Some(site) = cfg
                .sites
                .iter_mut()
                .find(|s| s.id.eq_ignore_ascii_case(id.trim()))
            else {
                bail!("Site '{id}' not found.");
            };
            site.enabled = true;
            let rec = SiteRecord {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment: site.deployment.as_str().to_string(),
                enabled: true,
                last_synced_at: None,
                last_error: None,
            };
            let site_id = site.id.clone();
            let _ = storage.upsert_site(&rec);
            cfg.save()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "site_id": site_id,
                        "enabled": true,
                    }))?
                );
            } else {
                println!("Enabled site '{site_id}'.");
            }
            Ok(())
        }
        SitesCommand::Disable { id, json } => {
            if cfg.sites.is_empty() {
                cfg.sites = cfg.effective_sites();
            }
            let Some(site) = cfg
                .sites
                .iter_mut()
                .find(|s| s.id.eq_ignore_ascii_case(id.trim()))
            else {
                bail!("Site '{id}' not found.");
            };
            site.enabled = false;
            let rec = SiteRecord {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment: site.deployment.as_str().to_string(),
                enabled: false,
                last_synced_at: None,
                last_error: None,
            };
            let site_id = site.id.clone();
            let _ = storage.upsert_site(&rec);
            cfg.save()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "site_id": site_id,
                        "enabled": false,
                    }))?
                );
            } else {
                println!("Disabled site '{site_id}'.");
            }
            Ok(())
        }
        SitesCommand::Remove { id, yes, json } => {
            let answer = if !yes && !json {
                Some(prompt_line(&format!(
                    "Remove site '{id}' and delete its local cache? [y/N]: "
                ))?)
            } else {
                None
            };
            delete_gate(yes, json, answer.as_deref())?;
            let Some(removed) = cfg.remove_effective_site(&id) else {
                bail!("Site '{id}' not found.");
            };
            // The cache is keyed by the canonical id, whatever spelling the user typed.
            let _ = storage.delete_site_data(&removed.id);
            cfg.save()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "removed": removed.id,
                    }))?
                );
            } else {
                println!("Removed site '{}' and purged its cached data.", removed.id);
            }
            Ok(())
        }
    }
}


pub(super) fn tls_report(sources: &[crate::atlassian::client::ExtraCaSource]) -> Value {
    json!({
        "native_roots": true,
        "extra_ca_sources": sources
            .iter()
            .map(|s| json!({
                "env": s.env,
                "path": s.path,
                "certificates": s.certificates,
                "error": s.error,
            }))
            .collect::<Vec<_>>(),
    })
}

pub(super) fn tls_roots_line(sources: &[crate::atlassian::client::ExtraCaSource]) -> String {
    let mut line = "OS trust store + bundled roots".to_string();
    if sources.is_empty() {
        line.push_str(" (no extra CA; set ATLASSIAN_EXTRA_CA_PEM for a corporate root)");
        return line;
    }
    for source in sources {
        match &source.error {
            None => line.push_str(&format!(
                " + {}={} ({} certificate(s))",
                source.env, source.path, source.certificates
            )),
            Some(err) => line.push_str(&format!(
                " + {}={} (ignored: {err})",
                source.env, source.path
            )),
        }
    }
    line
}

/// "env_credentials" object of the doctor report: whether the environment carries
/// credentials and which site they are primary for (so a 401 caused by a stray
/// ATLASSIAN_PAT is diagnosable). Never includes the credential itself.
pub(super) fn env_credentials_report(cfg: &AppConfig) -> Value {
    let env = crate::atlassian::client::PoolEnv::from_process_env();
    json!({
        "present": env.credentials_site_url.is_some(),
        "applies_to_site": env.credentials_site_id(&cfg.effective_sites()),
    })
}

/// "env:ATLASSIAN_CONFIG_DIR" when the override is set, else "default".
pub(super) fn config_dir_source(env_override: Option<&str>) -> &'static str {
    if env_override.is_some_and(|v| !v.trim().is_empty()) {
        "env:ATLASSIAN_CONFIG_DIR"
    } else {
        "default"
    }
}

/// Everything "doctor --json" reports, gathered before printing.
pub(super) struct DoctorFacts<'a> {
    pub(super) persona: Persona,
    pub(super) profile: &'a str,
    pub(super) site_id: &'a str,
    pub(super) credential_account: &'a str,
    pub(super) confluence_credential_account: Option<&'a str>,
    pub(super) env_credentials: Value,
    pub(super) tls: Value,
    pub(super) deployment: &'a str,
    pub(super) site_url: &'a str,
    pub(super) confluence_url: &'a str,
    pub(super) logged_in: bool,
    pub(super) auth_status: &'a str,
    pub(super) config_dir: &'a Path,
    pub(super) config_dir_source: &'a str,
    pub(super) data_dir: &'a Path,
    pub(super) config_path: &'a Path,
    pub(super) credentials_fallback_path: &'a Path,
    pub(super) db_path: &'a Path,
    pub(super) download_dir: &'a Path,
    pub(super) cache_counts: Value,
    /// "None" when "--probe" was not requested.
    pub(super) probe: Option<&'a IdentityProbe>,
}

/// JSON body of "doctor --json". "probe_me" / "probe_error" describe Jira (as before);
/// "probe_confluence_me" / "probe_confluence_error" describe Confluence.
pub(super) fn doctor_report(facts: &DoctorFacts<'_>) -> Value {
    let mut probe_me = Value::Null;
    let mut probe_error = Value::Null;
    let mut probe_confluence_me = Value::Null;
    let mut probe_confluence_error = Value::Null;
    if let Some(probe) = facts.probe {
        match &probe.jira {
            Some(Ok(user)) => probe_me = json!(user),
            Some(Err(failure)) => probe_error = json!(failure.message),
            None => probe_error = json!("Jira was not probed: no site URL is configured."),
        }
        match &probe.confluence {
            Some(Ok(user)) => probe_confluence_me = json!(user),
            Some(Err(failure)) => probe_confluence_error = json!(failure.message),
            None => {}
        }
    }
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "persona": facts.persona.as_str(),
        "profile": facts.profile,
        "site_id": facts.site_id,
        "credential_account": facts.credential_account,
        "confluence_credential_account": facts.confluence_credential_account,
        "env_credentials": facts.env_credentials,
        "tls": facts.tls,
        "deployment": facts.deployment,
        "site_url": facts.site_url,
        "confluence_url": facts.confluence_url,
        "logged_in": facts.logged_in,
        "auth_status": facts.auth_status,
        "config_dir": facts.config_dir,
        "config_dir_source": facts.config_dir_source,
        "data_dir": facts.data_dir,
        "config_path": facts.config_path,
        "config_exists": facts.config_path.exists(),
        "credentials_fallback_path": facts.credentials_fallback_path,
        "credentials_fallback_exists": facts.credentials_fallback_path.exists(),
        "db_path": facts.db_path,
        "db_exists": facts.db_path.exists(),
        "download_dir": facts.download_dir,
        "cache_counts": facts.cache_counts,
        "probe_me": probe_me,
        "probe_error": probe_error,
        "probe_confluence_me": probe_confluence_me,
        "probe_confluence_error": probe_confluence_error,
    })
}

pub(super) async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    probe: bool,
    json_out: bool,
) -> Result<()> {
    let (auth, client) = open_client(cfg, false)?;
    let cfg_path = config::config_file_path();
    let cred_path = config::credentials_fallback_path();
    let db_path = config::cache_db_path(&cfg.profile);
    let dl_dir = cfg.resolved_download_dir();
    let auth_status = auth.status_summary();
    let logged_in = auth.is_logged_in();
    let effective_site_url = client.site_url().to_string();
    let effective_conf_url = client.confluence_root_url();
    let effective_deployment = client.deployment().as_str();

    let cache_counts = if db_path.exists() {
        Storage::open(&db_path)
            .and_then(|s| s.cache_counts())
            .ok()
    } else {
        None
    };

    let ca_sources = crate::atlassian::client::extra_ca_sources();

    let identity = if probe {
        Some(client.probe_identity().await)
    } else {
        None
    };
    let config_dir = config::config_dir();
    let data_dir = config::data_dir(&cfg.profile);
    let config_dir_override = std::env::var("ATLASSIAN_CONFIG_DIR").ok();
    let config_dir_source = config_dir_source(config_dir_override.as_deref());

    if json_out {
        let report = doctor_report(&DoctorFacts {
            persona,
            profile: &cfg.profile,
            site_id: client.site_id(),
            credential_account: auth.profile(),
            confluence_credential_account: client.confluence_credential_account(),
            env_credentials: env_credentials_report(cfg),
            tls: tls_report(&ca_sources),
            deployment: effective_deployment,
            site_url: &effective_site_url,
            confluence_url: &effective_conf_url,
            logged_in,
            auth_status: &auth_status,
            config_dir: &config_dir,
            config_dir_source,
            data_dir: &data_dir,
            config_path: &cfg_path,
            credentials_fallback_path: &cred_path,
            db_path: &db_path,
            download_dir: &dl_dir,
            cache_counts: json!(&cache_counts),
            probe: identity.as_ref(),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("atlassian-cli v{} ({})", env!("CARGO_PKG_VERSION"), persona.as_str());
        println!("Profile: {} · Deployment: {effective_deployment}", cfg.profile);
        println!(
            "Jira URL: {}",
            if effective_site_url.is_empty() {
                "(not set)"
            } else {
                &effective_site_url
            }
        );
        println!(
            "Confluence URL: {}",
            if effective_conf_url.is_empty() {
                "(not set)"
            } else {
                &effective_conf_url
            }
        );
        println!("Auth status: {auth_status}");
        if let Some(account) = client.confluence_credential_account() {
            println!("Confluence credential: separate account '{account}'");
        }
        println!("TLS roots: {}", tls_roots_line(&ca_sources));
        println!(
            "Config dir: {} ({})",
            config_dir.display(),
            if config_dir_source == "default" {
                "default; override with ATLASSIAN_CONFIG_DIR"
            } else {
                "ATLASSIAN_CONFIG_DIR override"
            }
        );
        println!("Data dir: {}", data_dir.display());
        println!(
            "Config file: {} ({})",
            cfg_path.display(),
            if cfg_path.exists() { "exists" } else { "missing" }
        );
        println!(
            "Credentials fallback: {} ({})",
            cred_path.display(),
            if cred_path.exists() { "exists" } else { "missing" }
        );
        println!(
            "SQLite DB: {} ({})",
            db_path.display(),
            if db_path.exists() { "exists" } else { "missing" }
        );
        println!("Download dir: {}", dl_dir.display());
        if let Some(counts) = cache_counts {
            println!(
                "Cache: projects={}, boards={}, issues={}, spaces={}, pages={}",
                counts.projects, counts.boards, counts.issues, counts.spaces, counts.pages
            );
        }
        if let Some(identity) = &identity {
            match &identity.jira {
                Some(Ok(me)) => println!(
                    "Live probe OK: {} ({}) [id={}]",
                    me.display_name,
                    me.email.as_deref().unwrap_or("-"),
                    me.id
                ),
                Some(Err(failure)) => println!("Live probe failed: {}", failure.message),
                None => println!("Live probe skipped: no Jira URL is configured."),
            }
            match &identity.confluence {
                Some(Ok(me)) => println!(
                    "Confluence probe OK: {} ({}) [id={}]",
                    me.display_name,
                    me.email.as_deref().unwrap_or("-"),
                    me.id
                ),
                Some(Err(failure)) => println!("Confluence probe failed: {}", failure.message),
                None => {}
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Jira commands
// ---------------------------------------------------------------------------


