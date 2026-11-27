use super::*;
use super::jira::{offline_issue_filter, OFFLINE_JQL_ERROR, OFFLINE_MINE_ERROR};
use super::login::{
    confluence_login_warning, decide_login_identity, identity_json, login_failure_help,
    with_login_help,
};
use super::sites::{config_dir_source, doctor_report, DoctorFacts};
use crate::atlassian::client::IdentityProbe;
use crate::config::{AppConfig, DeploymentKind, Persona};
use crate::models::{BoardSummary, IssueSummary, StatusCategory};
use crate::shell_alias::{AliasCommand, ShellKind};
use crate::storage::IssueListFilter;
use clap::Parser;
use serde_json::Value;
use std::ffi::OsString;
use crate::atlassian::client::ProbeFailure;
use crate::models::{BoardColumn, IssueStatus, UserSummary};

#[test]
fn offline_issue_query_rejects_jql() {
    let cfg = AppConfig {
        default_project: Some("ENG".into()),
        ..AppConfig::default()
    };
    let err = offline_issue_filter(&cfg, None, Some("project = OPS"), false, None, None)
        .unwrap_err();
    assert_eq!(err.to_string(), OFFLINE_JQL_ERROR);
    assert!(OFFLINE_JQL_ERROR.contains("atlassian search <text>"));

    // Blank JQL is no JQL; the default project and the status filter apply.
    assert_eq!(
        offline_issue_filter(&cfg, None, Some("  "), false, Some(" In Progress "), None).unwrap(),
        IssueListFilter {
            project: Some("ENG"),
            status_contains: Some("In Progress"),
            ..Default::default()
        }
    );
    assert_eq!(
        offline_issue_filter(&cfg, Some("OPS"), None, false, None, None).unwrap().project,
        Some("OPS")
    );

    // --mine filters by the saved identity and refuses to guess without one.
    let err = offline_issue_filter(&cfg, None, None, true, None, None).unwrap_err();
    assert_eq!(err.to_string(), OFFLINE_MINE_ERROR);
    assert_eq!(
        offline_issue_filter(&cfg, None, None, true, None, Some("acct-1")).unwrap().assignee_id,
        Some("acct-1")
    );
    assert_eq!(
        offline_issue_filter(&cfg, None, None, false, None, Some("acct-1")).unwrap().assignee_id,
        None
    );
}

fn probe_user(id: &str) -> UserSummary {
    UserSummary {
        id: id.into(),
        display_name: format!("User {id}"),
        email: None,
        avatar_url: None,
        active: true,
    }
}

fn probe_failure(status: u16, anonymous: bool) -> ProbeFailure {
    ProbeFailure {
        status: Some(status),
        anonymous,
        message: format!("request failed (HTTP {status})"),
    }
}

#[test]
fn decide_login_identity_accepts_jira_user() {
    let probe = IdentityProbe {
        jira: Some(Ok(probe_user("jdoe"))),
        confluence: Some(Ok(probe_user("jdoe-wiki"))),
    };
    assert_eq!(decide_login_identity(Persona::Jira, &probe).unwrap().id, "jdoe");
    assert_eq!(
        confluence_login_warning(&probe, DeploymentKind::DataCenter, "corp", Persona::Jira),
        None
    );
}

#[test]
fn decide_login_identity_accepts_confluence_only_site_on_jira_404() {
    let probe = IdentityProbe {
        jira: Some(Err(probe_failure(404, false))),
        confluence: Some(Ok(probe_user("wiki-user"))),
    };
    assert_eq!(decide_login_identity(Persona::Jira, &probe).unwrap().id, "wiki-user");
    assert_eq!(
        decide_login_identity(Persona::Confluence, &probe).unwrap().id,
        "wiki-user"
    );
    assert_eq!(identity_json(&probe), json!({ "jira": "not_found", "confluence": "ok" }));
}

#[test]
fn decide_login_identity_jira_ok_confluence_anonymous_is_ok_with_warning() {
    let probe = IdentityProbe {
        jira: Some(Ok(probe_user("jdoe"))),
        confluence: Some(Err(probe_failure(401, true))),
    };
    assert_eq!(decide_login_identity(Persona::Jira, &probe).unwrap().id, "jdoe");
    let warning =
        confluence_login_warning(&probe, DeploymentKind::DataCenter, "corp", Persona::Jira)
            .expect("a Data Center login must warn");
    assert!(warning.contains("atlassian login --site corp --confluence-pat <PAT>"), "{warning}");
    assert!(warning.contains("ATLASSIAN_CONFLUENCE_PAT"), "{warning}");
    // Cloud uses one credential for both products: no split-PAT warning there.
    assert_eq!(
        confluence_login_warning(&probe, DeploymentKind::Cloud, "acme", Persona::Jira),
        None
    );
    // A Confluence 404 (no Confluence on this host) is not a credential problem.
    let no_wiki = IdentityProbe {
        jira: Some(Ok(probe_user("jdoe"))),
        confluence: Some(Err(probe_failure(404, false))),
    };
    assert_eq!(
        confluence_login_warning(&no_wiki, DeploymentKind::DataCenter, "corp", Persona::Jira),
        None
    );
    // The confluence persona logs into Confluence, so its rejection is a failure.
    assert!(decide_login_identity(Persona::Confluence, &probe).is_err());
}

#[test]
fn decide_login_identity_fails_when_both_products_reject() {
    let probe = IdentityProbe {
        jira: Some(Err(probe_failure(401, false))),
        confluence: Some(Err(probe_failure(401, true))),
    };
    let err = decide_login_identity(Persona::Jira, &probe).unwrap_err();
    let helped = with_login_help(
        err,
        &login_failure_help(Persona::Jira, "https://jira.corp", DeploymentKind::DataCenter),
    );
    // The HTTP status (JSON error code "unauthorized") survives the appended help.
    assert_eq!(error_payload(&helped)["code"], "unauthorized");
    let text = format!("{helped:#}");
    assert!(text.contains("request failed (HTTP 401)"), "{text}");
    assert!(text.contains("atlassian login --url https://jira.corp --pat <PAT>"), "{text}");
    assert!(text.contains("ATLASSIAN_PAT=<PAT>"), "{text}");
    assert!(text.contains("atlassian doctor --probe"), "{text}");

    let neither = IdentityProbe {
        jira: Some(Err(probe_failure(404, false))),
        confluence: Some(Err(probe_failure(404, false))),
    };
    assert_eq!(
        error_payload(&decide_login_identity(Persona::Jira, &neither).unwrap_err())["code"],
        "not_found"
    );
}

#[test]
fn login_failure_help_matches_deployment_and_persona() {
    let cloud = login_failure_help(Persona::Confluence, "", DeploymentKind::Cloud);
    assert!(cloud.starts_with("Could not verify the credential."), "{cloud}");
    assert!(
        cloud.contains("confluence login --url <URL> --email <EMAIL> --token <API_TOKEN>"),
        "{cloud}"
    );
    assert!(cloud.contains("confluence doctor --probe"), "{cloud}");
    assert!(cloud.contains("Windows consoles"), "{cloud}");
}

#[test]
fn login_accepts_confluence_pat_flags() {
    let cli = Cli::try_parse_from([
        "atlassian",
        "login",
        "--site",
        "corp",
        "--confluence-pat",
        "PAT-C",
        "--confluence-account",
        "corp-wiki",
    ])
    .unwrap();
    assert_eq!(cli.site.as_deref(), Some("corp"));
    match cli.command {
        Some(Command::Shared(SharedCommand::Login {
            confluence_pat,
            confluence_account,
            pat,
            ..
        })) => {
            assert_eq!(confluence_pat.as_deref(), Some("PAT-C"));
            assert_eq!(confluence_account.as_deref(), Some("corp-wiki"));
            assert_eq!(pat, None);
        }
        other => panic!("expected login, got {other:?}"),
    }
    let conf = ConfluenceCli::try_parse_from(["confluence", "login", "--confluence-pat", "PAT-C"])
        .unwrap();
    assert!(matches!(
        conf.command,
        Some(ConfluenceCommand::Shared(SharedCommand::Login { confluence_pat: Some(_), .. }))
    ));
}

#[test]
fn serve_accepts_repeated_cors_origin() {
    let cli = Cli::try_parse_from([
        "atlassian",
        "serve",
        "--cors-origin",
        "http://localhost:5173",
        "--cors-origin",
        "http://localhost:3000",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Shared(SharedCommand::Serve { cors_origin, token, .. })) => {
            assert_eq!(cors_origin, vec!["http://localhost:5173", "http://localhost:3000"]);
            assert_eq!(token, None);
        }
        other => panic!("expected serve, got {other:?}"),
    }
    let plain = Cli::try_parse_from(["atlassian", "serve"]).unwrap();
    assert!(matches!(
        plain.command,
        Some(Command::Shared(SharedCommand::Serve { cors_origin, .. })) if cors_origin.is_empty()
    ));
}

#[test]
fn doctor_reports_config_dir_override() {
    assert_eq!(config_dir_source(Some("D:\\atl")), "env:ATLASSIAN_CONFIG_DIR");
    assert_eq!(config_dir_source(Some("  ")), "default");
    assert_eq!(config_dir_source(None), "default");

    let probe = IdentityProbe {
        jira: Some(Err(probe_failure(404, false))),
        confluence: Some(Ok(probe_user("wiki-user"))),
    };
    let missing = Path::new("definitely-missing-doctor-path");
    let facts = DoctorFacts {
        persona: Persona::Jira,
        profile: "default",
        site_id: "corp",
        credential_account: "corp",
        confluence_credential_account: Some("corp-confluence"),
        env_credentials: json!({ "present": false, "applies_to_site": null }),
        tls: json!({ "native_roots": true, "extra_ca_sources": [] }),
        deployment: "datacenter",
        site_url: "https://jira.corp",
        confluence_url: "https://wiki.corp",
        logged_in: true,
        auth_status: "PAT",
        config_dir: Path::new("D:\\atl"),
        config_dir_source: config_dir_source(Some("D:\\atl")),
        data_dir: Path::new("D:\\atl\\default"),
        config_path: missing,
        credentials_fallback_path: missing,
        db_path: missing,
        download_dir: missing,
        cache_counts: Value::Null,
        probe: Some(&probe),
    };
    let report = doctor_report(&facts);
    assert_eq!(report["config_dir"], "D:\\atl");
    assert_eq!(report["config_dir_source"], "env:ATLASSIAN_CONFIG_DIR");
    assert_eq!(report["data_dir"], "D:\\atl\\default");
    assert_eq!(report["config_exists"], false);
    assert_eq!(report["confluence_credential_account"], "corp-confluence");
    // Jira keeps probe_me / probe_error; Confluence gets its own pair.
    assert_eq!(report["probe_me"], Value::Null);
    assert_eq!(report["probe_error"], "request failed (HTTP 404)");
    assert_eq!(report["probe_confluence_me"]["id"], "wiki-user");
    assert_eq!(report["probe_confluence_error"], Value::Null);

    let not_probed = doctor_report(&DoctorFacts { probe: None, ..facts });
    for key in ["probe_me", "probe_error", "probe_confluence_me", "probe_confluence_error"] {
        assert_eq!(not_probed[key], Value::Null, "{key}");
    }
}

#[test]
fn parses_jira_and_confluence_persona_clis() {
    // Jira CLI commands
    let cli = Cli::try_parse_from([
        "jira",
        "login",
        "--url",
        "https://acme.atlassian.net",
        "--email",
        "me@acme.com",
        "--token",
        "tok",
        "--deployment",
        "cloud",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Login {
            deployment: Some(DeploymentArg::Cloud),
            json: true,
            ..
        })
    ));

    let cli = Cli::try_parse_from([
        "jira", "issues", "--project", "ENG", "--mine", "--offline", "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Issues {
            mine: true,
            offline: true,
            json: true,
            ..
        })
    ));

    let cli = Cli::try_parse_from([
        "jira",
        "create",
        "--project",
        "ENG",
        "--type",
        "Bug",
        "--summary",
        "Fix crash",
        "--label",
        "cli",
        "--label",
        "urgent",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Create {
            issue_type, label, ..
        }) => {
            assert_eq!(issue_type, "Bug");
            assert_eq!(label, vec!["cli", "urgent"]);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = Cli::try_parse_from(["jira", "assign", "ENG-1", "--to", "me", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Assign {
            to: Some(_),
            unassign: false,
            json: true,
            ..
        })
    ));

    // Cross-mounted Confluence under `atlassian wiki` and alias `atlassian conf`
    let cli = Cli::try_parse_from(["jira", "wiki", "spaces", "--refresh"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Wiki {
            command: WikiCommand::Spaces { refresh: true, .. }
        }
    ));
    let cli = Cli::try_parse_from(["jira", "conf", "view", "9001", "--offline"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Wiki {
            command: WikiCommand::View { offline: true, .. }
        }
    ));

    // Serve & MCP
    let cli = Cli::try_parse_from([
        "jira", "serve", "--bind", "0.0.0.0", "--port", "9000", "--token", "sec",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Shared(SharedCommand::Serve {
            bind, port, token, ..
        }) => {
            assert_eq!(bind, "0.0.0.0");
            assert_eq!(port, 9000);
            assert_eq!(token.as_deref(), Some("sec"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    let cli = Cli::try_parse_from(["jira", "mcp-server"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Mcp { read_only: false })
    ));

    // Daemon subcommands for both Jira & Confluence personas
    let cli = Cli::try_parse_from(["jira", "daemon", "start", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Start { json: true }
        })
    ));
    let cli = Cli::try_parse_from(["jira", "daemon", "stop"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Stop { json: false }
        })
    ));
    let cli = Cli::try_parse_from(["jira", "daemon", "status", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Status { json: true }
        })
    ));
    let cli = Cli::try_parse_from(["jira", "daemon", "restart"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Restart { json: false }
        })
    ));
    let cli = Cli::try_parse_from(["jira", "daemon", "run"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Run
        })
    ));

    let conf = ConfluenceCli::try_parse_from(["confluence", "daemon", "status", "--json"]).unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Status { json: true }
        })
    ));
    let conf = ConfluenceCli::try_parse_from(["confluence", "daemon", "run"]).unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Shared(SharedCommand::Daemon {
            action: DaemonCommand::Run
        })
    ));

    // Confluence CLI commands
    let conf = ConfluenceCli::try_parse_from([
        "confluence",
        "pages",
        "ENG",
        "--limit",
        "25",
        "--offline",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Wiki(WikiCommand::Pages {
            limit: 25,
            offline: true,
            json: true,
            ..
        })
    ));

    let conf = ConfluenceCli::try_parse_from([
        "confluence",
        "edit",
        "9001",
        "--title",
        "Updated",
        "--message",
        "v2",
    ])
    .unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Wiki(WikiCommand::Edit { .. })
    ));

    // Cross-mounted Jira under `confluence jira`
    let conf = ConfluenceCli::try_parse_from([
        "confluence",
        "jira",
        "view",
        "ENG-42",
        "--preview-images",
    ])
    .unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Jira {
            command: JiraSubcommand::View {
                preview_images: true,
                ..
            }
        }
    ));

    // Multi-site global --site option, OAuth / Cloud Account login, and Sites subcommands
    let cli = Cli::try_parse_from(["jira", "--site", "acme-cloud", "issues", "--mine"]).unwrap();
    assert_eq!(cli.site.as_deref(), Some("acme-cloud"));
    assert!(matches!(cli.command.unwrap(), Command::Jira(JiraSubcommand::Issues { mine: true, .. })));

    let cli = Cli::try_parse_from(["jira", "issues", "--site", "corp-dc", "--offline"]).unwrap();
    assert_eq!(cli.site.as_deref(), Some("corp-dc"));

    let cli = Cli::try_parse_from([
        "jira",
        "login",
        "--oauth",
        "--client-id",
        "cid-123",
        "--client-secret",
        "sec-456",
        "--account",
        "work-cloud",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Login {
            oauth: true,
            cloud_account: false,
            client_id: Some(_),
            client_secret: Some(_),
            account: Some(_),
            json: true,
            ..
        })
    ));

    let cli = Cli::try_parse_from([
        "jira",
        "login",
        "--cloud-account",
        "--email",
        "dev@acme.com",
        "--token",
        "tok",
        "--account",
        "shared",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Login {
            oauth: false,
            cloud_account: true,
            ..
        })
    ));

    let cli = Cli::try_parse_from(["jira", "sites", "list", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::List { json: true })
    ));

    let cli = Cli::try_parse_from([
        "jira",
        "sites",
        "add",
        "https://jira.corp.internal",
        "--name",
        "Corp DC",
        "--pat",
        "dc-pat",
        "--confluence-url",
        "https://wiki.corp.internal",
        "--deployment",
        "dc",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Shared(SharedCommand::Sites { action }) => match *action {
            SitesCommand::Add(args) => {
                assert_eq!(args.url, "https://jira.corp.internal");
                assert_eq!(args.name.as_deref(), Some("Corp DC"));
                assert_eq!(args.pat.as_deref(), Some("dc-pat"));
                assert_eq!(args.deployment, Some(DeploymentArg::Datacenter));
            }
            other => panic!("unexpected sites action: {other:?}"),
        },
        other => panic!("unexpected: {other:?}"),
    }

    let cli = Cli::try_parse_from(["jira", "sites", "discover", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::Discover { json: true, .. })
    ));

    let cli = Cli::try_parse_from(["jira", "sites", "default", "acme"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::Default { .. })
    ));

    let cli = Cli::try_parse_from(["jira", "sites", "enable", "acme"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::Enable { .. })
    ));

    let cli = Cli::try_parse_from(["jira", "sites", "disable", "acme"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::Disable { .. })
    ));

    let cli = Cli::try_parse_from(["jira", "sites", "remove", "acme", "--yes"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::Remove { yes: true, .. })
    ));

    let conf = ConfluenceCli::try_parse_from([
        "confluence",
        "--site",
        "acme-cloud",
        "sites",
        "list",
        "--json",
    ])
    .unwrap();
    assert_eq!(conf.site.as_deref(), Some("acme-cloud"));
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Shared(SharedCommand::Sites { action }) if matches!(*action, SitesCommand::List { json: true })
    ));
}

#[test]
fn delete_gate_requires_yes_or_confirmation() {
    delete_gate(true, false, None).unwrap();
    delete_gate(true, true, None).unwrap();
    assert!(delete_gate(false, true, Some("yes")).is_err());
    delete_gate(false, false, Some("y")).unwrap();
    delete_gate(false, false, Some("YES")).unwrap();
    assert!(delete_gate(false, false, Some("no")).is_err());
    assert!(delete_gate(false, false, None).is_err());
}

#[test]
fn formats_kanban_board_and_detects_jira_keys() {
    assert!(looks_like_jira_key("ENG-123"));
    assert!(looks_like_jira_key("ABC_1-9"));
    assert!(!looks_like_jira_key("9001"));
    assert!(!looks_like_jira_key("ENG-abc"));

    let board = BoardSummary {
        id: 7,
        name: "Core Scrum".to_string(),
        board_type: "scrum".to_string(),
        columns: vec![
            BoardColumn {
                name: "To Do".to_string(),
                status_names: vec!["To Do".to_string()],
                category_hint: StatusCategory::Todo,
                ..Default::default()
            },
            BoardColumn {
                name: "Done".to_string(),
                status_names: vec!["Done".to_string()],
                category_hint: StatusCategory::Done,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let issues = vec![IssueSummary {
        key: "ENG-1".to_string(),
        summary: "Ship Kanban view".to_string(),
        status: IssueStatus {
            name: "To Do".to_string(),
            category: StatusCategory::Todo,
            ..Default::default()
        },
        assignee: Some(UserSummary {
            display_name: "김철수".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let rendered = format_kanban_board(&board, None, &issues);
    assert!(rendered.contains("Core Scrum"));
    assert!(rendered.contains("ENG-1"));
    assert!(rendered.contains("To Do (1)"));
}

#[test]
fn parses_hyphen_leading_free_text_and_preserves_flags_missing_values_and_help() {
    use clap::error::ErrorKind;

    let bullet = "- 목록 항목";
    let multiline = "- 첫째 항목\n- 둘째 항목";

    // 1. All 15 audited CLI templates parse exact values.
    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "edit", "FLS-14", "--append", bullet, "--dry-run", "--json"])
            .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { key, body, json, .. }) => {
            assert_eq!(key, "FLS-14");
            assert_eq!(body.append.as_deref(), Some(bullet));
            assert!(body.dry_run);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "edit", "FLS-14", "--body", multiline, "--json"])
            .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, json, .. }) => {
            assert_eq!(body.body.as_deref(), Some(multiline));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "edit", "FLS-14", "--replace", "old", bullet, "--dry-run"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, .. }) => {
            assert_eq!(body.replace, Some(vec!["old".into(), bullet.into()]));
            assert!(body.dry_run);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "edit", "FLS-14", "--replace", bullet, "new"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, .. }) => {
            assert_eq!(body.replace, Some(vec![bullet.into(), "new".into()]));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "edit", "FLS-14", "--replace", "- old", "- new", "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, json, .. }) => {
            assert_eq!(body.replace, Some(vec!["- old".into(), "- new".into()]));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "create", "--summary", bullet, "--body", multiline, "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Create { summary, body, json, .. }) => {
            assert_eq!(summary.as_deref(), Some(bullet));
            assert_eq!(body.as_deref(), Some(multiline));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "comment", "FLS-14", bullet, "--json"])
            .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Comment { key, body, json }) => {
            assert_eq!(key, "FLS-14");
            assert_eq!(body, bullet);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "wiki", "create", "--title", bullet, "--body", multiline, "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Wiki {
            command: WikiCommand::Create { title, body, json, .. },
        } => {
            assert_eq!(title.as_deref(), Some(bullet));
            assert_eq!(body.as_deref(), Some(multiline));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "wiki", "edit", "502", "--append", bullet, "--dry-run"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Wiki {
            command: WikiCommand::Edit { body, .. },
        } => {
            assert_eq!(body.body.append.as_deref(), Some(bullet));
            assert!(body.body.dry_run);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        [
            "atlassian",
            "wiki",
            "edit",
            "502",
            "--replace-section",
            "- 제목",
            multiline,
            "--prev-version",
            "7",
            "--dry-run",
            "--json",
        ],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Wiki {
            command: WikiCommand::Edit { body, prev_version, json, .. },
        } => {
            assert_eq!(
                body.replace_section,
                Some(vec!["- 제목".into(), multiline.into()])
            );
            assert_eq!(prev_version, Some(7));
            assert!(body.body.dry_run);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "wiki", "edit", "502", "--body", bullet, "--title", "- 제목", "--message", "- 메모"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Wiki {
            command: WikiCommand::Edit { title, body, message, .. },
        } => {
            assert_eq!(title.as_deref(), Some("- 제목"));
            assert_eq!(body.body.body.as_deref(), Some(bullet));
            assert_eq!(message.as_deref(), Some("- 메모"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "wiki", "comment", "502", bullet, "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Wiki {
            command: WikiCommand::Comment { page_id, body, json },
        } => {
            assert_eq!(page_id, "502");
            assert_eq!(body, bullet);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "log-work", "FLS-14", "--seconds", "60", "--comment", bullet, "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Workflow(JiraWorkflow::LogWork { seconds, comment, json, .. })) => {
            assert_eq!(seconds, 60);
            assert_eq!(comment.as_deref(), Some(bullet));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "transition", "FLS-14", "Done", "--comment", bullet, "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Transition { comment, json, .. }) => {
            assert_eq!(comment.as_deref(), Some(bullet));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "edit", "FLS-14", "--summary", bullet])
            .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { summary, .. }) => assert_eq!(summary.as_deref(), Some(bullet)),
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "search", bullet, "--remote", "--json"])
            .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Search { query, remote, json, .. }) => {
            assert_eq!(query, bullet);
            assert!(remote);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 2. Confluence persona (`confluence` / `conf`) and `confluence jira` subcommands.
    let conf: ConfluenceCli = try_parse_cli_from(
        "confluence",
        ["confluence", "edit", "502", "--title", bullet, "--replace-section", "Title", multiline, "--dry-run"],
    )
    .unwrap();
    match conf.command.unwrap() {
        ConfluenceCommand::Wiki(WikiCommand::Edit { title, body, .. }) => {
            assert_eq!(title.as_deref(), Some(bullet));
            assert_eq!(
                body.replace_section,
                Some(vec!["Title".into(), multiline.into()])
            );
            assert!(body.body.dry_run);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let conf: ConfluenceCli = try_parse_cli_from(
        "confluence",
        ["confluence", "comment", "502", bullet, "--json"],
    )
    .unwrap();
    match conf.command.unwrap() {
        ConfluenceCommand::Wiki(WikiCommand::Comment { page_id, body, json }) => {
            assert_eq!(page_id, "502");
            assert_eq!(body, bullet);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let conf: ConfluenceCli = try_parse_cli_from(
        "confluence",
        ["confluence", "search", bullet, "--remote", "--json"],
    )
    .unwrap();
    match conf.command.unwrap() {
        ConfluenceCommand::Wiki(WikiCommand::Search { query, remote, json, .. }) => {
            assert_eq!(query, bullet);
            assert!(remote);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let conf: ConfluenceCli = try_parse_cli_from(
        "confluence",
        ["confluence", "jira", "edit", "FLS-14", "--append", bullet, "--dry-run"],
    )
    .unwrap();
    match conf.command.unwrap() {
        ConfluenceCommand::Jira {
            command: JiraSubcommand::Edit { body, .. },
        } => {
            assert_eq!(body.append.as_deref(), Some(bullet));
            assert!(body.dry_run);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 3. Equals syntax for explicit flag-looking text and `--` for positional text.
    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "edit", "FLS-14", "--append=--dry-run", "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, json, .. }) => {
            assert_eq!(body.append.as_deref(), Some("--dry-run"));
            assert!(!body.dry_run);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "edit", "FLS-14", "--append=--help"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Edit { body, .. }) => assert_eq!(body.append.as_deref(), Some("--help")),
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "comment", "FLS-14", "--", "--json"],
    )
    .unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Comment { body, json, .. }) => {
            assert_eq!(body, "--json");
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "search", "--", "--help"]).unwrap();
    match cli.command.unwrap() {
        Command::Jira(JiraSubcommand::Search { query, .. }) => assert_eq!(query, "--help"),
        other => panic!("unexpected: {other:?}"),
    }

    // 4. Missing values before known flags MUST fail instead of swallowing the flag.
    for missing_case in [
        vec!["atlassian", "edit", "FLS-14", "--append", "--dry-run"],
        vec!["atlassian", "edit", "FLS-14", "--append", "--json"],
        vec!["atlassian", "edit", "FLS-14", "--append", "--site", "acme"],
        vec!["atlassian", "edit", "FLS-14", "--append", "--"],
        vec!["atlassian", "edit", "FLS-14", "--body", "--dry-run"],
        vec!["atlassian", "edit", "FLS-14", "--summary", "--json"],
        vec!["atlassian", "edit", "FLS-14", "--replace", "old", "--dry-run"],
        vec!["atlassian", "edit", "FLS-14", "--replace", "- old", "--dry-run"],
        vec!["atlassian", "edit", "FLS-14", "--replace", "--dry-run", "new"],
        vec!["atlassian", "wiki", "edit", "502", "--replace-section", "Title", "--dry-run"],
        vec!["atlassian", "wiki", "edit", "502", "--replace-section", "- 제목", "--json"],
        vec!["atlassian", "wiki", "edit", "502", "--append", "--dry-run"],
        vec!["atlassian", "conf", "edit", "502", "--append", "--dry-run"],
        vec!["atlassian", "--site", "acme", "edit", "FLS-14", "--append", "--dry-run"],
        vec!["atlassian", "wiki", "edit", "502", "--title", "--json"],
        vec!["atlassian", "create", "--summary", "Test", "--body", "--json"],
        vec!["atlassian", "new", "--summary", "Test", "--body", "--json"],
        vec!["atlassian", "log-work", "FLS-14", "--seconds", "60", "--comment", "--json"],
        vec!["atlassian", "comment", "FLS-14", "--json"],
        vec!["atlassian", "search", "--remote"],
    ] {
        let err = try_parse_cli_from::<Cli>("atlassian", missing_case.clone()).unwrap_err();
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected parse error, got help for {missing_case:?}: {err}"
        );
    }

    for conf_missing_case in [
        vec!["confluence", "edit", "502", "--append", "--dry-run"],
        vec!["confluence", "edit", "502", "--replace-section", "Title", "--dry-run"],
        vec!["confluence", "jira", "edit", "FLS-14", "--append", "--dry-run"],
        vec!["confluence", "comment", "502", "--json"],
        vec!["confluence", "search", "--remote"],
    ] {
        let err =
            try_parse_cli_from::<ConfluenceCli>("confluence", conf_missing_case.clone())
                .unwrap_err();
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected parse error, got help for {conf_missing_case:?}: {err}"
        );
    }

    // 5. Actual help invocations still display help.
    for help_case in [
        vec!["atlassian", "edit", "--help"],
        vec!["atlassian", "edit", "FLS-14", "--append", "--help"],
        vec!["atlassian", "edit", "FLS-14", "--append", "-h"],
        vec!["atlassian", "edit", "FLS-14", "--replace", "- old", "--help"],
        vec!["atlassian", "wiki", "edit", "502", "--replace-section", "Title", "-h"],
        vec!["atlassian", "comment", "FLS-14", "--help"],
        vec!["atlassian", "comment", "FLS-14", "-h"],
        vec!["atlassian", "search", "--help"],
        vec!["atlassian", "search", "-h"],
        vec!["atlassian", "mcp", "--help"],
    ] {
        let err = try_parse_cli_from::<Cli>("atlassian", help_case.clone()).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected help for {help_case:?}, got: {err}"
        );
    }

    // 6. IDs, numbers, and enums keep existing parser behavior.
    for unchanged_case in [
        vec!["atlassian", "log-work", "FLS-14", "--seconds", "-60"],
        vec!["atlassian", "wiki", "edit", "502", "--prev-version", "-1"],
        vec!["atlassian", "login", "--deployment", "-cloud"],
        vec!["atlassian", "edit", "-FLS-14", "--summary", "Test"],
    ] {
        assert!(
            try_parse_cli_from::<Cli>("atlassian", unchanged_case.clone()).is_err(),
            "expected rejection for {unchanged_case:?}"
        );
    }

    // 7. MCP `--read-only` flag on both entrypoints.
    let cli: Cli = try_parse_cli_from("atlassian", ["atlassian", "mcp", "--read-only"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Mcp { read_only: true })
    ));
    let conf: ConfluenceCli =
        try_parse_cli_from("confluence", ["confluence", "mcp", "--read-only"]).unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Shared(SharedCommand::Mcp { read_only: true })
    ));
}

#[test]
fn parses_smart_view_shortcuts_short_flags_and_aliases() {
    // 1. Smart Jira issue key shortcut (`atlassian ENG-42 -jf`, with global `--site`)
    let cli: Cli = try_parse_cli_from("atlassian", ["atlassian", "ENG-42", "-jf"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::View {
            ref key,
            json: true,
            full: true,
            offline: false,
            ..
        }) if key == "ENG-42"
    ));

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "--site", "acme", "PLAT_2-109", "-oj"],
    )
    .unwrap();
    assert_eq!(cli.site.as_deref(), Some("acme"));
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::View {
            ref key,
            offline: true,
            json: true,
            ..
        }) if key == "PLAT_2-109"
    ));

    // 2. Smart Confluence page ID shortcut (`conf 40211 -j`, `atlassian wiki 40211 -j`, `conf jira ENG-42 -j`)
    let conf: ConfluenceCli = try_parse_cli_from("conf", ["conf", "40211", "-jf"]).unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Wiki(WikiCommand::View {
            ref page_id,
            json: true,
            full: true,
            ..
        }) if page_id == "40211"
    ));

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "wiki", "40211", "-j"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Wiki {
            command: WikiCommand::View {
                ref page_id,
                json: true,
                ..
            }
        } if page_id == "40211"
    ));

    let conf: ConfluenceCli =
        try_parse_cli_from("conf", ["conf", "jira", "ENG-42", "-j"]).unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Jira {
            command: JiraSubcommand::View {
                ref key,
                json: true,
                ..
            }
        } if key == "ENG-42"
    ));

    // Non-key unknown subcommand still fails cleanly
    assert!(try_parse_cli_from::<Cli>("atlassian", ["atlassian", "bogus"]).is_err());

    // 3. Combined short flags & subcommand short aliases
    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "ls", "-mj", "-p", "ENG", "-n", "15"])
            .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Issues {
            mine: true,
            json: true,
            limit: 15,
            ref project,
            ..
        }) if project.as_deref() == Some("ENG")
    ));

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "q", "canary", "-rj", "-n", "10"])
            .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Search {
            ref query,
            remote: true,
            json: true,
            limit: 10,
            ..
        }) if query == "canary"
    ));

    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "mv", "ENG-42", "In Review", "-c", "LGTM", "-j"],
    )
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Transition {
            ref key,
            ref status,
            ref comment,
            json: true,
            ..
        }) if key == "ENG-42"
            && status.as_deref() == Some("In Review")
            && comment.as_deref() == Some("LGTM")
    ));

    // 4. Positional shortcuts for Create & Assign
    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "new", "Fix login", "-p", "ENG", "-a", "me", "-j"],
    )
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Create {
            ref summary_pos,
            summary: None,
            ref project,
            ref assignee,
            json: true,
            ..
        }) if summary_pos.as_deref() == Some("Fix login")
            && project.as_deref() == Some("ENG")
            && assignee.as_deref() == Some("me")
    ));

    let conf: ConfluenceCli = try_parse_cli_from(
        "conf",
        ["conf", "new", "Release Notes", "-s", "ENG", "-j"],
    )
    .unwrap();
    assert!(matches!(
        conf.command.unwrap(),
        ConfluenceCommand::Wiki(WikiCommand::Create {
            ref title_pos,
            title: None,
            ref space,
            json: true,
            ..
        }) if title_pos.as_deref() == Some("Release Notes") && space.as_deref() == Some("ENG")
    ));

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "as", "ENG-42", "me", "-j"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Jira(JiraSubcommand::Assign {
            ref key,
            ref assignee_pos,
            to: None,
            json: true,
            ..
        }) if key == "ENG-42" && assignee_pos.as_deref() == Some("me")
    ));

    // 5. Alias & Init subcommands
    let cli: Cli = try_parse_cli_from(
        "atlassian",
        ["atlassian", "alias", "install", "--shell", "powershell", "--dry-run", "-j"],
    )
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Alias {
            action: AliasCommand::Install {
                shell: Some(ShellKind::Powershell),
                dry_run: true,
                json: true,
                ref jira,
                ref conf,
                ..
            }
        }) if jira == "j" && conf == "cf"
    ));

    let cli: Cli =
        try_parse_cli_from("atlassian", ["atlassian", "init", "zsh"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Shared(SharedCommand::Init {
            shell: ShellKind::Zsh,
            ref jira,
            ref conf,
            force: false,
        }) if jira == "j" && conf == "cf"
    ));

    for (raw_shell, expected_shell) in [
        ("tcsh", ShellKind::Tcsh),
        ("csh", ShellKind::Csh),
        ("sh", ShellKind::Sh),
        ("dash", ShellKind::Sh),
        ("ash", ShellKind::Sh),
        ("ksh", ShellKind::Sh),
    ] {
        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "init", raw_shell]).unwrap();
        assert!(
            matches!(
                cli.command.unwrap(),
                Command::Shared(SharedCommand::Init { shell, .. }) if shell == expected_shell
            ),
            "init {raw_shell} should parse as {expected_shell:?}"
        );
        let alias_cli: Cli = try_parse_cli_from(
            "atlassian",
            ["atlassian", "alias", "install", "--shell", raw_shell, "--dry-run"],
        )
        .unwrap();
        assert!(
            matches!(
                alias_cli.command.unwrap(),
                Command::Shared(SharedCommand::Alias {
                    action: AliasCommand::Install { shell: Some(s), .. }
                }) if s == expected_shell
            ),
            "alias install --shell {raw_shell} should parse as {expected_shell:?}"
        );
    }

    // 6. JSON detection helper covers `--json`, `-j`, and combined short flags
    let to_os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
    assert!(argv_requests_json(&to_os(&["atlassian", "ENG-42", "--json"])));
    assert!(argv_requests_json(&to_os(&["atlassian", "ENG-42", "-j"])));
    assert!(argv_requests_json(&to_os(&["atlassian", "q", "bug", "-rj"])));
    assert!(argv_requests_json(&to_os(&["atlassian", "ls", "-mj"])));
    assert!(!argv_requests_json(&to_os(&["atlassian", "ENG-42"])));
    assert!(!argv_requests_json(&to_os(&["atlassian", "comment", "ENG-42", "--", "-j"])));
}
