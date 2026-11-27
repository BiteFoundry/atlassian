//! Authentication commands: `login` (OAuth 3LO / Cloud Account / API Token / PAT), `logout`, `whoami`.

use std::io::{self, IsTerminal};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{
    atlassian::{
        client::{HttpStatusError, IdentityProbe},
        AtlassianClient,
    },
    auth::{
        run_oauth_loopback_login, AuthManager, EnvCredentialPolicy, OAuthLoopbackOptions,
        StoredCredentials,
    },
    config::{self, AppConfig, DeploymentKind, Persona, SiteProfile},
    storage::{SiteRecord, Storage},
};

use super::{format::prompt_line, open_runtime};

pub(super) struct LoginParams {
    pub(super) url: Option<String>,
    pub(super) confluence_url: Option<String>,
    pub(super) email: Option<String>,
    pub(super) token: Option<String>,
    pub(super) pat: Option<String>,
    pub(super) confluence_pat: Option<String>,
    pub(super) confluence_account: Option<String>,
    pub(super) deployment: Option<DeploymentKind>,
    pub(super) oauth: bool,
    pub(super) cloud_account: bool,
    pub(super) client_id: Option<String>,
    pub(super) client_secret: Option<String>,
    pub(super) account: Option<String>,
    pub(super) name: Option<String>,
}

pub(super) async fn cmd_login(
    cfg: &mut AppConfig,
    site_override: Option<&str>,
    params: LoginParams,
    json_out: bool,
) -> Result<()> {
    if params.oauth {
        return cmd_login_oauth(cfg, params, json_out).await;
    }
    if params.cloud_account {
        return cmd_login_cloud_account(cfg, params, json_out).await;
    }

    let persona = Persona::detect();
    let confluence_pat = params
        .confluence_pat
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // "login --site <id> --confluence-pat <PAT>" only adds the Confluence credential: the
    // site's stored primary credential is reused instead of being asked for again.
    let reuse_primary_credential = confluence_pat.is_some()
        && params.pat.is_none()
        && params.token.is_none()
        && params.email.is_none()
        && params.account.is_none();

    let explicit_site_ref = site_override
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"));
    let target_site = if let Some(raw) = explicit_site_ref {
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
        Some(found)
    } else {
        None
    };

    let default_url_hint = target_site
        .as_ref()
        .map(|s| s.site_url.clone())
        .or_else(|| cfg.resolved_site_profile().map(|s| s.site_url))
        .unwrap_or_else(|| cfg.site_url.clone());

    let raw_url = match params.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => u.to_string(),
        None => {
            if !default_url_hint.trim().is_empty()
                && (params.pat.is_some()
                    || (params.email.is_some() && params.token.is_some())
                    || reuse_primary_credential)
            {
                default_url_hint.clone()
            } else {
                let default_hint = if default_url_hint.trim().is_empty() {
                    String::new()
                } else {
                    format!(" [{default_url_hint}]")
                };
                let entered = prompt_line(&format!(
                    "Atlassian site URL{default_hint} (e.g. https://acme.atlassian.net): "
                ))?;
                if entered.is_empty() && !default_url_hint.trim().is_empty() {
                    default_url_hint.clone()
                } else {
                    entered
                }
            }
        }
    };
    let site_url = config::normalize_base_url(&raw_url);
    if site_url.is_empty() {
        bail!("Site URL is empty. Specify `--url https://<site>.atlassian.net`.");
    }

    let matched_site: Option<SiteProfile> = if let Some(ref target) = target_site {
        Some(target.clone())
    } else {
        // A split Confluence credential lives on a site entry, so a legacy single-site
        // config is matched (and then stored) as its "default" site.
        let known_sites = if cfg.sites.is_empty() && confluence_pat.is_some() {
            cfg.effective_sites()
        } else {
            cfg.sites.clone()
        };
        let norm_target = config::normalize_url_for_match(&site_url);
        known_sites.into_iter().find(|s| {
            let norm = config::normalize_url_for_match(&s.site_url);
            !norm.is_empty() && norm == norm_target
        })
    };

    let deployment = params.deployment.unwrap_or_else(|| {
        if params.url.is_none() {
            if let Some(ref matched) = matched_site {
                return matched.deployment;
            }
        }
        DeploymentKind::infer_from_url(&site_url)
    });

    let confluence_url = if let Some(ref cu) = params.confluence_url {
        config::normalize_base_url(cu)
    } else if deployment == DeploymentKind::DataCenter
        && params.pat.is_none()
        && params.token.is_none()
        && confluence_pat.is_none()
    {
        let entered = prompt_line("Separate Confluence URL (press Enter for same host): ")?;
        config::normalize_base_url(&entered)
    } else if let Some(ref matched) = matched_site {
        matched.confluence_url.clone()
    } else if cfg.sites.is_empty()
        && !cfg.site_url.trim().is_empty()
        && config::normalize_url_for_match(&cfg.site_url)
            == config::normalize_url_for_match(&site_url)
    {
        cfg.confluence_url.clone()
    } else {
        String::new()
    };

    if confluence_pat.is_some() && deployment != DeploymentKind::DataCenter {
        bail!("`--confluence-pat` applies to Data Center sites only; Atlassian Cloud uses one credential for Jira and Confluence.");
    }
    let help = login_failure_help(persona, &site_url, deployment);
    let stored_primary = if reuse_primary_credential {
        matched_site.as_ref().and_then(|site| {
            AuthManager::for_account(&site.account_id)
                .load_credentials()
                .ok()
                .flatten()
        })
    } else {
        None
    };
    let primary_is_reused = stored_primary.is_some();

    let creds = if let Some(stored) = stored_primary {
        stored
    } else if let Some(pat) = params.pat.filter(|s| !s.trim().is_empty()) {
        StoredCredentials::BearerPat {
            site_url: site_url.clone(),
            token: pat.trim().to_string(),
            deployment,
        }
    } else if let Some(token) = params.token.filter(|s| !s.trim().is_empty()) {
        let email = match params.email.filter(|s| !s.trim().is_empty()) {
            Some(e) => e.trim().to_string(),
            None => prompt_line("Email (or Data Center username): ")?,
        };
        if email.trim().is_empty() {
            bail!("Email is empty. Use `--email <email>` or `--pat <PAT>`.");
        }
        StoredCredentials::ApiToken {
            site_url: site_url.clone(),
            email,
            api_token: token.trim().to_string(),
            deployment,
        }
    } else if deployment == DeploymentKind::DataCenter {
        let email = if let Some(e) = params.email.filter(|s| !s.trim().is_empty()) {
            e.trim().to_string()
        } else {
            prompt_line("Data Center username/email (leave empty and press Enter for PAT auth): ")?
        };
        if email.trim().is_empty() {
            let pat = rpassword::prompt_password("Personal Access Token (PAT): ")
                .map_err(|err| anyhow::anyhow!("Failed to read PAT: {err}\n{help}"))?;
            if pat.trim().is_empty() {
                bail!("PAT is empty.\n{help}");
            }
            StoredCredentials::BearerPat {
                site_url: site_url.clone(),
                token: pat.trim().to_string(),
                deployment,
            }
        } else {
            let token = rpassword::prompt_password("Password or API token: ")
                .map_err(|err| anyhow::anyhow!("Failed to read token: {err}\n{help}"))?;
            if token.trim().is_empty() {
                bail!("Password/token is empty.\n{help}");
            }
            StoredCredentials::ApiToken {
                site_url: site_url.clone(),
                email,
                api_token: token.trim().to_string(),
                deployment,
            }
        }
    } else {
        let email = match params.email.filter(|s| !s.trim().is_empty()) {
            Some(e) => e.trim().to_string(),
            None => prompt_line("Atlassian account email: ")?,
        };
        if email.trim().is_empty() {
            bail!("Email is empty.");
        }
        let token = rpassword::prompt_password("Atlassian API token: ")
            .map_err(|err| anyhow::anyhow!("Failed to read API token: {err}\n{help}"))?;
        if token.trim().is_empty() {
            bail!("API token is empty.\n{help}");
        }
        StoredCredentials::ApiToken {
            site_url: site_url.clone(),
            email,
            api_token: token.trim().to_string(),
            deployment,
        }
    };

    let explicit_account = params
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let explicit_name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let has_legacy_other_url = cfg.sites.is_empty()
        && !cfg.site_url.trim().is_empty()
        && config::normalize_url_for_match(&cfg.site_url)
            != config::normalize_url_for_match(&site_url);
    let use_multi_site = target_site.is_some()
        || !cfg.sites.is_empty()
        || explicit_name.is_some()
        || explicit_account.is_some()
        || has_legacy_other_url
        || confluence_pat.is_some();

    let (resolved_site_id, account_id, display_name) = if use_multi_site {
        if let Some(ref existing) = matched_site {
            let sid = existing.id.clone();
            let acct = explicit_account.unwrap_or_else(|| existing.account_id.clone());
            let name = explicit_name.unwrap_or_else(|| existing.name.clone());
            (sid, acct, name)
        } else {
            let base_id = config::slugify_site_id(&site_url);
            let existing_ids: Vec<String> = cfg
                .effective_sites()
                .into_iter()
                .map(|s| s.id)
                .collect();
            let mut sid = base_id.clone();
            let mut suffix = 2usize;
            while existing_ids.iter().any(|id| id.eq_ignore_ascii_case(&sid)) {
                sid = format!("{base_id}-{suffix}");
                suffix += 1;
            }
            let acct = explicit_account.unwrap_or_else(|| sid.clone());
            let name = explicit_name.unwrap_or_else(|| sid.clone());
            (sid, acct, name)
        }
    } else {
        let acct = explicit_account.unwrap_or_else(|| cfg.profile.clone());
        ("default".to_string(), acct, "default".to_string())
    };

    let confluence_account_id: Option<String> = if confluence_pat.is_some() {
        let chosen = params
            .confluence_account
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                matched_site
                    .as_ref()
                    .and_then(|s| s.confluence_account_id.clone())
                    .filter(|id| !id.trim().is_empty())
            })
            .unwrap_or_else(|| format!("{account_id}-confluence"));
        if chosen.eq_ignore_ascii_case(&account_id) {
            bail!(
                "`--confluence-account` must differ from the site's credential account '{account_id}', or the Confluence PAT would overwrite the Jira credential."
            );
        }
        Some(chosen)
    } else if deployment == DeploymentKind::DataCenter {
        // Logging in again keeps an existing split Confluence credential.
        matched_site
            .as_ref()
            .and_then(|s| s.confluence_account_id.clone())
            .filter(|id| !id.trim().is_empty())
    } else {
        None
    };

    let candidate_profile = SiteProfile {
        id: resolved_site_id.clone(),
        name: display_name,
        site_url: site_url.clone(),
        confluence_url: confluence_url.clone(),
        deployment,
        cloud_id: matched_site.as_ref().and_then(|s| s.cloud_id.clone()),
        account_id: account_id.clone(),
        enabled: true,
        scopes: matched_site
            .as_ref()
            .map(|s| s.scopes.clone())
            .unwrap_or_default(),
        confluence_account_id,
    };

    // Validate credentials against the target site BEFORE persisting config or credentials.
    let probe_auth = AuthManager::with_credentials(&account_id, creds.clone());
    let mut probe_client = AtlassianClient::for_site(&candidate_profile, probe_auth, None)?;
    let new_confluence_credential = match (&confluence_pat, &candidate_profile.confluence_account_id)
    {
        (Some(token), Some(conf_account)) => {
            let conf_creds = StoredCredentials::BearerPat {
                site_url: candidate_profile.effective_confluence_root(),
                token: token.clone(),
                deployment,
            };
            probe_client = probe_client.with_confluence_auth(AuthManager::with_credentials(
                conf_account,
                conf_creds.clone(),
            ));
            Some((conf_account.clone(), conf_creds))
        }
        (None, Some(conf_account)) => {
            // The site already has a separate Confluence credential: probe with that one.
            probe_client = probe_client.with_confluence_auth(
                AuthManager::for_account(conf_account).with_env_policy(EnvCredentialPolicy::Never),
            );
            None
        }
        _ => None,
    };
    let probe = probe_client.probe_identity().await;
    let me = decide_login_identity(persona, &probe).map_err(|err| with_login_help(err, &help))?;
    if new_confluence_credential.is_some() {
        if let Some(Err(failure)) = &probe.confluence {
            // No generic login help here: it describes the Jira credential, and the
            // failure already says what Confluence expects.
            return Err(failure
                .to_error()
                .context("Confluence did not accept the `--confluence-pat` credential"));
        }
    }
    let confluence_warning = if new_confluence_credential.is_some() {
        None
    } else {
        confluence_login_warning(&probe, deployment, &resolved_site_id, persona)
    };

    if !primary_is_reused {
        let auth = AuthManager::for_account(&account_id);
        auth.save_credentials(&creds)?;
    }
    if let Some((conf_account, conf_creds)) = &new_confluence_credential {
        AuthManager::for_account(conf_account).save_credentials(conf_creds)?;
    }

    if use_multi_site {
        cfg.materialize_legacy_site();
        cfg.upsert_site(candidate_profile.clone());
        if target_site.is_none() {
            cfg.default_site = Some(resolved_site_id.clone());
            cfg.site_url = site_url.clone();
            cfg.confluence_url = confluence_url.clone();
            cfg.deployment = deployment;
        } else if cfg
            .default_site
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(&resolved_site_id))
        {
            cfg.site_url = site_url.clone();
            cfg.confluence_url = confluence_url.clone();
            cfg.deployment = deployment;
        }
    } else {
        cfg.site_url = site_url.clone();
        cfg.confluence_url = confluence_url.clone();
        cfg.deployment = deployment;
    }
    cfg.save()?;

    let effective_site_url = probe_client.site_url();
    let effective_conf_url = probe_client.confluence_root_url();
    let effective_deployment = probe_client.deployment().as_str();

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "site_id": resolved_site_id,
                "deployment": effective_deployment,
                "site_url": effective_site_url,
                "confluence_url": effective_conf_url,
                "credential_account": account_id,
                "confluence_credential_account": candidate_profile.confluence_account_id,
                "user": me,
                "confluence_user": probe.confluence_user(),
                "identity": identity_json(&probe),
            }))?
        );
    } else {
        println!(
            "Logged in: {} ({}) [id={}]",
            me.display_name,
            me.email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "Deployment: {} · Jira={} · Confluence={} · Account={}",
            effective_deployment, effective_site_url, effective_conf_url, account_id
        );
        if let Some(conf_account) = &candidate_profile.confluence_account_id {
            println!("Confluence credential: separate account '{conf_account}'");
        }
    }
    // stderr only, so --json output stays a single JSON document.
    if let Some(warning) = confluence_warning {
        eprintln!("{warning}");
    }
    Ok(())
}

/// Picks the user a login belongs to from the per-product probe (see
/// [IdentityProbe::login_user]).
pub(super) fn decide_login_identity(persona: Persona, probe: &IdentityProbe) -> Result<crate::models::UserSummary> {
    probe.login_user(persona)
}

/// Warning for a Data Center login whose Jira credential Confluence did not accept.
pub(super) fn confluence_login_warning(
    probe: &IdentityProbe,
    deployment: DeploymentKind,
    site_id: &str,
    persona: Persona,
) -> Option<String> {
    if deployment != DeploymentKind::DataCenter || probe.jira_user().is_none() {
        return None;
    }
    match &probe.confluence {
        Some(Err(failure)) if failure.is_unauthorized() => Some(format!(
            "Warning: Confluence did not accept this credential ({}). Data Center PATs are per application — a Jira PAT does not work on Confluence. Run \"{} login --site {site_id} --confluence-pat <PAT>\" or set ATLASSIAN_CONFLUENCE_PAT.",
            failure.label(),
            persona.command_name()
        )),
        _ => None,
    }
}

/// {"jira": "ok|anonymous|unauthorized|not_found|error|skipped", "confluence": "..."}.
pub(super) fn identity_json(probe: &IdentityProbe) -> Value {
    let (jira, confluence) = probe.status_labels();
    json!({ "jira": jira, "confluence": confluence })
}

/// Next steps appended to a login failure (credential rejected, or the hidden prompt
/// returned nothing — common when pasting into Windows consoles).
pub(super) fn login_failure_help(persona: Persona, site_url: &str, deployment: DeploymentKind) -> String {
    let cmd = persona.command_name();
    let url = if site_url.trim().is_empty() {
        "<URL>"
    } else {
        site_url.trim()
    };
    let direct = match deployment {
        DeploymentKind::DataCenter => {
            format!("{cmd} login --url {url} --pat <PAT>        or set ATLASSIAN_PAT=<PAT>")
        }
        DeploymentKind::Cloud => {
            format!("{cmd} login --url {url} --email <EMAIL> --token <API_TOKEN>")
        }
    };
    format!(
        "Could not verify the credential.\n  - If pasting into the hidden prompt fails in your terminal (common in Windows consoles), pass it directly:\n      {direct}\n  - Check paths, TLS roots and credentials with:  {cmd} doctor --probe"
    )
}

/// Appends "help" to an error while keeping its HTTP status (and so its JSON error code).
pub(super) fn with_login_help(err: anyhow::Error, help: &str) -> anyhow::Error {
    let text = format!("{err:#}\n{help}");
    match err.downcast_ref::<HttpStatusError>() {
        Some(http) => HttpStatusError {
            status: http.status,
            message: text,
            body: http.body.clone(),
        }
        .into(),
        None => anyhow::anyhow!(text),
    }
}

async fn cmd_login_oauth(
    cfg: &mut AppConfig,
    params: LoginParams,
    json_out: bool,
) -> Result<()> {
    let client_id = match params
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cfg.effective_oauth_client_id())
    {
        Some(id) => id,
        None => {
            if !io::stdin().is_terminal() {
                bail!("OAuth client ID is required. Pass `--client-id <ID>` or set ATLASSIAN_OAUTH_CLIENT_ID.");
            }
            let entered = prompt_line("Atlassian OAuth 2.0 (3LO) Client ID: ")?;
            if entered.trim().is_empty() {
                bail!("OAuth client ID cannot be empty.");
            }
            entered.trim().to_string()
        }
    };
    let client_secret = params
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cfg.effective_oauth_client_secret());

    cfg.oauth_client_id = Some(client_id.clone());
    if let Some(ref secret) = client_secret {
        cfg.oauth_client_secret = Some(secret.clone());
    }

    let account_id = params
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "cloud-oauth".to_string());

    let http = reqwest::Client::new();
    let opts = OAuthLoopbackOptions {
        client_id,
        client_secret,
        ..Default::default()
    };
    if !json_out {
        println!("Opening Atlassian Cloud OAuth 2.0 (3LO) consent flow in your browser…");
    }
    let (creds, resources) = run_oauth_loopback_login(&http, &opts).await?;
    let auth = AuthManager::for_account(&account_id);
    auth.save_credentials(&creds)?;

    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    // Keep a site configured the legacy way (top-level site_url) next to the new ones.
    cfg.materialize_legacy_site();
    let mut registered = Vec::with_capacity(resources.len());
    for res in &resources {
        let profile = res.to_site_profile(&account_id);
        let _ = storage.upsert_site(&SiteRecord {
            site_id: profile.id.clone(),
            name: profile.display_name().to_string(),
            site_url: profile.site_url.clone(),
            deployment: profile.deployment.as_str().to_string(),
            enabled: profile.enabled,
            last_synced_at: None,
            last_error: None,
        });
        cfg.upsert_site(profile.clone());
        registered.push(profile);
    }

    if let Some(first) = registered.first() {
        if cfg.default_site.is_none() {
            cfg.default_site = Some(first.id.clone());
        }
        if cfg.site_url.trim().is_empty() {
            cfg.site_url = first.site_url.clone();
            cfg.deployment = DeploymentKind::Cloud;
        }
    }
    cfg.save()?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "auth_kind": "cloud_oauth",
                "account_id": account_id,
                "sites_discovered": registered.len(),
                "sites": registered,
            }))?
        );
    } else {
        println!(
            "Cloud OAuth 2.0 sign-in complete (account='{account_id}'). Registered {} site(s):",
            registered.len()
        );
        for site in &registered {
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

async fn cmd_login_cloud_account(
    cfg: &mut AppConfig,
    params: LoginParams,
    json_out: bool,
) -> Result<()> {
    let email = match params
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(e) => e.to_string(),
        None => {
            if !io::stdin().is_terminal() {
                bail!("Email is required for `--cloud-account`. Pass `--email <email>`.");
            }
            let entered = prompt_line("Atlassian Cloud Account Email: ")?;
            if entered.trim().is_empty() {
                bail!("Email cannot be empty.");
            }
            entered.trim().to_string()
        }
    };
    let api_token = match params
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(t) => t.to_string(),
        None => {
            if !io::stdin().is_terminal() {
                bail!("API token is required for `--cloud-account`. Pass `--token <TOKEN>`.");
            }
            let help = login_failure_help(Persona::detect(), "", DeploymentKind::Cloud);
            let entered = rpassword::prompt_password("Atlassian Cloud API Token: ")
                .map_err(|err| anyhow::anyhow!("Failed to read API token: {err}\n{help}"))?;
            if entered.trim().is_empty() {
                bail!("API token cannot be empty.\n{help}");
            }
            entered.trim().to_string()
        }
    };

    let account_id = params
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "cloud-account".to_string());

    let creds = StoredCredentials::CloudAccountToken {
        email: email.clone(),
        api_token,
    };
    let auth = AuthManager::for_account(&account_id);
    auth.save_credentials(&creds)?;

    let mut added_site: Option<SiteProfile> = None;
    if let Some(raw_url) = params.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let site_url = config::normalize_base_url(raw_url);
        let site_id = config::slugify_site_id(&site_url);
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| site_id.clone());
        let profile = SiteProfile {
            id: site_id.clone(),
            name,
            site_url: site_url.clone(),
            confluence_url: params
                .confluence_url
                .as_deref()
                .map(config::normalize_base_url)
                .unwrap_or_default(),
            deployment: DeploymentKind::Cloud,
            cloud_id: None,
            account_id: account_id.clone(),
            enabled: true,
            scopes: Vec::new(),
            confluence_account_id: None,
        };
        let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
        let _ = storage.upsert_site(&SiteRecord {
            site_id: profile.id.clone(),
            name: profile.display_name().to_string(),
            site_url: profile.site_url.clone(),
            deployment: profile.deployment.as_str().to_string(),
            enabled: true,
            last_synced_at: None,
            last_error: None,
        });
        cfg.materialize_legacy_site();
        cfg.upsert_site(profile.clone());
        if cfg.default_site.is_none() {
            cfg.default_site = Some(site_id);
        }
        if cfg.site_url.trim().is_empty() {
            cfg.site_url = site_url;
            cfg.deployment = DeploymentKind::Cloud;
        }
        cfg.save()?;
        added_site = Some(profile);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "auth_kind": "cloud_account_token",
                "account_id": account_id,
                "email": email,
                "site": added_site,
            }))?
        );
    } else {
        println!("Saved shared Cloud account token '{account_id}' ({email}).");
        if let Some(site) = added_site {
            println!("Registered site '{}' ({}) using account '{account_id}'.", site.id, site.site_url);
        } else {
            println!(
                "Attach any Cloud site with: atlassian sites add https://<site>.atlassian.net --account {account_id}"
            );
        }
    }
    Ok(())
}

pub(super) fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let account_id = cfg
        .resolved_site_profile()
        .map(|s| s.account_id)
        .unwrap_or_else(|| cfg.profile.clone());
    let auth = AuthManager::for_account(&account_id);
    auth.delete_credentials()?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Logged out. Removed stored credentials.");
    }
    Ok(())
}


pub(super) async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let (auth, client, storage) = open_runtime(cfg, true)?;
    let probe = client.probe_identity().await;
    // Issue assignees are Jira users, so only the Jira identity serves `issues --offline --mine`.
    if let Some(user) = probe.jira_user() {
        let _ = storage.save_identity_for_site(client.site_id(), user);
    }
    // Non-zero exit only when no product accepts the credential.
    let me = match probe.jira_user().or(probe.confluence_user()) {
        Some(user) => user.clone(),
        None => {
            return Err(decide_login_identity(persona, &probe).err().unwrap_or_else(|| {
                anyhow::anyhow!("Neither Jira nor Confluence accepted the credential.")
            }))
        }
    };
    let effective_site_url = client.site_url();
    let effective_conf_url = client.confluence_root_url();
    let effective_deployment = client.deployment().as_str();
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona.as_str(),
                "site_id": client.site_id(),
                "credential_account": auth.profile(),
                "confluence_credential_account": client.confluence_credential_account(),
                "deployment": effective_deployment,
                "site_url": effective_site_url,
                "confluence_url": effective_conf_url,
                "auth_status": auth.status_summary(),
                "user": me,
                "confluence_user": probe.confluence_user(),
                "identity": identity_json(&probe),
            }))?
        );
    } else {
        println!("persona: {}", persona.as_str());
        println!(
            "User: {} ({})",
            me.display_name,
            me.email.as_deref().unwrap_or("-")
        );
        println!("id: {}", me.id);
        match &probe.confluence {
            Some(Ok(user)) => println!(
                "Confluence user: {} ({}) [id={}]",
                user.display_name,
                user.email.as_deref().unwrap_or("-"),
                user.id
            ),
            Some(Err(failure)) => println!("Confluence user: ({})", failure.label()),
            None => {}
        }
        if let Some(Err(failure)) = &probe.jira {
            println!("Jira user: ({})", failure.label());
        }
        println!("Deployment: {effective_deployment}");
        println!("Jira URL: {effective_site_url}");
        println!("Confluence URL: {effective_conf_url}");
        println!("Auth: {}", auth.status_summary());
    }
    Ok(())
}

