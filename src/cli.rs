//! Dual-persona (`jira` / `confluence` / `conf`) CLI parser and command dispatcher.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{self, AppConfig, DeploymentKind, Persona, DEFAULT_SERVE_PORT},
    daemon, img_preview,
    models::{
        AttachmentItem, BoardSummary, IssueDetail, IssueSummary, IssueUpdate, OutgoingIssue,
        OutgoingPage, PageDetail, PageUpdate, SprintSummary, StatusCategory,
    },
    serve::{self, ServeOptions},
    storage::Storage,
    sync, tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "Deleting in --json mode requires passing --yes.";

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentArg {
    Cloud,
    #[value(alias = "dc")]
    Datacenter,
}

impl From<DeploymentArg> for DeploymentKind {
    fn from(value: DeploymentArg) -> Self {
        match value {
            DeploymentArg::Cloud => DeploymentKind::Cloud,
            DeploymentArg::Datacenter => DeploymentKind::DataCenter,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "jira",
    version,
    about = "Native terminal client for Atlassian Jira & Confluence",
    long_about = "Native terminal client for Atlassian Cloud and Data Center (Jira · Agile · Confluence).\nRun without arguments to check login status and launch the TUI."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Parser, Debug)]
#[command(
    name = "confluence",
    version,
    about = "Native terminal client for Atlassian Confluence & Jira",
    long_about = "Native terminal client for Atlassian Cloud and Data Center (Confluence · Jira).\nRun without arguments to open the Confluence Space & Page Tree TUI."
)]
pub struct ConfluenceCli {
    #[command(subcommand)]
    pub command: Option<ConfluenceCommand>,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background sync daemon
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Stop the running background sync daemon
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Show background sync daemon status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Restart the background sync daemon
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Run the background sync daemon in the foreground
    Run,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Log in to Atlassian Cloud (API Token) or Data Center (PAT/Basic)
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        confluence_url: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        pat: Option<String>,
        #[arg(long, value_enum)]
        deployment: Option<DeploymentArg>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored credentials
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Show the currently authenticated Atlassian user profile
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Diagnose config, SQLite cache, keyring paths, deployment mode, and optional API probe
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Jira projects
    Projects {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Jira Agile boards
    Boards {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// List sprints for a Jira Agile board
    Sprints {
        board_id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Render a Kanban board by columns in the terminal
    Board {
        board_id: Option<i64>,
        #[arg(long)]
        sprint: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// List Jira issues
    #[command(visible_alias = "ls")]
    Issues {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        jql: Option<String>,
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// View Jira issue details (description, linked Confluence pages, subtasks, comments)
    #[command(visible_aliases = ["read", "cat"])]
    View {
        key: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new Jira issue
    #[command(visible_alias = "new")]
    Create {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "type", default_value = "Task")]
        issue_type: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long = "label")]
        label: Vec<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Edit fields of an existing Jira issue
    Edit {
        key: String,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long = "label")]
        label: Option<Vec<String>>,
        #[arg(long)]
        json: bool,
    },
    /// Transition a Jira issue workflow status or list available transitions
    #[command(visible_alias = "move")]
    Transition {
        key: String,
        status: Option<String>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
    },
    /// Assign or unassign a Jira issue (supports `--to me`)
    Assign {
        key: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        unassign: bool,
        #[arg(long)]
        json: bool,
    },
    /// Add a Markdown comment to a Jira issue
    Comment {
        key: String,
        body: String,
        #[arg(long)]
        json: bool,
    },
    /// Upload a local file attachment to a Jira issue
    Attach {
        key: String,
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Download attachments from a Jira issue
    #[command(visible_alias = "dl")]
    Download {
        key: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Jira issue
    Delete {
        key: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search Jira issues in local SQLite cache or via remote JQL
    Search {
        query: String,
        #[arg(long)]
        jql: bool,
        #[arg(long)]
        remote: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Open a Jira issue web URL in the default browser
    Open { key: String },
    /// Confluence command namespace (`jira wiki <subcmd>`)
    #[command(visible_alias = "conf")]
    Wiki {
        #[command(subcommand)]
        command: WikiCommand,
    },
    /// Preview image attachments of a Jira issue or Confluence page in the terminal
    Preview {
        target_id_or_key: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Preview a local image file in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Sync Jira and Confluence workspace metadata into the local SQLite cache
    Sync {
        #[arg(long)]
        json: bool,
    },
    /// Manage the background sync daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Check GitHub Releases for updates or self-update the binary
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Launch the interactive terminal UI (TUI)
    Tui,
    /// Run the local HTTP JSON API server for AI agents
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
}

#[derive(Subcommand, Debug)]
pub enum ConfluenceCommand {
    /// Log in to Atlassian Cloud (API Token) or Data Center (PAT/Basic)
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        confluence_url: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        pat: Option<String>,
        #[arg(long, value_enum)]
        deployment: Option<DeploymentArg>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored credentials
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Show the currently authenticated Atlassian user profile
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Diagnose config, SQLite cache, keyring paths, deployment mode, and optional API probe
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Confluence spaces
    Spaces {
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List root pages in a space or child pages under a parent page
    #[command(visible_alias = "tree")]
    Pages {
        space: Option<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read Confluence page body and metadata
    #[command(visible_aliases = ["read", "cat"])]
    View {
        page_id: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new Confluence page
    #[command(visible_alias = "new")]
    Create {
        #[arg(long)]
        space: Option<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Edit a Confluence page (auto-fetches version_number)
    Edit {
        page_id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a Markdown comment to a Confluence page
    Comment {
        page_id: String,
        body: String,
        #[arg(long)]
        json: bool,
    },
    /// Upload a local file attachment to a Confluence page
    Attach {
        page_id: String,
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Download attachments from a Confluence page
    #[command(visible_alias = "dl")]
    Download {
        page_id: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Confluence page
    Delete {
        page_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search Confluence pages in local SQLite cache or via remote CQL
    Search {
        query: String,
        #[arg(long)]
        cql: bool,
        #[arg(long)]
        remote: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Open a Confluence page web URL in the default browser
    Open { page_id: String },
    /// Jira command namespace (`confluence jira <subcmd>`)
    Jira {
        #[command(subcommand)]
        command: JiraSubcommand,
    },
    /// Preview image attachments of a Confluence page or Jira issue in the terminal
    Preview {
        target_id_or_key: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Preview a local image file in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Sync Jira and Confluence workspace metadata into the local SQLite cache
    Sync {
        #[arg(long)]
        json: bool,
    },
    /// Manage the background sync daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Check GitHub Releases for updates or self-update the binary
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Launch the interactive terminal UI (TUI)
    Tui,
    /// Run the local HTTP JSON API server for AI agents
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
}

#[derive(Subcommand, Debug)]
pub enum WikiCommand {
    /// List Confluence spaces
    Spaces {
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List root pages in a space or child pages under a parent page
    #[command(visible_alias = "tree")]
    Pages {
        space: Option<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read a Confluence page
    #[command(visible_aliases = ["read", "cat"])]
    View {
        page_id: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new Confluence page
    #[command(visible_alias = "new")]
    Create {
        #[arg(long)]
        space: Option<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Edit a Confluence page
    Edit {
        page_id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a comment to a Confluence page
    Comment {
        page_id: String,
        body: String,
        #[arg(long)]
        json: bool,
    },
    /// Attach a file to a Confluence page
    Attach {
        page_id: String,
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Download attachments from a Confluence page
    #[command(visible_alias = "dl")]
    Download {
        page_id: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Confluence page
    Delete {
        page_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search Confluence pages
    Search {
        query: String,
        #[arg(long)]
        cql: bool,
        #[arg(long)]
        remote: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Open a Confluence page web URL
    Open { page_id: String },
}

#[derive(Subcommand, Debug)]
pub enum JiraSubcommand {
    /// List Jira projects
    Projects {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Jira Agile boards
    Boards {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// List sprints for a board
    Sprints {
        board_id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Render a Kanban board by columns
    Board {
        board_id: Option<i64>,
        #[arg(long)]
        sprint: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// List Jira issues
    #[command(visible_alias = "ls")]
    Issues {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        jql: Option<String>,
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// View Jira issue details
    #[command(visible_aliases = ["read", "cat"])]
    View {
        key: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new Jira issue
    #[command(visible_alias = "new")]
    Create {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "type", default_value = "Task")]
        issue_type: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long = "label")]
        label: Vec<String>,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Edit a Jira issue
    Edit {
        key: String,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long = "label")]
        label: Option<Vec<String>>,
        #[arg(long)]
        json: bool,
    },
    /// Transition a Jira issue status
    #[command(visible_alias = "move")]
    Transition {
        key: String,
        status: Option<String>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
    },
    /// Assign or unassign a Jira issue
    Assign {
        key: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        unassign: bool,
        #[arg(long)]
        json: bool,
    },
    /// Add a comment to a Jira issue
    Comment {
        key: String,
        body: String,
        #[arg(long)]
        json: bool,
    },
    /// Attach a file to a Jira issue
    Attach {
        key: String,
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Download attachments from a Jira issue
    #[command(visible_alias = "dl")]
    Download {
        key: String,
        #[arg(long = "attachment-id")]
        attachment_id: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Jira issue
    Delete {
        key: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search Jira issues
    Search {
        query: String,
        #[arg(long)]
        jql: bool,
        #[arg(long)]
        remote: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Open a Jira issue web URL
    Open { key: String },
}

fn parse_cli_with_stack<T: Parser + Send + 'static>() -> T {
    std::thread::Builder::new()
        .name("atlassian-cli-parse".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(T::parse)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(T::parse)
}

pub async fn run() -> Result<()> {
    let mut cfg = AppConfig::load()?;
    let persona = Persona::detect();
    match persona {
        Persona::Jira => {
            let cli: Cli = parse_cli_with_stack();
            Box::pin(dispatch_jira_cli(&mut cfg, persona, cli.command)).await
        }
        Persona::Confluence => {
            let cli: ConfluenceCli = parse_cli_with_stack();
            Box::pin(dispatch_confluence_cli(&mut cfg, persona, cli.command)).await
        }
    }
}

async fn dispatch_jira_cli(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Login {
            url,
            confluence_url,
            email,
            token,
            pat,
            deployment,
            json,
        }) => {
            Box::pin(cmd_login(
                cfg,
                LoginParams {
                    url,
                    confluence_url,
                    email,
                    token,
                    pat,
                    deployment: deployment.map(Into::into),
                },
                json,
            ))
            .await
        }
        Some(Command::Logout { json }) => cmd_logout(cfg, json),
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(Command::Doctor { probe, json }) => Box::pin(cmd_doctor(cfg, persona, probe, json)).await,
        Some(Command::Projects { refresh, json }) => Box::pin(cmd_projects(cfg, refresh, json)).await,
        Some(Command::Boards {
            project,
            refresh,
            json,
        }) => Box::pin(cmd_boards(cfg, project.as_deref(), refresh, json)).await,
        Some(Command::Sprints { board_id, json }) => {
            Box::pin(cmd_sprints(cfg, board_id, json)).await
        }
        Some(Command::Board {
            board_id,
            sprint,
            json,
        }) => Box::pin(cmd_board(cfg, board_id, sprint, json)).await,
        Some(Command::Issues {
            project,
            jql,
            mine,
            status,
            limit,
            offline,
            json,
        }) => {
            Box::pin(cmd_issues(
                cfg,
                project.as_deref(),
                jql.as_deref(),
                mine,
                status.as_deref(),
                limit,
                offline,
                json,
            ))
            .await
        }
        Some(Command::View {
            key,
            preview_images,
            offline,
            json,
        }) => Box::pin(cmd_issue_view(cfg, &key, preview_images, offline, json)).await,
        Some(Command::Create {
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
        }) => {
            Box::pin(cmd_issue_create(
                cfg,
                project.as_deref(),
                &issue_type,
                &summary,
                body.as_deref(),
                body_file.as_deref(),
                priority.as_deref(),
                assignee.as_deref(),
                label,
                parent.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Edit {
            key,
            summary,
            body,
            body_file,
            priority,
            assignee,
            label,
            json,
        }) => {
            Box::pin(cmd_issue_edit(
                cfg,
                &key,
                summary,
                body.as_deref(),
                body_file.as_deref(),
                priority,
                assignee,
                label,
                json,
            ))
            .await
        }
        Some(Command::Transition {
            key,
            status,
            comment,
            list,
            json,
        }) => {
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
        Some(Command::Assign {
            key,
            to,
            unassign,
            json,
        }) => Box::pin(cmd_issue_assign(cfg, &key, to.as_deref(), unassign, json)).await,
        Some(Command::Comment { key, body, json }) => {
            Box::pin(cmd_issue_comment(cfg, &key, &body, json)).await
        }
        Some(Command::Attach { key, file, json }) => {
            Box::pin(cmd_issue_attach(cfg, &key, &file, json)).await
        }
        Some(Command::Download {
            key,
            attachment_id,
            dir,
            json,
        }) => {
            Box::pin(cmd_issue_download(
                cfg,
                &key,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Delete { key, yes, json }) => {
            Box::pin(cmd_issue_delete(cfg, &key, yes, json)).await
        }
        Some(Command::Search {
            query,
            jql,
            remote,
            limit,
            json,
        }) => Box::pin(cmd_issue_search(cfg, &query, jql, remote, limit, json)).await,
        Some(Command::Open { key }) => Box::pin(cmd_issue_open(cfg, &key)).await,
        Some(Command::Wiki { command }) => Box::pin(dispatch_wiki_subcommand(cfg, command)).await,
        Some(Command::Preview {
            target_id_or_key,
            attachment_id,
            cols,
            rows,
        }) => {
            Box::pin(cmd_preview(
                cfg,
                &target_id_or_key,
                attachment_id.as_deref(),
                cols,
                rows,
            ))
            .await
        }
        Some(Command::PreviewFile { path, cols, rows }) => cmd_preview_file(&path, cols, rows),
        Some(Command::Sync { json }) => Box::pin(cmd_sync(cfg, json)).await,
        Some(Command::Daemon { action }) => Box::pin(cmd_daemon(cfg, action)).await,
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Serve { bind, port, token }) => {
            let (_auth, client, storage) = open_runtime(cfg, false)?;
            Box::pin(serve::run(
                cfg.clone(),
                client,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(Command::Mcp) => {
            let (_auth, client, storage) = open_runtime(cfg, false)?;
            Box::pin(crate::mcp::run(cfg.clone(), client, storage)).await
        }
    }
}

async fn dispatch_confluence_cli(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<ConfluenceCommand>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(ConfluenceCommand::Login {
            url,
            confluence_url,
            email,
            token,
            pat,
            deployment,
            json,
        }) => {
            Box::pin(cmd_login(
                cfg,
                LoginParams {
                    url,
                    confluence_url,
                    email,
                    token,
                    pat,
                    deployment: deployment.map(Into::into),
                },
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Logout { json }) => cmd_logout(cfg, json),
        Some(ConfluenceCommand::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(ConfluenceCommand::Doctor { probe, json }) => {
            Box::pin(cmd_doctor(cfg, persona, probe, json)).await
        }
        Some(ConfluenceCommand::Spaces {
            refresh,
            limit,
            json,
        }) => Box::pin(cmd_spaces(cfg, refresh, limit, json)).await,
        Some(ConfluenceCommand::Pages {
            space,
            parent,
            limit,
            offline,
            json,
        }) => {
            Box::pin(cmd_pages(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                limit,
                offline,
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::View {
            page_id,
            preview_images,
            offline,
            json,
        }) => Box::pin(cmd_page_view(cfg, &page_id, preview_images, offline, json)).await,
        Some(ConfluenceCommand::Create {
            space,
            parent,
            title,
            body,
            body_file,
            json,
        }) => {
            Box::pin(cmd_page_create(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                &title,
                body.as_deref(),
                body_file.as_deref(),
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Edit {
            page_id,
            title,
            body,
            body_file,
            message,
            json,
        }) => {
            Box::pin(cmd_page_edit(
                cfg,
                &page_id,
                title,
                body.as_deref(),
                body_file.as_deref(),
                message,
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Comment {
            page_id,
            body,
            json,
        }) => Box::pin(cmd_page_comment(cfg, &page_id, &body, json)).await,
        Some(ConfluenceCommand::Attach {
            page_id,
            file,
            json,
        }) => Box::pin(cmd_page_attach(cfg, &page_id, &file, json)).await,
        Some(ConfluenceCommand::Download {
            page_id,
            attachment_id,
            dir,
            json,
        }) => {
            Box::pin(cmd_page_download(
                cfg,
                &page_id,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Delete { page_id, yes, json }) => {
            Box::pin(cmd_page_delete(cfg, &page_id, yes, json)).await
        }
        Some(ConfluenceCommand::Search {
            query,
            cql,
            remote,
            limit,
            json,
        }) => Box::pin(cmd_page_search(cfg, &query, cql, remote, limit, json)).await,
        Some(ConfluenceCommand::Open { page_id }) => Box::pin(cmd_page_open(cfg, &page_id)).await,
        Some(ConfluenceCommand::Jira { command }) => {
            Box::pin(dispatch_jira_subcommand(cfg, command)).await
        }
        Some(ConfluenceCommand::Preview {
            target_id_or_key,
            attachment_id,
            cols,
            rows,
        }) => {
            Box::pin(cmd_preview(
                cfg,
                &target_id_or_key,
                attachment_id.as_deref(),
                cols,
                rows,
            ))
            .await
        }
        Some(ConfluenceCommand::PreviewFile { path, cols, rows }) => {
            cmd_preview_file(&path, cols, rows)
        }
        Some(ConfluenceCommand::Sync { json }) => Box::pin(cmd_sync(cfg, json)).await,
        Some(ConfluenceCommand::Daemon { action }) => Box::pin(cmd_daemon(cfg, action)).await,
        Some(ConfluenceCommand::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(ConfluenceCommand::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(ConfluenceCommand::Serve { bind, port, token }) => {
            let (_auth, client, storage) = open_runtime(cfg, false)?;
            Box::pin(serve::run(
                cfg.clone(),
                client,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(ConfluenceCommand::Mcp) => {
            let (_auth, client, storage) = open_runtime(cfg, false)?;
            Box::pin(crate::mcp::run(cfg.clone(), client, storage)).await
        }
    }
}

async fn dispatch_wiki_subcommand(cfg: &AppConfig, command: WikiCommand) -> Result<()> {
    match command {
        WikiCommand::Spaces {
            refresh,
            limit,
            json,
        } => Box::pin(cmd_spaces(cfg, refresh, limit, json)).await,
        WikiCommand::Pages {
            space,
            parent,
            limit,
            offline,
            json,
        } => {
            Box::pin(cmd_pages(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                limit,
                offline,
                json,
            ))
            .await
        }
        WikiCommand::View {
            page_id,
            preview_images,
            offline,
            json,
        } => Box::pin(cmd_page_view(cfg, &page_id, preview_images, offline, json)).await,
        WikiCommand::Create {
            space,
            parent,
            title,
            body,
            body_file,
            json,
        } => {
            Box::pin(cmd_page_create(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                &title,
                body.as_deref(),
                body_file.as_deref(),
                json,
            ))
            .await
        }
        WikiCommand::Edit {
            page_id,
            title,
            body,
            body_file,
            message,
            json,
        } => {
            Box::pin(cmd_page_edit(
                cfg,
                &page_id,
                title,
                body.as_deref(),
                body_file.as_deref(),
                message,
                json,
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

async fn dispatch_jira_subcommand(cfg: &AppConfig, command: JiraSubcommand) -> Result<()> {
    match command {
        JiraSubcommand::Projects { refresh, json } => Box::pin(cmd_projects(cfg, refresh, json)).await,
        JiraSubcommand::Boards {
            project,
            refresh,
            json,
        } => Box::pin(cmd_boards(cfg, project.as_deref(), refresh, json)).await,
        JiraSubcommand::Sprints { board_id, json } => {
            Box::pin(cmd_sprints(cfg, board_id, json)).await
        }
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
                offline,
                json,
            ))
            .await
        }
        JiraSubcommand::View {
            key,
            preview_images,
            offline,
            json,
        } => Box::pin(cmd_issue_view(cfg, &key, preview_images, offline, json)).await,
        JiraSubcommand::Create {
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
        } => {
            Box::pin(cmd_issue_create(
                cfg,
                project.as_deref(),
                &issue_type,
                &summary,
                body.as_deref(),
                body_file.as_deref(),
                priority.as_deref(),
                assignee.as_deref(),
                label,
                parent.as_deref(),
                json,
            ))
            .await
        }
        JiraSubcommand::Edit {
            key,
            summary,
            body,
            body_file,
            priority,
            assignee,
            label,
            json,
        } => {
            Box::pin(cmd_issue_edit(
                cfg,
                &key,
                summary,
                body.as_deref(),
                body_file.as_deref(),
                priority,
                assignee,
                label,
                json,
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
            to,
            unassign,
            json,
        } => Box::pin(cmd_issue_assign(cfg, &key, to.as_deref(), unassign, json)).await,
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
            json,
        } => Box::pin(cmd_issue_search(cfg, &query, jql, remote, limit, json)).await,
        JiraSubcommand::Open { key } => Box::pin(cmd_issue_open(cfg, &key)).await,
    }
}

// ---------------------------------------------------------------------------
// Runtime & Auth
// ---------------------------------------------------------------------------

struct LoginParams {
    url: Option<String>,
    confluence_url: Option<String>,
    email: Option<String>,
    token: Option<String>,
    pat: Option<String>,
    deployment: Option<DeploymentKind>,
}

fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, AtlassianClient, Storage)> {
    let auth = AuthManager::new(&cfg.profile);
    if require_login && !auth.is_logged_in() {
        bail!(
            "로그인이 필요합니다. 먼저 `{} login --url https://<site>.atlassian.net --email <email>` 을 실행하세요.",
            Persona::detect().as_str()
        );
    }
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let client = AtlassianClient::new(cfg.clone(), auth.clone())?;
    Ok((auth, client, storage))
}

async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let auth = AuthManager::new(&cfg.profile);
    if !auth.is_logged_in() || cfg.site_url.trim().is_empty() {
        let bin = persona.as_str();
        println!("저장된 Atlassian 로그인 정보가 없습니다.");
        println!("빠른 로그인 예시: {bin} login --url https://<site>.atlassian.net --email <email>");
        if !io::stdin().is_terminal() {
            bail!("비대화형 터미널입니다. 먼저 `{bin} login --url ... --email ... --token ...` 으로 로그인하세요.");
        }
        println!();
        cmd_login(
            cfg,
            LoginParams {
                url: None,
                confluence_url: None,
                email: None,
                token: None,
                pat: None,
                deployment: None,
            },
            false,
        )
        .await?;
        println!();
    }

    let (auth, client, storage) = open_runtime(cfg, true)?;
    tui::run_with_persona(cfg.clone(), persona, auth, client, storage).await
}

async fn cmd_login(cfg: &mut AppConfig, params: LoginParams, json_out: bool) -> Result<()> {
    let raw_url = match params.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => u.to_string(),
        None => {
            if !cfg.site_url.trim().is_empty()
                && (params.pat.is_some() || (params.email.is_some() && params.token.is_some()))
            {
                cfg.site_url.clone()
            } else {
                let default_hint = if cfg.site_url.trim().is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", cfg.site_url)
                };
                let entered = prompt_line(&format!(
                    "Atlassian 사이트 URL{default_hint} (예: https://acme.atlassian.net): "
                ))?;
                if entered.is_empty() && !cfg.site_url.trim().is_empty() {
                    cfg.site_url.clone()
                } else {
                    entered
                }
            }
        }
    };
    let site_url = config::normalize_base_url(&raw_url);
    if site_url.is_empty() {
        bail!("사이트 URL이 비어 있습니다. `--url https://<site>.atlassian.net` 를 지정하세요.");
    }

    let deployment = params
        .deployment
        .unwrap_or_else(|| DeploymentKind::infer_from_url(&site_url));

    let confluence_url = if let Some(ref cu) = params.confluence_url {
        config::normalize_base_url(cu)
    } else if deployment == DeploymentKind::DataCenter && params.pat.is_none() && params.token.is_none()
    {
        let entered = prompt_line("Confluence 별도 URL (동일 호스트면 Enter): ")?;
        config::normalize_base_url(&entered)
    } else {
        cfg.confluence_url.clone()
    };

    let creds = if let Some(pat) = params.pat.filter(|s| !s.trim().is_empty()) {
        StoredCredentials::BearerPat {
            site_url: site_url.clone(),
            token: pat.trim().to_string(),
            deployment,
        }
    } else if let Some(token) = params.token.filter(|s| !s.trim().is_empty()) {
        let email = match params.email.filter(|s| !s.trim().is_empty()) {
            Some(e) => e.trim().to_string(),
            None => prompt_line("이메일 (또는 Data Center 사용자명): ")?,
        };
        if email.trim().is_empty() {
            bail!("이메일이 비어 있습니다. `--email <email>` 또는 `--pat <PAT>` 를 사용하세요.");
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
            prompt_line("Data Center 사용자명/이메일 (PAT 인증 시 비워두고 Enter): ")?
        };
        if email.trim().is_empty() {
            let pat = rpassword::prompt_password("Personal Access Token (PAT): ")
                .context("PAT 입력 실패")?;
            if pat.trim().is_empty() {
                bail!("PAT가 비어 있습니다.");
            }
            StoredCredentials::BearerPat {
                site_url: site_url.clone(),
                token: pat.trim().to_string(),
                deployment,
            }
        } else {
            let token = rpassword::prompt_password("비밀번호 또는 API 토큰: ")
                .context("토큰 입력 실패")?;
            if token.trim().is_empty() {
                bail!("비밀번호/토큰이 비어 있습니다.");
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
            None => prompt_line("Atlassian 계정 이메일: ")?,
        };
        if email.trim().is_empty() {
            bail!("이메일이 비어 있습니다.");
        }
        let token = rpassword::prompt_password("Atlassian API 토큰: ")
            .context("API 토큰 입력 실패")?;
        if token.trim().is_empty() {
            bail!("API 토큰이 비어 있습니다.");
        }
        StoredCredentials::ApiToken {
            site_url: site_url.clone(),
            email,
            api_token: token.trim().to_string(),
            deployment,
        }
    };

    cfg.site_url = site_url.clone();
    cfg.confluence_url = confluence_url;
    cfg.deployment = deployment;
    cfg.save()?;

    let auth = AuthManager::new(&cfg.profile);
    auth.save_credentials(&creds)?;

    let client = AtlassianClient::new(cfg.clone(), auth)?;
    let me = client
        .get_myself()
        .await
        .context("자격 증명 저장 후 `/myself` 검증에 실패했습니다")?;
    client.set_cached_me(me.clone());

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "deployment": cfg.deployment.as_str(),
                "site_url": cfg.site_url,
                "confluence_url": cfg.effective_confluence_root(),
                "user": me,
            }))?
        );
    } else {
        println!(
            "로그인 성공: {} ({}) [id={}]",
            me.display_name,
            me.email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "환경: {} · Jira={} · Confluence={}",
            cfg.deployment.as_str(),
            cfg.site_url,
            cfg.effective_confluence_root()
        );
    }
    Ok(())
}

fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(&cfg.profile);
    auth.delete_credentials()?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("로그아웃했습니다. 저장된 자격 증명을 삭제했습니다.");
    }
    Ok(())
}

async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let (auth, client, _storage) = open_runtime(cfg, true)?;
    let me = client.get_myself().await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona.as_str(),
                "deployment": cfg.deployment.as_str(),
                "site_url": cfg.site_url,
                "confluence_url": cfg.effective_confluence_root(),
                "auth_status": auth.status_summary(),
                "user": me,
            }))?
        );
    } else {
        println!("persona: {}", persona.as_str());
        println!(
            "사용자: {} ({})",
            me.display_name,
            me.email.as_deref().unwrap_or("-")
        );
        println!("id: {}", me.id);
        println!("배포: {}", cfg.deployment.as_str());
        println!("Jira URL: {}", cfg.site_url);
        println!("Confluence URL: {}", cfg.effective_confluence_root());
        println!("인증: {}", auth.status_summary());
    }
    Ok(())
}

async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    probe: bool,
    json_out: bool,
) -> Result<()> {
    let auth = AuthManager::new(&cfg.profile);
    let cfg_path = config::config_file_path();
    let cred_path = config::credentials_fallback_path();
    let db_path = config::cache_db_path(&cfg.profile);
    let dl_dir = cfg.resolved_download_dir();
    let auth_status = auth.status_summary();
    let logged_in = auth.is_logged_in();

    let cache_counts = if db_path.exists() {
        Storage::open(&db_path)
            .and_then(|s| s.cache_counts())
            .ok()
    } else {
        None
    };

    let mut probe_me = None;
    let mut probe_error = None;
    if probe {
        let client = AtlassianClient::new(cfg.clone(), auth.clone())?;
        match client.get_myself().await {
            Ok(me) => probe_me = Some(me),
            Err(err) => probe_error = Some(format!("{err:#}")),
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "persona": persona.as_str(),
                "profile": cfg.profile,
                "deployment": cfg.deployment.as_str(),
                "site_url": cfg.site_url,
                "confluence_url": cfg.effective_confluence_root(),
                "logged_in": logged_in,
                "auth_status": auth_status,
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "db_path": db_path,
                "db_exists": db_path.exists(),
                "download_dir": dl_dir,
                "cache_counts": cache_counts,
                "probe_me": probe_me,
                "probe_error": probe_error,
            }))?
        );
    } else {
        println!("atlassian-cli v{} ({})", env!("CARGO_PKG_VERSION"), persona.as_str());
        println!("프로필: {} · 배포: {}", cfg.profile, cfg.deployment.as_str());
        println!(
            "Jira URL: {}",
            if cfg.site_url.is_empty() { "(미설정)" } else { &cfg.site_url }
        );
        let conf_root = cfg.effective_confluence_root();
        println!(
            "Confluence URL: {}",
            if conf_root.is_empty() { "(미설정)" } else { &conf_root }
        );
        println!("인증 상태: {auth_status}");
        println!(
            "설정 파일: {} ({})",
            cfg_path.display(),
            if cfg_path.exists() { "존재" } else { "없음" }
        );
        println!(
            "자격 증명 폴백: {} ({})",
            cred_path.display(),
            if cred_path.exists() { "존재" } else { "없음" }
        );
        println!(
            "SQLite DB: {} ({})",
            db_path.display(),
            if db_path.exists() { "존재" } else { "없음" }
        );
        println!("다운로드 경로: {}", dl_dir.display());
        if let Some(counts) = cache_counts {
            println!(
                "캐시: projects={}, boards={}, issues={}, spaces={}, pages={}",
                counts.projects, counts.boards, counts.issues, counts.spaces, counts.pages
            );
        }
        if probe {
            if let Some(me) = probe_me {
                println!(
                    "라이브 프로브 성공: {} ({}) [id={}]",
                    me.display_name,
                    me.email.as_deref().unwrap_or("-"),
                    me.id
                );
            } else if let Some(err) = probe_error {
                println!("라이브 프로브 실패: {err}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Jira commands
// ---------------------------------------------------------------------------

async fn cmd_projects(cfg: &AppConfig, refresh: bool, json_out: bool) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, refresh)?;
    let cached = storage.list_projects()?;
    let projects = if refresh || cached.is_empty() {
        if !refresh && !AuthManager::new(&cfg.profile).is_logged_in() {
            cached
        } else {
            let fetched = client.list_projects().await?;
            let _ = storage.upsert_projects(&fetched);
            fetched
        }
    } else {
        cached
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "projects": projects }))?);
    } else if projects.is_empty() {
        println!("프로젝트가 없습니다.");
    } else {
        for p in &projects {
            println!(
                "{:<10} {:<28} [{}] lead={}",
                p.key,
                p.name,
                p.project_type,
                p.lead_name.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

async fn cmd_boards(
    cfg: &AppConfig,
    project: Option<&str>,
    refresh: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, refresh)?;
    let cached = storage.list_boards(project)?;
    let boards = if refresh || cached.is_empty() {
        if !refresh && !AuthManager::new(&cfg.profile).is_logged_in() {
            cached
        } else {
            let fetched = client.list_boards(project).await?;
            let _ = storage.upsert_boards(&fetched);
            fetched
        }
    } else {
        cached
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "boards": boards }))?);
    } else if boards.is_empty() {
        println!("보드가 없습니다.");
    } else {
        for b in &boards {
            println!(
                "#{:<6} {:<32} ({}) project={}",
                b.id,
                b.name,
                b.board_type,
                b.project_key.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

async fn cmd_sprints(cfg: &AppConfig, board_id: i64, json_out: bool) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let sprints = client.list_board_sprints(board_id).await?;
    let _ = storage.upsert_sprints(&sprints);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "board_id": board_id, "sprints": sprints }))?
        );
    } else if sprints.is_empty() {
        println!("보드 #{board_id}에 스프린트가 없습니다.");
    } else {
        for s in &sprints {
            println!(
                "#{:<6} [{}] {:<28} {} ~ {}",
                s.id,
                s.state,
                s.name,
                s.start_date.as_deref().unwrap_or("-"),
                s.end_date.as_deref().unwrap_or("-")
            );
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

    let mut boards = storage.list_boards(cfg.default_project.as_deref())?;
    if boards.is_empty() {
        boards = client.list_boards(cfg.default_project.as_deref()).await?;
        let _ = storage.upsert_boards(&boards);
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
            .context("사용 가능한 Jira 보드를 찾지 못했습니다. `board <ID>`를 지정하세요.")?,
    };

    if board.columns.is_empty() {
        if let Ok(cols) = client.get_board_columns(board.id).await {
            board.columns = cols;
        }
    }

    let sprints = client.list_board_sprints(board.id).await.unwrap_or_default();
    let _ = storage.upsert_sprints(&sprints);

    let active_sprint = if let Some(sid) = sprint_id {
        sprints.iter().find(|s| s.id == sid).cloned()
    } else {
        sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .cloned()
    };

    let chosen_sprint_id = sprint_id.or_else(|| active_sprint.as_ref().map(|s| s.id));
    let issues = client
        .list_board_issues(board.id, chosen_sprint_id, 100)
        .await?;
    let _ = storage.upsert_issues(&issues);

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

#[allow(clippy::too_many_arguments)]
async fn cmd_issues(
    cfg: &AppConfig,
    project: Option<&str>,
    jql: Option<&str>,
    mine: bool,
    status: Option<&str>,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let limit = serve::clamp_limit(limit);

    let mut issues = if offline {
        let mut list = storage.list_issues(project, limit)?;
        if let Some(st) = status {
            let st_low = st.trim().to_ascii_lowercase();
            list.retain(|i| i.status.name.to_ascii_lowercase().contains(&st_low));
        }
        list
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
            .search_issues_jql(&effective_jql, limit, None, None)
            .await?;
        let _ = storage.upsert_issues(&page.issues);
        page.issues
    };

    issues.truncate(limit);

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "issues": issues }))?);
    } else if issues.is_empty() {
        println!("이슈가 없습니다.");
    } else {
        for issue in &issues {
            let assignee = issue
                .assignee
                .as_ref()
                .map(|u| u.display_name.as_str())
                .unwrap_or("미배정");
            let prio = issue
                .priority
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("-");
            println!(
                "{:<12} [{:<12}] ({:<6}) {:<12} {}",
                issue.key, issue.status.name, prio, assignee, issue.summary
            );
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
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let detail = if offline {
        storage
            .get_issue_detail(key)?
            .with_context(|| format!("오프라인 캐시에서 이슈 '{key}'를 찾을 수 없습니다."))?
    } else {
        let fetched = client.get_issue(key).await?;
        let _ = storage.upsert_issue_detail(&fetched);
        fetched
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "issue": detail }))?);
        return Ok(());
    }

    print_issue_detail(&detail);

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
) -> Result<()> {
    let project_key = project
        .or(cfg.default_project.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("프로젝트 키가 필요합니다 (`--project <KEY>` 또는 config `default_project`).")?;
    let description_markdown = read_body_arg(body, body_file)?.unwrap_or_default();

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let assignee_id = match assignee.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("me") => Some(client.get_myself().await?.id),
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    };

    let req = OutgoingIssue {
        project_key: project_key.to_string(),
        issue_type: issue_type.to_string(),
        summary: summary.to_string(),
        description_markdown,
        priority: priority.map(str::to_string),
        assignee_id,
        labels,
        parent_key: parent.map(str::to_string),
    };
    let created = client.create_issue(&req).await?;
    let _ = storage.upsert_issue_detail(&created);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "issue": created }))?
        );
    } else {
        println!(
            "이슈 생성 완료: {} — {} ({})",
            created.summary.key, created.summary.summary, created.summary.web_url
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_issue_edit(
    cfg: &AppConfig,
    key: &str,
    summary: Option<String>,
    body: Option<&str>,
    body_file: Option<&Path>,
    priority: Option<String>,
    assignee: Option<String>,
    labels: Option<Vec<String>>,
    json_out: bool,
) -> Result<()> {
    let description_markdown = read_body_arg(body, body_file)?;
    if summary.is_none()
        && description_markdown.is_none()
        && priority.is_none()
        && assignee.is_none()
        && labels.is_none()
    {
        bail!("수정할 필드를 하나 이상 지정하세요 (`--summary`, `--body`, `--priority`, `--assignee`, `--label`).");
    }

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let assignee_id = match assignee.as_deref().map(str::trim) {
        None => None,
        Some("") | Some("none") | Some("unassign") => Some(None),
        Some(v) if v.eq_ignore_ascii_case("me") => Some(Some(client.get_myself().await?.id)),
        Some(v) => Some(Some(v.to_string())),
    };

    let patch = IssueUpdate {
        summary,
        description_markdown,
        priority,
        assignee_id,
        labels,
    };
    let updated = client.update_issue(key, &patch).await?;
    let _ = storage.upsert_issue_detail(&updated);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "issue": updated }))?
        );
    } else {
        println!(
            "이슈 수정 완료: {} — {}",
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
    let (_auth, client, _storage) = open_runtime(cfg, true)?;

    if list || status.is_none() {
        let transitions = client.list_transitions(key).await?;
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "key": key, "transitions": transitions }))?
            );
        } else if transitions.is_empty() {
            println!("{key}: 사용 가능한 상태 전환이 없습니다.");
        } else {
            println!("{key} 상태 전환 목록:");
            for t in transitions {
                println!("  [{}] {} -> {}", t.id, t.name, t.to_status.name);
            }
        }
        return Ok(());
    }

    let target = status.unwrap();
    client.transition_issue(key, target, comment).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "key": key,
                "transition": target,
            }))?
        );
    } else {
        println!("{key}: '{target}'(으)로 상태 전환했습니다.");
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
        bail!("`--to <USER_OR_ME>` 또는 `--unassign` 을 지정하세요.");
    }
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let target_id = if unassign {
        None
    } else {
        let raw = to.unwrap().trim();
        if raw.eq_ignore_ascii_case("me") {
            Some(client.get_myself().await?.id)
        } else {
            Some(raw.to_string())
        }
    };

    client.assign_issue(key, target_id.as_deref()).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "key": key,
                "assignee_id": target_id,
            }))?
        );
    } else if let Some(uid) = target_id {
        println!("{key}: 담당자를 '{uid}'(으)로 지정했습니다.");
    } else {
        println!("{key}: 담당자를 해제했습니다.");
    }
    Ok(())
}

async fn cmd_issue_comment(cfg: &AppConfig, key: &str, body: &str, json_out: bool) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let comment = client.add_issue_comment(key, body).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "comment": comment }))?
        );
    } else {
        println!("{key}: 댓글을 등록했습니다 (id={}).", comment.id);
    }
    Ok(())
}

async fn cmd_issue_attach(cfg: &AppConfig, key: &str, file: &Path, json_out: bool) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let uploaded = client.upload_issue_attachment(key, file).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "key": key, "attachments": uploaded }))?
        );
    } else {
        println!("{key}: 파일 '{}' 첨부 완료 ({}개).", file.display(), uploaded.len());
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
    let detail = client.get_issue(key).await?;
    let attachment = select_attachment(&detail.attachments, attachment_id)
        .with_context(|| format!("이슈 {key}에서 다운로드할 첨부파일을 찾지 못했습니다."))?;
    let dest_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = client.download_attachment(attachment, &dest_dir).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "file": downloaded }))?
        );
    } else {
        println!(
            "다운로드 완료: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.size
        );
    }
    Ok(())
}

async fn cmd_issue_delete(cfg: &AppConfig, key: &str, yes: bool, json_out: bool) -> Result<()> {
    let answer = if !yes && !json_out {
        Some(prompt_line(&format!(
            "정말 Jira 이슈 '{key}'를 삭제하시겠습니까? [y/N]: "
        ))?)
    } else {
        None
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    client.delete_issue(key).await?;
    let _ = storage.delete_issue(key);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "key": key }))?
        );
    } else {
        println!("Jira 이슈 '{key}'를 삭제했습니다.");
    }
    Ok(())
}

async fn cmd_issue_search(
    cfg: &AppConfig,
    query: &str,
    jql: bool,
    remote: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let limit = serve::clamp_limit(limit);
    let use_remote = remote || jql;
    let (_auth, client, storage) = open_runtime(cfg, use_remote)?;

    if use_remote {
        let effective_jql = if jql || serve::looks_like_query_expr(query) {
            query.to_string()
        } else {
            format!(
                "text ~ \"{}\" ORDER BY updated DESC",
                serve::escape_jql_str(query)
            )
        };
        let page = client
            .search_issues_jql(&effective_jql, limit, None, None)
            .await?;
        let _ = storage.upsert_issues(&page.issues);

        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": true,
                    "jql": effective_jql,
                    "issues": page.issues,
                }))?
            );
        } else if page.issues.is_empty() {
            println!("검색 결과가 없습니다.");
        } else {
            for i in &page.issues {
                println!("{:<12} [{:<12}] {}", i.key, i.status.name, i.summary);
            }
        }
    } else {
        let hits = storage.search_issues(query, limit)?;
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
            println!("로컬 캐시 검색 결과가 없습니다. (`--remote`로 Jira 서버 검색 가능)");
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
    let (_auth, client, storage) = open_runtime(cfg, false)?;
    let url = if let Ok(Some(detail)) = storage.get_issue_detail(key) {
        detail.summary.web_url
    } else if AuthManager::new(&cfg.profile).is_logged_in() {
        client.get_issue(key).await?.summary.web_url
    } else if !cfg.site_url.trim().is_empty() {
        format!("{}/browse/{key}", cfg.jira_base_url())
    } else {
        bail!("웹 URL을 구성할 수 없습니다. 먼저 `jira login` 을 실행하세요.");
    };
    open::that(&url).with_context(|| format!("브라우저 열기 실패: {url}"))?;
    println!("브라우저에서 열었습니다: {url}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Confluence commands
// ---------------------------------------------------------------------------

async fn cmd_spaces(cfg: &AppConfig, refresh: bool, limit: usize, json_out: bool) -> Result<()> {
    let limit = serve::clamp_limit(limit);
    let (_auth, client, storage) = open_runtime(cfg, refresh)?;
    let cached = storage.list_spaces()?;
    let mut spaces = if refresh || cached.is_empty() {
        if !refresh && !AuthManager::new(&cfg.profile).is_logged_in() {
            cached
        } else {
            let fetched = client.list_spaces(limit).await?;
            let _ = storage.upsert_spaces(&fetched);
            fetched
        }
    } else {
        cached
    };
    spaces.truncate(limit);

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "spaces": spaces }))?);
    } else if spaces.is_empty() {
        println!("스페이스가 없습니다.");
    } else {
        for s in &spaces {
            println!("{:<12} {:<32} [{}]", s.key, s.name, s.space_type);
        }
    }
    Ok(())
}

async fn cmd_pages(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let limit = serve::clamp_limit(limit);
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;

    let mut pages = if let Some(parent_id) = parent {
        if offline {
            storage.list_child_pages(parent_id)?
        } else {
            let fetched = client.list_child_pages(parent_id).await?;
            let _ = storage.upsert_pages(&fetched);
            fetched
        }
    } else if let Some(sp) = space.or(cfg.default_space_key.as_deref()) {
        if offline {
            storage.list_space_root_pages(sp)?
        } else {
            let fetched = client.list_space_root_pages(sp, limit).await?;
            let _ = storage.upsert_pages(&fetched);
            fetched
        }
    } else if offline {
        storage.list_recent_pages(limit)?
    } else {
        bail!("스페이스 키 또는 `--parent <PAGE_ID>` 를 지정하세요.");
    };
    pages.truncate(limit);

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "pages": pages }))?);
    } else if pages.is_empty() {
        println!("페이지가 없습니다.");
    } else {
        for p in &pages {
            let children_mark = if p.has_children { "▸" } else { "·" };
            println!(
                "{children_mark} {:<12} [{}] v{} {}",
                p.id, p.space_key, p.version_number, p.title
            );
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
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let detail = if offline {
        storage
            .get_page_detail(page_id)?
            .with_context(|| format!("오프라인 캐시에서 페이지 '{page_id}'를 찾을 수 없습니다."))?
    } else {
        let fetched = client.get_page(page_id).await?;
        let _ = storage.upsert_page_detail(&fetched);
        fetched
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "page": detail }))?);
        return Ok(());
    }

    print_page_detail(&detail);

    if preview_images && !offline {
        render_attachments_preview(&client, &detail.attachments, None, None).await;
    }
    Ok(())
}

async fn cmd_page_create(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    title: &str,
    body: Option<&str>,
    body_file: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let space_id_or_key = space
        .or(cfg.default_space_key.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("스페이스 키 또는 ID가 필요합니다 (`--space <KEY_OR_ID>`).")?;
    let body_markdown = read_body_arg(body, body_file)?.unwrap_or_default();

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let req = OutgoingPage {
        space_id_or_key: space_id_or_key.to_string(),
        parent_id: parent.map(str::to_string),
        title: title.to_string(),
        body_markdown,
    };
    let created = client.create_page(&req).await?;
    let _ = storage.upsert_page_detail(&created);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page": created }))?
        );
    } else {
        println!(
            "페이지 생성 완료: [{}] {} ({})",
            created.summary.id, created.summary.title, created.summary.web_url
        );
    }
    Ok(())
}

async fn cmd_page_edit(
    cfg: &AppConfig,
    page_id: &str,
    title: Option<String>,
    body: Option<&str>,
    body_file: Option<&Path>,
    message: Option<String>,
    json_out: bool,
) -> Result<()> {
    let body_opt = read_body_arg(body, body_file)?;
    if title.is_none() && body_opt.is_none() {
        bail!("수정할 제목(`--title`) 또는 본문(`--body` / `--body-file`)을 지정하세요.");
    }

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let current = client.get_page(page_id).await?;

    let patch = PageUpdate {
        title: title.unwrap_or_else(|| current.summary.title.clone()),
        body_markdown: body_opt.unwrap_or_else(|| current.body_markdown.clone()),
        prev_version: current.summary.version_number,
        version_message: message,
    };
    let updated = client.update_page(page_id, &patch).await?;
    let _ = storage.upsert_page_detail(&updated);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page": updated }))?
        );
    } else {
        println!(
            "페이지 수정 완료: [{}] {} (v{})",
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
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let comment = client.add_page_comment(page_id, body).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "comment": comment }))?
        );
    } else {
        println!("페이지 {page_id}: 댓글을 등록했습니다 (id={}).", comment.id);
    }
    Ok(())
}

async fn cmd_page_attach(
    cfg: &AppConfig,
    page_id: &str,
    file: &Path,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let uploaded = client.upload_page_attachment(page_id, file).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page_id": page_id, "attachments": uploaded }))?
        );
    } else {
        println!(
            "페이지 {page_id}: 파일 '{}' 첨부 완료 ({}개).",
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
    let detail = client.get_page(page_id).await?;
    let attachment = select_attachment(&detail.attachments, attachment_id)
        .with_context(|| format!("페이지 {page_id}에서 다운로드할 첨부파일을 찾지 못했습니다."))?;
    let dest_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = client.download_attachment(attachment, &dest_dir).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "file": downloaded }))?
        );
    } else {
        println!(
            "다운로드 완료: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.size
        );
    }
    Ok(())
}

async fn cmd_page_delete(cfg: &AppConfig, page_id: &str, yes: bool, json_out: bool) -> Result<()> {
    let answer = if !yes && !json_out {
        Some(prompt_line(&format!(
            "정말 Confluence 페이지 '{page_id}'를 삭제하시겠습니까? [y/N]: "
        ))?)
    } else {
        None
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    client.delete_page(page_id).await?;
    let _ = storage.delete_page(page_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page_id": page_id }))?
        );
    } else {
        println!("Confluence 페이지 '{page_id}'를 삭제했습니다.");
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

    if use_remote {
        let effective_cql = if cql || serve::looks_like_query_expr(query) {
            query.to_string()
        } else {
            format!("text ~ \"{}\"", serve::escape_jql_str(query))
        };
        let pages = client.search_confluence_cql(&effective_cql, limit).await?;
        let _ = storage.upsert_pages(&pages);

        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": true,
                    "cql": effective_cql,
                    "pages": pages,
                }))?
            );
        } else if pages.is_empty() {
            println!("검색 결과가 없습니다.");
        } else {
            for p in &pages {
                println!("{:<12} [{}] {}", p.id, p.space_key, p.title);
            }
        }
    } else {
        let hits = storage.search_pages(query, limit)?;
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
            println!("로컬 캐시 검색 결과가 없습니다. (`--remote`로 Confluence 서버 검색 가능)");
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
    let (_auth, client, storage) = open_runtime(cfg, false)?;
    let url = if let Ok(Some(detail)) = storage.get_page_detail(page_id) {
        detail.summary.web_url
    } else if AuthManager::new(&cfg.profile).is_logged_in() {
        client.get_page(page_id).await?.summary.web_url
    } else {
        let root = cfg.effective_confluence_root();
        if root.is_empty() {
            bail!("웹 URL을 구성할 수 없습니다. 먼저 `confluence login` 을 실행하세요.");
        }
        format!("{root}/pages/{page_id}")
    };
    open::that(&url).with_context(|| format!("브라우저 열기 실패: {url}"))?;
    println!("브라우저에서 열었습니다: {url}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared utilities
// ---------------------------------------------------------------------------

async fn cmd_preview(
    cfg: &AppConfig,
    target_id_or_key: &str,
    attachment_id: Option<&str>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let (_auth, client, _storage) = open_runtime(cfg, true)?;
    let attachments = if looks_like_jira_key(target_id_or_key) {
        client.get_issue(target_id_or_key).await?.attachments
    } else {
        client.get_page(target_id_or_key).await?.attachments
    };

    let selected = if let Some(aid) = attachment_id {
        select_attachment(&attachments, Some(aid))
    } else {
        attachments
            .iter()
            .find(|a| img_preview::is_image_attachment(&a.mime_type, &a.filename))
    }
    .with_context(|| format!("'{target_id_or_key}'에서 이미지 첨부파일을 찾지 못했습니다."))?;

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

fn cmd_preview_file(path: &Path, cols: Option<u16>, rows: Option<u16>) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("이미지 파일 읽기 실패: {}", path.display()))?;
    let rendered = img_preview::render_for_stdout(
        &bytes,
        cols.unwrap_or(80),
        rows.unwrap_or(24),
    )?;
    println!("{rendered}");
    Ok(())
}

async fn cmd_sync(cfg: &AppConfig, json_out: bool) -> Result<()> {
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

async fn cmd_daemon(cfg: &AppConfig, action: DaemonCommand) -> Result<()> {
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

async fn cmd_update(
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
// ---------------------------------------------------------------------------

pub(crate) fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation received; item was not deleted.")?;
    let trimmed = answer.trim();
    if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
        bail!("Cancelled; item was not deleted.");
    }
    Ok(())
}

fn read_body_arg(body: Option<&str>, body_file: Option<&Path>) -> Result<Option<String>> {
    match (body, body_file) {
        (Some(_), Some(_)) => bail!("`--body` and `--body-file` cannot be used together."),
        (Some(b), None) => Ok(Some(b.to_string())),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read body file: {}", path.display()))?;
            Ok(Some(content))
        }
        (None, None) => Ok(None),
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line)?;
    if n == 0 {
        bail!("표준 입력이 닫혀 있습니다.");
    }
    Ok(line.trim().to_string())
}

fn select_attachment<'a>(
    attachments: &'a [AttachmentItem],
    attachment_id: Option<&str>,
) -> Option<&'a AttachmentItem> {
    if let Some(id) = attachment_id {
        attachments
            .iter()
            .find(|a| a.id == id || a.filename == id)
    } else {
        attachments.first()
    }
}

pub(crate) fn looks_like_jira_key(raw: &str) -> bool {
    let trimmed = raw.trim();
    let Some((prefix, num)) = trimmed.split_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

fn print_issue_detail(detail: &IssueDetail) {
    let s = &detail.summary;
    println!("{} — {}", s.key, s.summary);
    println!(
        "상태: [{}] ({}) · 유형: {} · 우선순위: {}",
        s.status.name,
        s.status.category.badge_label(),
        s.issue_type.name,
        s.priority.as_ref().map(|p| p.name.as_str()).unwrap_or("-")
    );
    println!(
        "담당자: {} · 보고자: {} · 스프린트: {}",
        s.assignee
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("미배정"),
        s.reporter
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("-"),
        s.sprint_name.as_deref().unwrap_or("-")
    );
    if !s.labels.is_empty() {
        println!("라벨: {}", s.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }

    if !detail.description_markdown.trim().is_empty() {
        println!("\n--- 설명 ---\n{}", detail.description_markdown.trim());
    }
    if !detail.subtasks.is_empty() {
        println!("\n--- 서브태스크 ({}) ---", detail.subtasks.len());
        for sub in &detail.subtasks {
            println!("  {} [{}] {}", sub.key, sub.status.name, sub.summary);
        }
    }
    if !detail.linked_pages.is_empty() {
        println!("\n--- 연결된 Confluence 페이지 ({}) ---", detail.linked_pages.len());
        for page in &detail.linked_pages {
            println!("  [{}] {} ({})", page.page_id, page.title, page.web_url);
        }
    }
    if !detail.attachments.is_empty() {
        println!("\n--- 첨부파일 ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- 댓글 ({}) ---", detail.comments.len());
        for c in &detail.comments {
            println!("  [{}] {}:", c.created_at, c.author.display_name);
            for line in c.body_markdown.lines() {
                println!("    {line}");
            }
        }
    }
}

fn print_page_detail(detail: &PageDetail) {
    let s = &detail.summary;
    println!("[{}] {} (v{})", s.id, s.title, s.version_number);
    println!(
        "스페이스: {} · 작성자: {} · 수정일: {}",
        s.space_key,
        s.author_name.as_deref().unwrap_or("-"),
        s.updated_at
    );
    if !detail.ancestors.is_empty() {
        let trail: Vec<&str> = detail.ancestors.iter().map(|a| a.title.as_str()).collect();
        println!("경로: {}", trail.join(" > "));
    }
    if !detail.labels.is_empty() {
        println!("라벨: {}", detail.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }
    if !detail.body_markdown.trim().is_empty() {
        println!("\n--- 본문 ---\n{}", detail.body_markdown.trim());
    }
    if !detail.linked_issue_keys.is_empty() {
        println!("\n--- 연결된 Jira 이슈 ---\n  {}", detail.linked_issue_keys.join(", "));
    }
    if !detail.attachments.is_empty() {
        println!("\n--- 첨부파일 ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- 댓글 ({}) ---", detail.comments.len());
        for c in &detail.comments {
            println!("  [{}] {}:", c.created_at, c.author_name);
            for line in c.body_markdown.lines() {
                println!("    {line}");
            }
        }
    }
}

async fn render_attachments_preview(
    client: &AtlassianClient,
    attachments: &[AttachmentItem],
    cols: Option<u16>,
    rows: Option<u16>,
) {
    for att in attachments {
        if !img_preview::is_image_attachment(&att.mime_type, &att.filename) {
            continue;
        }
        match client.fetch_attachment_bytes(att).await {
            Ok(bytes) => {
                match img_preview::render_for_stdout(
                    &bytes,
                    cols.unwrap_or(64),
                    rows.unwrap_or(18),
                ) {
                    Ok(rendered) => {
                        println!("\n[이미지 미리보기: {}]", att.filename);
                        println!("{rendered}");
                    }
                    Err(err) => {
                        eprintln!("이미지 렌더링 건너뜀 ({}): {err:#}", att.filename);
                    }
                }
            }
            Err(err) => {
                eprintln!("이미지 다운로드 건너뜀 ({}): {err:#}", att.filename);
            }
        }
    }
}

pub(crate) fn format_kanban_board(
    board: &BoardSummary,
    sprint: Option<&SprintSummary>,
    issues: &[IssueSummary],
) -> String {
    let mut columns: Vec<(String, Vec<&IssueSummary>)> = if !board.columns.is_empty() {
        board
            .columns
            .iter()
            .map(|c| (c.name.clone(), Vec::new()))
            .collect()
    } else {
        vec![
            ("TO DO".to_string(), Vec::new()),
            ("IN PROGRESS".to_string(), Vec::new()),
            ("DONE".to_string(), Vec::new()),
        ]
    };

    for issue in issues {
        let idx = if !board.columns.is_empty() {
            board
                .columns
                .iter()
                .position(|c| {
                    c.status_ids.iter().any(|id| id == &issue.status.id)
                        || c.status_names
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(&issue.status.name))
                })
                .or_else(|| {
                    board
                        .columns
                        .iter()
                        .position(|c| c.category_hint == issue.status.category)
                })
                .unwrap_or(0)
        } else {
            match issue.status.category {
                StatusCategory::Todo => 0,
                StatusCategory::InProgress => 1,
                StatusCategory::Done => 2,
            }
        };
        if let Some((_, list)) = columns.get_mut(idx) {
            list.push(issue);
        }
    }

    let col_width: usize = 28;
    let mut out = String::new();
    let sprint_label = sprint
        .map(|s| format!(" · Sprint: {} [{}]", s.name, s.state))
        .unwrap_or_default();
    out.push_str(&format!(
        "Board #{}: {} ({}){sprint_label}\n",
        board.id, board.name, board.board_type
    ));

    let col_count = columns.len().max(1);
    let top = format!(
        "┌{}┐\n",
        vec!["─".repeat(col_width + 2); col_count].join("┬")
    );
    let mid = format!(
        "├{}┤\n",
        vec!["─".repeat(col_width + 2); col_count].join("┼")
    );
    let bot = format!(
        "└{}┘\n",
        vec!["─".repeat(col_width + 2); col_count].join("┴")
    );

    out.push_str(&top);
    let header_cells: Vec<String> = columns
        .iter()
        .map(|(name, list)| {
            let label = format!("{name} ({})", list.len());
            format!(" {} ", pad_width(&label, col_width))
        })
        .collect();
    out.push_str(&format!("│{}│\n", header_cells.join("│")));
    out.push_str(&mid);

    let max_cards = columns.iter().map(|(_, l)| l.len()).max().unwrap_or(0);
    if max_cards == 0 {
        let empty_cells: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let txt = if i == 0 { "(이슈 없음)" } else { "" };
                format!(" {} ", pad_width(txt, col_width))
            })
            .collect();
        out.push_str(&format!("│{}│\n", empty_cells.join("│")));
    } else {
        for row in 0..max_cards {
            let line1: Vec<String> = columns
                .iter()
                .map(|(_, list)| {
                    let text = list
                        .get(row)
                        .map(|i| {
                            let assignee = i
                                .assignee
                                .as_ref()
                                .map(|u| u.display_name.as_str())
                                .unwrap_or("-");
                            format!("{} ({assignee})", i.key)
                        })
                        .unwrap_or_default();
                    format!(" {} ", pad_width(&text, col_width))
                })
                .collect();
            let line2: Vec<String> = columns
                .iter()
                .map(|(_, list)| {
                    let text = list
                        .get(row)
                        .map(|i| i.summary.clone())
                        .unwrap_or_default();
                    format!(" {} ", pad_width(&text, col_width))
                })
                .collect();
            out.push_str(&format!("│{}│\n", line1.join("│")));
            out.push_str(&format!("│{}│\n", line2.join("│")));
        }
    }
    out.push_str(&bot);
    out
}

fn pad_width(raw: &str, max_width: usize) -> String {
    let mut acc = String::new();
    let mut width = 0;
    for ch in raw.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max_width {
            if max_width >= 1 {
                while width + 1 > max_width && !acc.is_empty() {
                    if let Some(popped) = acc.pop() {
                        width = width.saturating_sub(popped.width().unwrap_or(0));
                    }
                }
                acc.push('…');
                width += 1;
            }
            break;
        }
        acc.push(ch);
        width += cw;
    }
    if width < max_width {
        acc.push_str(&" ".repeat(max_width - width));
    }
    let _ = acc.width();
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BoardColumn, IssueStatus, UserSummary};

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
            Command::Login {
                deployment: Some(DeploymentArg::Cloud),
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from([
            "jira", "issues", "--project", "ENG", "--mine", "--offline", "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Issues {
                mine: true,
                offline: true,
                json: true,
                ..
            }
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
            Command::Create {
                issue_type, label, ..
            } => {
                assert_eq!(issue_type, "Bug");
                assert_eq!(label, vec!["cli", "urgent"]);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from(["jira", "assign", "ENG-1", "--to", "me", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Assign {
                to: Some(_),
                unassign: false,
                json: true,
                ..
            }
        ));

        // Cross-mounted Confluence under `jira wiki` and alias `jira conf`
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
            Command::Serve { bind, port, token } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 9000);
                assert_eq!(token.as_deref(), Some("sec"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let cli = Cli::try_parse_from(["jira", "mcp-server"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Command::Mcp));

        // Daemon subcommands for both Jira & Confluence personas
        let cli = Cli::try_parse_from(["jira", "daemon", "start", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Daemon {
                action: DaemonCommand::Start { json: true }
            }
        ));
        let cli = Cli::try_parse_from(["jira", "daemon", "stop"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Daemon {
                action: DaemonCommand::Stop { json: false }
            }
        ));
        let cli = Cli::try_parse_from(["jira", "daemon", "status", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Daemon {
                action: DaemonCommand::Status { json: true }
            }
        ));
        let cli = Cli::try_parse_from(["jira", "daemon", "restart"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Daemon {
                action: DaemonCommand::Restart { json: false }
            }
        ));
        let cli = Cli::try_parse_from(["jira", "daemon", "run"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Daemon {
                action: DaemonCommand::Run
            }
        ));

        let conf = ConfluenceCli::try_parse_from(["confluence", "daemon", "status", "--json"]).unwrap();
        assert!(matches!(
            conf.command.unwrap(),
            ConfluenceCommand::Daemon {
                action: DaemonCommand::Status { json: true }
            }
        ));
        let conf = ConfluenceCli::try_parse_from(["confluence", "daemon", "run"]).unwrap();
        assert!(matches!(
            conf.command.unwrap(),
            ConfluenceCommand::Daemon {
                action: DaemonCommand::Run
            }
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
            ConfluenceCommand::Pages {
                limit: 25,
                offline: true,
                json: true,
                ..
            }
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
            ConfluenceCommand::Edit { .. }
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
}
