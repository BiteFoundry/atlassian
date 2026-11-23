//! Dual-persona (`atlassian` / `confluence` / `conf`) CLI parser and command dispatcher.

use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    atlassian::{
        body::{BodyEdit, SectionReplacement, TextReplacement},
        client::{error_payload, is_not_found_error, HttpStatusError, IdentityProbe},
        AtlassianClient, SiteClientPool,
    },
    auth::{
        fetch_accessible_resources, run_oauth_loopback_login, AuthManager, EnvCredentialPolicy,
        OAuthLoopbackOptions, StoredCredentials, DEFAULT_OAUTH_API_BASE, DEFAULT_OAUTH_AUTH_BASE,
    },
    config::{
        self, AppConfig, DeploymentKind, Persona, SiteProfile, DEFAULT_SERVE_PORT,
    },
    daemon, img_preview,
    models::{
        parse_offset_cursor, slice_page, AttachmentItem, BoardSummary, IssueDetail, IssueSummary,
        IssueUpdate, OutgoingIssue, OutgoingPage, PageDetail, SprintSummary, StatusCategory,
    },
    resolver::resolve_rejected_assignee,
    serve::{self, ServeOptions},
    shell_alias::{
        self, AliasCommand, ShellKind, DEFAULT_CONF_ALIAS, DEFAULT_JIRA_ALIAS,
    },
    storage::{IssueListFilter, SiteRecord, Storage},
    sync::{self, SiteSyncTarget},
    tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "Deleting in --json mode requires passing --yes.";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct JsonReported(String);

pub(crate) fn argv_requests_json(args: &[OsString]) -> bool {
    for arg in args.iter().skip(1) {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--json" || text == "-j" {
            return true;
        }
        if let Some(short_cluster) = text.strip_prefix('-') {
            if !short_cluster.starts_with('-')
                && short_cluster.len() >= 2
                && short_cluster.bytes().all(|b| b.is_ascii_alphabetic())
                && short_cluster.contains('j')
            {
                return true;
            }
        }
    }
    false
}

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
    name = "atlassian",
    version,
    about = "Native terminal client for Atlassian Jira & Confluence",
    long_about = "Native terminal client for Atlassian Cloud and Data Center (Jira · Agile · Confluence).\nRun without arguments to check login status and launch the TUI."
)]
pub struct Cli {
    /// Target Atlassian site ID, name, or URL (overrides default_site)
    #[arg(long, global = true, env = "ATLASSIAN_SITE", value_name = "SITE")]
    pub site: Option<String>,
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
    /// Target Atlassian site ID, name, or URL (overrides default_site)
    #[arg(long, global = true, env = "ATLASSIAN_SITE", value_name = "SITE")]
    pub site: Option<String>,
    #[command(subcommand)]
    pub command: Option<ConfluenceCommand>,
}

#[derive(clap::Args, Debug)]
pub struct EditBodyArgs {
    /// Replace the entire body with Markdown
    #[arg(short = 'b', long, allow_hyphen_values = true, conflicts_with = "body_file")]
    body: Option<String>,
    /// Replace the entire body from a Markdown file
    #[arg(long = "body-file")]
    body_file: Option<PathBuf>,
    /// Append Markdown while preserving the existing body
    #[arg(
        long,
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append_file"]
    )]
    append: Option<String>,
    /// Append a Markdown file while preserving the existing body
    #[arg(long = "append-file", conflicts_with_all = ["body", "body_file"])]
    append_file: Option<PathBuf>,
    /// Replace one unique text occurrence in place
    #[arg(
        long,
        num_args = 2,
        value_names = ["OLD", "NEW"],
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append", "append_file"]
    )]
    replace: Option<Vec<String>>,
    /// Preview the computed changes without saving (always prints JSON)
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct PageBodyArgs {
    #[command(flatten)]
    body: EditBodyArgs,
    /// Replace the section under an exact top-level or layout-column heading on a Confluence Cloud ADF page
    #[arg(
        long,
        num_args = 2,
        value_names = ["HEADING", "MARKDOWN"],
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append", "append_file", "replace"]
    )]
    replace_section: Option<Vec<String>>,
}

impl EditBodyArgs {
    fn read(self) -> Result<(Option<String>, Option<String>, Option<TextReplacement>)> {
        let full = read_body_arg(self.body.as_deref(), self.body_file.as_deref())?;
        let append = read_body_arg(self.append.as_deref(), self.append_file.as_deref())?;
        let replacement = self.replace.map(|values| {
            let mut values = values.into_iter();
            TextReplacement {
                old_text: values.next().expect("clap requires OLD"),
                new_text: values.next().expect("clap requires NEW"),
            }
        });
        Ok((full, append, replacement))
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct SiteAddArgs {
    /// Atlassian site URL (e.g. https://acme.atlassian.net or https://jira.corp.internal)
    pub url: String,
    /// Optional explicit site slug ID (auto-derived from URL if omitted)
    #[arg(long)]
    pub id: Option<String>,
    /// Human-readable site display name
    #[arg(long)]
    pub name: Option<String>,
    /// Shared credential account ID (e.g. a saved --cloud-account)
    #[arg(long)]
    pub account: Option<String>,
    /// Personal Access Token (Bearer PAT)
    #[arg(long)]
    pub pat: Option<String>,
    /// Email / username for API token or Basic auth
    #[arg(long)]
    pub email: Option<String>,
    /// API token or password
    #[arg(long)]
    pub token: Option<String>,
    /// Optional separate Confluence base URL (Data Center)
    #[arg(long = "confluence-url")]
    pub confluence_url: Option<String>,
    /// Cloud vs Data Center API dialect
    #[arg(long, value_enum)]
    pub deployment: Option<DeploymentArg>,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum SitesCommand {
    /// List all registered Atlassian sites and sync/cache status
    #[command(visible_alias = "ls")]
    List {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Register a Cloud or Data Center Atlassian site
    Add(Box<SiteAddArgs>),
    /// Discover all consented Atlassian Cloud sites via OAuth 2.0 (3LO) accessible-resources
    Discover {
        #[arg(long)]
        account: Option<String>,
        #[arg(long = "client-id")]
        client_id: Option<String>,
        #[arg(long = "client-secret")]
        client_secret: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Set the default target site for single-site CLI / HTTP / MCP commands
    Default {
        id: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Enable a site for federated sync, search, and TUI views
    Enable {
        id: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Disable a site without deleting its config or cached data
    Disable {
        id: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Remove a registered site and purge its local cached data
    #[command(visible_alias = "rm")]
    Remove {
        id: String,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background sync daemon
    Start {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Stop the running background sync daemon
    Stop {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Show background sync daemon status
    Status {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Restart the background sync daemon
    Restart {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Run the background sync daemon in the foreground
    Run,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Log in to Atlassian Cloud (OAuth 2.0 3LO / Shared Cloud Account / API Token) or Data Center (PAT/Basic)
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
        /// Data Center only: separate Confluence PAT, stored under the site's confluence_account_id
        #[arg(long = "confluence-pat")]
        confluence_pat: Option<String>,
        /// Credential account ID for --confluence-pat (default: "<account>-confluence")
        #[arg(long = "confluence-account")]
        confluence_account: Option<String>,
        #[arg(long, value_enum)]
        deployment: Option<DeploymentArg>,
        /// Sign in via Atlassian Cloud OAuth 2.0 (3LO) loopback flow and auto-discover all consented sites
        #[arg(long)]
        oauth: bool,
        /// Save a shared Atlassian Cloud account API token reusable across unlimited *.atlassian.net sites
        #[arg(long = "cloud-account")]
        cloud_account: bool,
        /// OAuth 2.0 (3LO) client ID (overrides ATLASSIAN_OAUTH_CLIENT_ID)
        #[arg(long = "client-id")]
        client_id: Option<String>,
        /// Optional OAuth 2.0 (3LO) client secret (overrides ATLASSIAN_OAUTH_CLIENT_SECRET)
        #[arg(long = "client-secret")]
        client_secret: Option<String>,
        /// Credential account ID to store/reuse credentials under
        #[arg(long)]
        account: Option<String>,
        /// Human-readable site name when registering a site during login
        #[arg(long)]
        name: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Manage connected Atlassian Cloud and Data Center sites
    Sites {
        #[command(subcommand)]
        action: Box<SitesCommand>,
    },
    /// Remove stored credentials
    #[command(visible_alias = "out")]
    Logout {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Show the currently authenticated Atlassian user profile
    Whoami {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Diagnose config, SQLite cache, keyring paths, deployment mode, and optional API probe
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Jira projects
    Projects {
        #[arg(short = 'r', long)]
        refresh: bool,
        /// Maximum number of projects to return (default: 50 for --json, all for text)
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Jira Agile boards
    Boards {
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(short = 'r', long)]
        refresh: bool,
        /// Maximum number of boards to return (default: 50 for --json, all for text)
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List active and future sprints for a Jira Agile board
    Sprints {
        board_id: i64,
        /// Maximum number of sprints to return (up to 50 per live call)
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Render a Kanban board by columns in the terminal
    Board {
        board_id: Option<i64>,
        #[arg(long)]
        sprint: Option<i64>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Jira issues
    #[command(visible_alias = "ls")]
    Issues {
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(long)]
        jql: Option<String>,
        #[arg(short = 'm', long)]
        mine: bool,
        #[arg(short = 's', long)]
        status: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Cloud pagination token from next_page_token
        #[arg(long = "page-token", conflicts_with_all = ["start_at", "offline"])]
        page_token: Option<String>,
        /// Data Center 0-based pagination offset from next_start_at
        #[arg(long = "start-at", conflicts_with = "offline")]
        start_at: Option<usize>,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// View Jira issue details (description, linked Confluence pages, subtasks, comments)
    #[command(visible_aliases = ["read", "cat"])]
    View {
        key: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
        /// Accepted for compatibility: compact is the default for --json
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json", conflicts_with = "compact")]
        full: bool,
    },
    /// Create a new Jira issue
    #[command(visible_alias = "new")]
    Create {
        /// Issue summary (shorthand for --summary)
        #[arg(
            value_name = "SUMMARY",
            allow_hyphen_values = true,
            conflicts_with = "summary",
            required_unless_present = "summary"
        )]
        summary_pos: Option<String>,
        /// Project key (default: config default_project)
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(short = 't', long = "type", default_value = "Task")]
        issue_type: String,
        #[arg(short = 's', long, allow_hyphen_values = true)]
        summary: Option<String>,
        #[arg(short = 'b', long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(short = 'P', long)]
        priority: Option<String>,
        /// Account ID, "me", or a display name / email (resolved when the ID is rejected)
        #[arg(short = 'a', long)]
        assignee: Option<String>,
        /// Label to set (repeat for several)
        #[arg(short = 'l', long = "label")]
        label: Vec<String>,
        /// Parent issue key
        #[arg(long)]
        parent: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Edit fields of an existing Jira issue
    #[command(visible_alias = "e")]
    Edit {
        key: String,
        #[arg(short = 's', long, allow_hyphen_values = true)]
        summary: Option<String>,
        #[command(flatten)]
        body: EditBodyArgs,
        #[arg(short = 'P', long)]
        priority: Option<String>,
        /// Account ID, "me", "none" (unassign), or a display name / email (resolved when the ID is rejected)
        #[arg(short = 'a', long)]
        assignee: Option<String>,
        /// Replace all labels with the given set (repeat for several)
        #[arg(short = 'l', long = "label")]
        label: Option<Vec<String>>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Transition a Jira issue workflow status or list available transitions
    #[command(visible_aliases = ["move", "mv"])]
    Transition {
        key: String,
        status: Option<String>,
        #[arg(short = 'c', long, allow_hyphen_values = true)]
        comment: Option<String>,
        #[arg(short = 'l', long)]
        list: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Assign or unassign a Jira issue (supports `me`, account ID, name, or email)
    #[command(visible_alias = "as")]
    Assign {
        key: String,
        /// Account ID, "me", or a display name / email (shorthand for --to)
        #[arg(value_name = "ASSIGNEE", conflicts_with_all = ["to", "unassign"])]
        assignee_pos: Option<String>,
        /// Account ID, "me", or a display name / email (resolved when the ID is rejected)
        #[arg(short = 't', long)]
        to: Option<String>,
        #[arg(short = 'u', long)]
        unassign: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Add a Markdown comment to a Jira issue
    #[command(visible_alias = "c")]
    Comment {
        key: String,
        #[arg(allow_hyphen_values = true)]
        body: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Upload a local file attachment to a Jira issue
    Attach {
        key: String,
        file: PathBuf,
        #[arg(short = 'j', long)]
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Delete a Jira issue
    Delete {
        key: String,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Search Jira issues in local SQLite cache or via remote JQL (for custom raw JQL, pass `--jql`, e.g. `atlassian search 'customField = "foo"' --jql`)
    #[command(visible_alias = "q")]
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Treat `query` as a raw JQL expression verbatim (e.g. `--jql 'customField = "read and write"'`)
        #[arg(long)]
        jql: bool,
        #[arg(short = 'r', long)]
        remote: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// Cloud pagination token from next_page_token (requires --remote or --jql)
        #[arg(long = "page-token", conflicts_with = "start_at")]
        page_token: Option<String>,
        /// Data Center 0-based pagination offset from next_start_at (requires --remote or --jql)
        #[arg(long = "start-at")]
        start_at: Option<usize>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Open a Jira issue web URL in the default browser
    Open { key: String },
    /// Confluence command namespace (`atlassian wiki <subcmd>`)
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Manage the background sync daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Manage opt-in short shell aliases (`j` -> atlassian, `cf` -> conf)
    Alias {
        #[command(subcommand)]
        action: AliasCommand,
    },
    /// Print shell initialization snippet defining short aliases (`j` -> atlassian, `cf` -> conf)
    Init {
        #[arg(value_enum)]
        shell: ShellKind,
        /// Alias name for `atlassian` (Jira TUI & CLI)
        #[arg(long, default_value = DEFAULT_JIRA_ALIAS)]
        jira: String,
        /// Alias name for `conf` (Confluence TUI & CLI)
        #[arg(long, default_value = DEFAULT_CONF_ALIAS)]
        conf: String,
        /// Define aliases even if another command with the same name exists on PATH
        #[arg(long)]
        force: bool,
    },
    /// Check GitHub Releases for updates or self-update the binary
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(short = 'j', long)]
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
        /// Require "Authorization: Bearer <TOKEN>" (mandatory unless --bind is a loopback address)
        #[arg(long)]
        token: Option<String>,
        /// Browser origin allowed to call the API cross-origin, e.g. http://localhost:5173 (repeatable; default: none)
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origin: Vec<String>,
    },
    /// Run the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp {
        /// Expose only read-only query tools and reject write, sync, and download calls
        #[arg(long = "read-only")]
        read_only: bool,
    },
    #[command(flatten)]
    Workflow(JiraWorkflow),
}

#[derive(Subcommand, Debug)]
pub enum ConfluenceCommand {
    /// Log in to Atlassian Cloud (OAuth 2.0 3LO / Shared Cloud Account / API Token) or Data Center (PAT/Basic)
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
        /// Data Center only: separate Confluence PAT, stored under the site's confluence_account_id
        #[arg(long = "confluence-pat")]
        confluence_pat: Option<String>,
        /// Credential account ID for --confluence-pat (default: "<account>-confluence")
        #[arg(long = "confluence-account")]
        confluence_account: Option<String>,
        #[arg(long, value_enum)]
        deployment: Option<DeploymentArg>,
        /// Sign in via Atlassian Cloud OAuth 2.0 (3LO) loopback flow and auto-discover all consented sites
        #[arg(long)]
        oauth: bool,
        /// Save a shared Atlassian Cloud account API token reusable across unlimited *.atlassian.net sites
        #[arg(long = "cloud-account")]
        cloud_account: bool,
        /// OAuth 2.0 (3LO) client ID (overrides ATLASSIAN_OAUTH_CLIENT_ID)
        #[arg(long = "client-id")]
        client_id: Option<String>,
        /// Optional OAuth 2.0 (3LO) client secret (overrides ATLASSIAN_OAUTH_CLIENT_SECRET)
        #[arg(long = "client-secret")]
        client_secret: Option<String>,
        /// Credential account ID to store/reuse credentials under
        #[arg(long)]
        account: Option<String>,
        /// Human-readable site name when registering a site during login
        #[arg(long)]
        name: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Manage connected Atlassian Cloud and Data Center sites
    Sites {
        #[command(subcommand)]
        action: Box<SitesCommand>,
    },
    /// Remove stored credentials
    #[command(visible_alias = "out")]
    Logout {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Show the currently authenticated Atlassian user profile
    Whoami {
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Diagnose config, SQLite cache, keyring paths, deployment mode, and optional API probe
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Confluence spaces
    Spaces {
        #[arg(short = 'r', long)]
        refresh: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List root pages in a space or child pages under a parent page
    #[command(visible_alias = "tree")]
    Pages {
        space: Option<String>,
        #[arg(short = 'p', long)]
        parent: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Read Confluence page body and metadata
    #[command(visible_aliases = ["read", "cat"])]
    View {
        page_id: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
        /// Accepted for compatibility: compact is the default for --json
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json", conflicts_with = "compact")]
        full: bool,
    },
    /// Create a new Confluence page
    #[command(visible_alias = "new")]
    Create {
        /// Page title (shorthand for --title)
        #[arg(
            value_name = "TITLE",
            allow_hyphen_values = true,
            conflicts_with = "title",
            required_unless_present = "title"
        )]
        title_pos: Option<String>,
        #[arg(short = 's', long)]
        space: Option<String>,
        #[arg(short = 'p', long)]
        parent: Option<String>,
        #[arg(short = 't', long, allow_hyphen_values = true)]
        title: Option<String>,
        #[arg(short = 'b', long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Edit a Confluence page
    #[command(visible_alias = "e")]
    Edit {
        page_id: String,
        #[arg(short = 't', long, allow_hyphen_values = true)]
        title: Option<String>,
        #[command(flatten)]
        body: PageBodyArgs,
        /// Current page version from view (required for body edits; optional for title/message-only edits)
        #[arg(long = "prev-version")]
        prev_version: Option<i32>,
        #[arg(short = 'm', long, allow_hyphen_values = true)]
        message: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Add a Markdown comment to a Confluence page
    #[command(visible_alias = "c")]
    Comment {
        page_id: String,
        #[arg(allow_hyphen_values = true)]
        body: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Upload a local file attachment to a Confluence page
    Attach {
        page_id: String,
        file: PathBuf,
        #[arg(short = 'j', long)]
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Delete a Confluence page
    Delete {
        page_id: String,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Search Confluence pages in local SQLite cache or via remote CQL (for custom raw CQL, pass `--cql`, e.g. `confluence search 'customMeta = "foo"' --cql`)
    #[command(visible_alias = "q")]
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Treat `query` as a raw CQL expression verbatim (e.g. `--cql 'customMeta = "alpha or beta"'`)
        #[arg(long)]
        cql: bool,
        #[arg(short = 'r', long)]
        remote: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        #[arg(short = 'j', long)]
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Manage the background sync daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Manage opt-in short shell aliases (`j` -> atlassian, `cf` -> conf)
    Alias {
        #[command(subcommand)]
        action: AliasCommand,
    },
    /// Print shell initialization snippet defining short aliases (`j` -> atlassian, `cf` -> conf)
    Init {
        #[arg(value_enum)]
        shell: ShellKind,
        /// Alias name for `atlassian` (Jira TUI & CLI)
        #[arg(long, default_value = DEFAULT_JIRA_ALIAS)]
        jira: String,
        /// Alias name for `conf` (Confluence TUI & CLI)
        #[arg(long, default_value = DEFAULT_CONF_ALIAS)]
        conf: String,
        /// Define aliases even if another command with the same name exists on PATH
        #[arg(long)]
        force: bool,
    },
    /// Check GitHub Releases for updates or self-update the binary
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(short = 'j', long)]
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
        /// Require "Authorization: Bearer <TOKEN>" (mandatory unless --bind is a loopback address)
        #[arg(long)]
        token: Option<String>,
        /// Browser origin allowed to call the API cross-origin, e.g. http://localhost:5173 (repeatable; default: none)
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origin: Vec<String>,
    },
    /// Run the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp {
        /// Expose only read-only query tools and reject write, sync, and download calls
        #[arg(long = "read-only")]
        read_only: bool,
    },
    #[command(flatten)]
    Workflow(ConfluenceWorkflow),
}

#[derive(Subcommand, Debug)]
pub enum WikiCommand {
    /// List Confluence spaces
    Spaces {
        #[arg(short = 'r', long)]
        refresh: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List root pages in a space or child pages under a parent page
    #[command(visible_alias = "tree")]
    Pages {
        space: Option<String>,
        #[arg(short = 'p', long)]
        parent: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Read a Confluence page
    #[command(visible_aliases = ["read", "cat"])]
    View {
        page_id: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
        /// Accepted for compatibility: compact is the default for --json
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json", conflicts_with = "compact")]
        full: bool,
    },
    /// Create a new Confluence page
    #[command(visible_alias = "new")]
    Create {
        /// Page title (shorthand for --title)
        #[arg(
            value_name = "TITLE",
            allow_hyphen_values = true,
            conflicts_with = "title",
            required_unless_present = "title"
        )]
        title_pos: Option<String>,
        #[arg(short = 's', long)]
        space: Option<String>,
        #[arg(short = 'p', long)]
        parent: Option<String>,
        #[arg(short = 't', long, allow_hyphen_values = true)]
        title: Option<String>,
        #[arg(short = 'b', long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Edit a Confluence page
    #[command(visible_alias = "e")]
    Edit {
        page_id: String,
        #[arg(short = 't', long, allow_hyphen_values = true)]
        title: Option<String>,
        #[command(flatten)]
        body: PageBodyArgs,
        /// Current page version from view (required for body edits; optional for title/message-only edits)
        #[arg(long = "prev-version")]
        prev_version: Option<i32>,
        #[arg(short = 'm', long, allow_hyphen_values = true)]
        message: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Add a comment to a Confluence page
    #[command(visible_alias = "c")]
    Comment {
        page_id: String,
        #[arg(allow_hyphen_values = true)]
        body: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Attach a file to a Confluence page
    Attach {
        page_id: String,
        file: PathBuf,
        #[arg(short = 'j', long)]
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Delete a Confluence page
    Delete {
        page_id: String,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Search Confluence pages (for custom raw CQL, pass `--cql`, e.g. `atlassian wiki search 'customMeta = "foo"' --cql`)
    #[command(visible_alias = "q")]
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Treat `query` as a raw CQL expression verbatim
        #[arg(long)]
        cql: bool,
        #[arg(short = 'r', long)]
        remote: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Open a Confluence page web URL
    Open { page_id: String },
    #[command(flatten)]
    Workflow(ConfluenceWorkflow),
}

#[derive(Subcommand, Debug)]
pub enum JiraSubcommand {
    /// List Jira projects
    Projects {
        #[arg(short = 'r', long)]
        refresh: bool,
        /// Maximum number of projects to return (default: 50 for --json, all for text)
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Jira Agile boards
    Boards {
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(short = 'r', long)]
        refresh: bool,
        /// Maximum number of boards to return (default: 50 for --json, all for text)
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List active and future sprints for a board
    Sprints {
        board_id: i64,
        /// Maximum number of sprints to return (up to 50 per live call)
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Pagination offset cursor from next_cursor
        #[arg(long)]
        cursor: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Render a Kanban board by columns
    Board {
        board_id: Option<i64>,
        #[arg(long)]
        sprint: Option<i64>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List Jira issues
    #[command(visible_alias = "ls")]
    Issues {
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(long)]
        jql: Option<String>,
        #[arg(short = 'm', long)]
        mine: bool,
        #[arg(short = 's', long)]
        status: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Cloud pagination token from next_page_token
        #[arg(long = "page-token", conflicts_with_all = ["start_at", "offline"])]
        page_token: Option<String>,
        /// Data Center 0-based pagination offset from next_start_at
        #[arg(long = "start-at", conflicts_with = "offline")]
        start_at: Option<usize>,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// View Jira issue details
    #[command(visible_aliases = ["read", "cat"])]
    View {
        key: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(short = 'o', long)]
        offline: bool,
        #[arg(short = 'j', long)]
        json: bool,
        /// Accepted for compatibility: compact is the default for --json
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json", conflicts_with = "compact")]
        full: bool,
    },
    /// Create a new Jira issue
    #[command(visible_alias = "new")]
    Create {
        /// Issue summary (shorthand for --summary)
        #[arg(
            value_name = "SUMMARY",
            allow_hyphen_values = true,
            conflicts_with = "summary",
            required_unless_present = "summary"
        )]
        summary_pos: Option<String>,
        /// Project key (default: config default_project)
        #[arg(short = 'p', long)]
        project: Option<String>,
        #[arg(short = 't', long = "type", default_value = "Task")]
        issue_type: String,
        #[arg(short = 's', long, allow_hyphen_values = true)]
        summary: Option<String>,
        #[arg(short = 'b', long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "body-file")]
        body_file: Option<PathBuf>,
        #[arg(short = 'P', long)]
        priority: Option<String>,
        /// Account ID, "me", or a display name / email (resolved when the ID is rejected)
        #[arg(short = 'a', long)]
        assignee: Option<String>,
        /// Label to set (repeat for several)
        #[arg(short = 'l', long = "label")]
        label: Vec<String>,
        /// Parent issue key
        #[arg(long)]
        parent: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Edit a Jira issue
    #[command(visible_alias = "e")]
    Edit {
        key: String,
        #[arg(short = 's', long, allow_hyphen_values = true)]
        summary: Option<String>,
        #[command(flatten)]
        body: EditBodyArgs,
        #[arg(short = 'P', long)]
        priority: Option<String>,
        /// Account ID, "me", "none" (unassign), or a display name / email (resolved when the ID is rejected)
        #[arg(short = 'a', long)]
        assignee: Option<String>,
        /// Replace all labels with the given set (repeat for several)
        #[arg(short = 'l', long = "label")]
        label: Option<Vec<String>>,
        #[arg(short = 'j', long)]
        json: bool,
        /// Return the full structure: native body, blocks, comments, attachments
        #[arg(short = 'f', long, requires = "json")]
        full: bool,
    },
    /// Transition a Jira issue status
    #[command(visible_aliases = ["move", "mv"])]
    Transition {
        key: String,
        status: Option<String>,
        #[arg(short = 'c', long, allow_hyphen_values = true)]
        comment: Option<String>,
        #[arg(short = 'l', long)]
        list: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Assign or unassign a Jira issue
    #[command(visible_alias = "as")]
    Assign {
        key: String,
        /// Account ID, "me", or a display name / email (shorthand for --to)
        #[arg(value_name = "ASSIGNEE", conflicts_with_all = ["to", "unassign"])]
        assignee_pos: Option<String>,
        /// Account ID, "me", or a display name / email (resolved when the ID is rejected)
        #[arg(short = 't', long)]
        to: Option<String>,
        #[arg(short = 'u', long)]
        unassign: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Add a comment to a Jira issue
    #[command(visible_alias = "c")]
    Comment {
        key: String,
        #[arg(allow_hyphen_values = true)]
        body: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Attach a file to a Jira issue
    Attach {
        key: String,
        file: PathBuf,
        #[arg(short = 'j', long)]
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
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Delete a Jira issue
    Delete {
        key: String,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Search Jira issues (for custom raw JQL, pass `--jql`, e.g. `confluence jira search 'customField = "foo"' --jql`)
    #[command(visible_alias = "q")]
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Treat `query` as a raw JQL expression verbatim
        #[arg(long)]
        jql: bool,
        #[arg(short = 'r', long)]
        remote: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// Cloud pagination token from next_page_token (requires --remote or --jql)
        #[arg(long = "page-token", conflicts_with = "start_at")]
        page_token: Option<String>,
        /// Data Center 0-based pagination offset from next_start_at (requires --remote or --jql)
        #[arg(long = "start-at")]
        start_at: Option<usize>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Open a Jira issue web URL
    Open { key: String },
    #[command(flatten)]
    Workflow(JiraWorkflow),
}

#[derive(Subcommand, Debug)]
pub enum JiraWorkflow {
    /// Set a Cloud parent or assign an eligible Data Center issue to an epic
    Parent {
        key: String,
        parent_key: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Update explicit custom field IDs with native JSON values
    CustomFields {
        key: String,
        /// JSON object mapping customfield_<digits> to Jira API values
        #[arg(
            long,
            required_unless_present = "fields_file",
            conflicts_with = "fields_file"
        )]
        fields_json: Option<String>,
        /// Read the JSON object from a UTF-8 file
        #[arg(long)]
        fields_file: Option<PathBuf>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Create a Jira link using its named type and explicit link ends
    Link {
        inward_key: String,
        outward_key: String,
        #[arg(short = 't', long = "type")]
        link_type: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Move 1 to 50 existing issues to an open or active sprint
    MoveToSprint {
        sprint_id: i64,
        #[arg(required = true, num_args = 1..=50)]
        keys: Vec<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Record positive seconds while keeping the remaining estimate
    LogWork {
        key: String,
        #[arg(long)]
        seconds: i32,
        /// RFC 3339 start time with a timezone
        #[arg(long)]
        started: Option<String>,
        /// Markdown worklog comment
        #[arg(short = 'c', long, allow_hyphen_values = true)]
        comment: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfluenceWorkflow {
    /// Move a page under a parent page
    Move {
        page_id: String,
        parent_id: String,
        /// Current page version required on Data Center
        #[arg(long)]
        prev_version: Option<i32>,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Add global labels or remove one label by name
    Labels {
        page_id: String,
        #[arg(long, required_unless_present = "remove", conflicts_with = "remove")]
        add: Vec<String>,
        #[arg(long)]
        remove: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KnownFlagToken {
    Help,
    Other,
}

fn find_matching_option<'a>(
    active: &'a clap::Command,
    root: &'a clap::Command,
    flag: &str,
) -> Option<&'a clap::Arg> {
    if let Some(long_name) = flag.strip_prefix("--") {
        if long_name.is_empty() {
            return None;
        }
        let matches_long = |arg: &&clap::Arg| {
            !arg.is_positional()
                && (arg.get_long() == Some(long_name)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&long_name)))
        };
        active
            .get_arguments()
            .find(matches_long)
            .or_else(|| {
                root.get_arguments()
                    .filter(|arg| arg.is_global_set())
                    .find(matches_long)
            })
    } else if flag.starts_with('-') && flag.len() == 2 {
        let ch = flag.as_bytes()[1] as char;
        let matches_short = |arg: &&clap::Arg| {
            !arg.is_positional()
                && (arg.get_short() == Some(ch)
                    || arg
                        .get_all_short_aliases()
                        .is_some_and(|aliases| aliases.contains(&ch)))
        };
        active
            .get_arguments()
            .find(matches_short)
            .or_else(|| {
                root.get_arguments()
                    .filter(|arg| arg.is_global_set())
                    .find(matches_short)
            })
    } else {
        None
    }
}

fn classify_known_flag_or_terminator(
    active: &clap::Command,
    root: &clap::Command,
    token: &OsStr,
) -> Option<KnownFlagToken> {
    let text = token.to_str()?;
    if text == "--" {
        return Some(KnownFlagToken::Other);
    }
    if (text == "-h" || text == "--help") && !active.is_disable_help_flag_set() {
        return Some(KnownFlagToken::Help);
    }
    if (text == "-V" || text == "--version")
        && active.get_version().is_some()
        && !active.is_disable_version_flag_set()
    {
        return Some(KnownFlagToken::Other);
    }
    if text.starts_with("--") {
        let flag = text.split_once('=').map_or(text, |(left, _)| left);
        return find_matching_option(active, root, flag).map(|_| KnownFlagToken::Other);
    }
    if text.starts_with('-') && text.len() == 2 {
        return find_matching_option(active, root, text).map(|_| KnownFlagToken::Other);
    }
    if let Some(cluster) = text.strip_prefix('-') {
        if cluster.len() >= 2
            && !cluster.starts_with('-')
            && cluster.bytes().all(|b| b.is_ascii_alphabetic())
        {
            let mut single = [b'-', 0];
            let all_known = cluster.bytes().all(|b| {
                single[1] = b;
                let flag = std::str::from_utf8(&single).unwrap_or("");
                find_matching_option(active, root, flag).is_some()
            });
            if all_known {
                return Some(KnownFlagToken::Other);
            }
        }
    }
    None
}

fn is_jira_issue_key(token: &str) -> bool {
    let Some((project, number)) = token.split_once('-') else {
        return false;
    };
    if project.is_empty() || number.is_empty() || number.contains('-') {
        return false;
    }
    let mut chars = project.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && number.bytes().all(|b| b.is_ascii_digit())
}

fn is_confluence_page_id(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

fn has_subcommand_named(cmd: &clap::Command, token: &str) -> bool {
    cmd.get_subcommands()
        .any(|sub| sub.get_name() == token || sub.get_all_aliases().any(|a| a == token))
}

/// Expands shorthand target lookups into explicit `view` invocations:
/// - `atlassian ENG-42 [-j]` -> `atlassian view ENG-42 [-j]`
/// - `conf 40211 [-j]` -> `conf view 40211 [-j]`
/// - `atlassian wiki 40211 [-j]` -> `atlassian wiki view 40211 [-j]`
/// - `conf jira ENG-42 [-j]` -> `conf jira view ENG-42 [-j]`
fn expand_smart_view_shortcut(cmd: &clap::Command, name: &str, argv: &mut Vec<OsString>) {
    let is_confluence_root = matches!(name, "confluence" | "conf");
    let mut i = 1usize;
    while i < argv.len() {
        let Some(text) = argv[i].to_str() else {
            return;
        };
        if text == "--" || text == "-h" || text == "--help" || text == "-V" || text == "--version" {
            return;
        }
        if text == "--site" {
            i += 2;
            continue;
        }
        if text.starts_with("--site=") {
            i += 1;
            continue;
        }
        if text.starts_with('-') {
            return;
        }

        // Handle cross-mounted namespace shortcuts: `atlassian wiki 40211` / `conf jira ENG-42`
        if !is_confluence_root && matches!(text, "wiki" | "conf") {
            if let Some(next_tok) = argv.get(i + 1).and_then(|s| s.to_str()) {
                if let Some(wiki_sub) = cmd.find_subcommand("wiki") {
                    if !next_tok.starts_with('-')
                        && !has_subcommand_named(wiki_sub, next_tok)
                        && is_confluence_page_id(next_tok)
                    {
                        argv.insert(i + 1, OsString::from("view"));
                    }
                }
            }
            return;
        }
        if is_confluence_root && text == "jira" {
            if let Some(next_tok) = argv.get(i + 1).and_then(|s| s.to_str()) {
                if let Some(jira_sub) = cmd.find_subcommand("jira") {
                    if !next_tok.starts_with('-')
                        && !has_subcommand_named(jira_sub, next_tok)
                        && is_jira_issue_key(next_tok)
                    {
                        argv.insert(i + 1, OsString::from("view"));
                    }
                }
            }
            return;
        }

        if has_subcommand_named(cmd, text) {
            return;
        }
        let matches_target = if is_confluence_root {
            is_confluence_page_id(text)
        } else {
            is_jira_issue_key(text)
        };
        if matches_target {
            argv.insert(i, OsString::from("view"));
        }
        return;
    }
}

fn disable_hyphen_values_on_arg(
    cmd: clap::Command,
    subcmd_path: &[String],
    arg_id: &str,
) -> clap::Command {
    if let Some((first, rest)) = subcmd_path.split_first() {
        cmd.mut_subcommand(first, |sub| {
            disable_hyphen_values_on_arg(sub, rest, arg_id)
        })
    } else {
        cmd.mut_arg(arg_id, |arg| arg.allow_hyphen_values(false))
    }
}

enum OptionFlagViolation {
    Help(Vec<OsString>),
    FlagInValue {
        subcmd_path: Vec<String>,
        arg_id: String,
        err_argv: Vec<OsString>,
    },
}

fn has_potential_flag_in_option_value(argv: &[OsString]) -> bool {
    let mut dash_tokens = 0usize;
    for token in argv.iter().skip(1) {
        let Some(text) = token.to_str() else {
            continue;
        };
        if text.starts_with('-') && text != "-" {
            dash_tokens += 1;
            if dash_tokens >= 2 {
                return true;
            }
            if text == "--" {
                break;
            }
        }
    }
    false
}

fn find_option_flag_violation(
    cmd: &clap::Command,
    argv: &[OsString],
) -> Option<OptionFlagViolation> {
    let mut active = cmd;
    let mut subcmd_path: Vec<String> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let token = &argv[i];
        if token == "--" {
            break;
        }
        if let Some(text) = token.to_str() {
            if (text == "-h" || text == "--help") && !active.is_disable_help_flag_set() {
                return None;
            }
            if let Some((flag, _)) = text.split_once('=') {
                if flag.starts_with("--") && find_matching_option(active, cmd, flag).is_some() {
                    i += 1;
                    continue;
                }
            }
            if let Some(opt) = find_matching_option(active, cmd, text) {
                let takes = opt
                    .get_num_args()
                    .map(|range| range.min_values())
                    .unwrap_or_else(|| usize::from(opt.get_action().takes_values()));
                if takes == 0 {
                    i += 1;
                    continue;
                }
                if opt.is_allow_hyphen_values_set() {
                    for k in 0..takes {
                        let Some(next_tok) = argv.get(i + 1 + k) else {
                            break;
                        };
                        if let Some(kind) =
                            classify_known_flag_or_terminator(active, cmd, next_tok)
                        {
                            if kind == KnownFlagToken::Help {
                                let mut help_argv = vec![argv[0].clone()];
                                help_argv.extend(subcmd_path.iter().map(OsString::from));
                                help_argv.push(next_tok.clone());
                                return Some(OptionFlagViolation::Help(help_argv));
                            }
                            let mut err_argv = argv.to_vec();
                            for prev in 0..k {
                                err_argv[i + 1 + prev] = OsString::from("placeholder");
                            }
                            return Some(OptionFlagViolation::FlagInValue {
                                subcmd_path,
                                arg_id: opt.get_id().as_str().to_string(),
                                err_argv,
                            });
                        }
                    }
                    i += 1 + takes;
                    continue;
                }
                let mut consumed = 0;
                while consumed < takes {
                    let Some(next_tok) = argv.get(i + 1 + consumed) else {
                        break;
                    };
                    if next_tok
                        .to_str()
                        .is_some_and(|s| s.starts_with('-') && s != "-")
                    {
                        break;
                    }
                    consumed += 1;
                }
                i += 1 + consumed;
                continue;
            }
            if active.has_subcommands() {
                if let Some(sub) = active.get_subcommands().find(|sub| {
                    sub.get_name() == text
                        || sub.get_all_aliases().any(|alias| alias == text)
                }) {
                    subcmd_path.push(sub.get_name().to_string());
                    active = sub;
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn try_parse_cli_from<T: Parser>(
    name: &'static str,
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<T, clap::Error> {
    let mut argv: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let cmd = T::command().name(name).bin_name(name);
    expand_smart_view_shortcut(&cmd, name, &mut argv);

    if has_potential_flag_in_option_value(&argv) {
        if let Some(violation) = find_option_flag_violation(&cmd, &argv) {
            let parsed = match violation {
                OptionFlagViolation::Help(help_argv) => cmd.try_get_matches_from(help_argv),
                OptionFlagViolation::FlagInValue {
                    subcmd_path,
                    arg_id,
                    err_argv,
                } => {
                    let err_cmd = disable_hyphen_values_on_arg(cmd, &subcmd_path, &arg_id);
                    err_cmd.try_get_matches_from(err_argv)
                }
            };
            return Err(parsed.err().unwrap_or_else(|| {
                clap::Error::raw(
                    clap::error::ErrorKind::InvalidValue,
                    "expected a value before the next option",
                )
            }));
        }
    }

    let mut matches = cmd.try_get_matches_from(argv)?;
    T::from_arg_matches_mut(&mut matches)
}

fn parse_cli_with_stack<T: Parser + Send + 'static>(name: &'static str) -> T {
    let parse = move || {
        try_parse_cli_from::<T>(name, std::env::args_os()).unwrap_or_else(|error| error.exit())
    };
    std::thread::Builder::new()
        .name("atlassian-cli-parse".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(parse)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(parse)
}

pub async fn run() -> Result<()> {
    let mut cfg = AppConfig::load()?;
    let persona = Persona::detect();
    match persona {
        Persona::Jira => {
            let cli: Cli = parse_cli_with_stack("atlassian");
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
            let cli: ConfluenceCli = parse_cli_with_stack(name);
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

fn apply_site_override(cfg: &AppConfig, site_override: Option<&str>) -> Result<AppConfig> {
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
            Box::pin(cmd_tui(&mut run_cfg, persona)).await
        }
        Some(Command::Login {
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
        }) => {
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
        Some(Command::Sites { action }) => Box::pin(cmd_sites(cfg, *action)).await,
        Some(Command::Logout { json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            cmd_logout(&scoped, json)
        }
        Some(Command::Whoami { json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_whoami(&scoped, persona, json)).await
        }
        Some(Command::Doctor { probe, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_doctor(&scoped, persona, probe, json)).await
        }
        Some(Command::Projects {
            refresh,
            limit,
            cursor,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_projects(&scoped, refresh, limit, cursor.as_deref(), json)).await
        }
        Some(Command::Boards {
            project,
            refresh,
            limit,
            cursor,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_boards(
                &scoped,
                project.as_deref(),
                refresh,
                limit,
                cursor.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Sprints {
            board_id,
            limit,
            cursor,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_sprints(&scoped, board_id, limit, cursor.as_deref(), json)).await
        }
        Some(Command::Board {
            board_id,
            sprint,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_board(&scoped, board_id, sprint, json)).await
        }
        Some(Command::Issues {
            project,
            jql,
            mine,
            status,
            limit,
            page_token,
            start_at,
            offline,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issues(
                &scoped,
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
        Some(Command::Workflow(command)) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(dispatch_jira_workflow(&scoped, command)).await
        }
        Some(Command::View {
            key,
            preview_images,
            offline,
            json,
            compact: _,
            full,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_view(
                &scoped,
                &key,
                preview_images,
                offline,
                json,
                !full,
            ))
            .await
        }
        Some(Command::Create {
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
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            let resolved_summary = summary.or(summary_pos).unwrap_or_default();
            Box::pin(cmd_issue_create(
                &scoped,
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
        Some(Command::Edit {
            key,
            summary,
            body,
            priority,
            assignee,
            label,
            json,
            full,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_edit(
                &scoped, &key, summary, body, priority, assignee, label, json, full,
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
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_transition(
                &scoped,
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
            assignee_pos,
            to,
            unassign,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            let resolved_to = to.or(assignee_pos);
            Box::pin(cmd_issue_assign(
                &scoped,
                &key,
                resolved_to.as_deref(),
                unassign,
                json,
            ))
            .await
        }
        Some(Command::Comment { key, body, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_comment(&scoped, &key, &body, json)).await
        }
        Some(Command::Attach { key, file, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_attach(&scoped, &key, &file, json)).await
        }
        Some(Command::Download {
            key,
            attachment_id,
            dir,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_download(
                &scoped,
                &key,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Delete { key, yes, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_delete(&scoped, &key, yes, json)).await
        }
        Some(Command::Search {
            query,
            jql,
            remote,
            limit,
            page_token,
            start_at,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_search(
                &scoped,
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
        Some(Command::Open { key }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_issue_open(&scoped, &key)).await
        }
        Some(Command::Wiki { command }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(dispatch_wiki_subcommand(&scoped, command)).await
        }
        Some(Command::Preview {
            target_id_or_key,
            attachment_id,
            cols,
            rows,
        }) => {
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
        Some(Command::PreviewFile { path, cols, rows }) => cmd_preview_file(&path, cols, rows),
        Some(Command::Sync { json }) => {
            Box::pin(cmd_sync(cfg, site_override.as_deref(), json)).await
        }
        Some(Command::Daemon { action }) => Box::pin(cmd_daemon(cfg, action)).await,
        Some(Command::Alias { action }) => shell_alias::cmd_alias(action),
        Some(Command::Init {
            shell,
            jira,
            conf,
            force,
        }) => shell_alias::cmd_init(shell, &jira, &conf, force),
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_tui(&mut run_cfg, persona)).await
        }
        Some(Command::Serve {
            bind,
            port,
            token,
            cors_origin,
        }) => {
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
        Some(Command::Mcp { read_only }) => {
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

async fn dispatch_confluence_cli(
    cfg: &mut AppConfig,
    persona: Persona,
    site_override: Option<String>,
    command: Option<ConfluenceCommand>,
) -> Result<()> {
    match command {
        None => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_tui(&mut run_cfg, persona)).await
        }
        Some(ConfluenceCommand::Login {
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
        }) => {
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
        Some(ConfluenceCommand::Sites { action }) => Box::pin(cmd_sites(cfg, *action)).await,
        Some(ConfluenceCommand::Logout { json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            cmd_logout(&scoped, json)
        }
        Some(ConfluenceCommand::Whoami { json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_whoami(&scoped, persona, json)).await
        }
        Some(ConfluenceCommand::Doctor { probe, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_doctor(&scoped, persona, probe, json)).await
        }
        Some(ConfluenceCommand::Spaces {
            refresh,
            limit,
            cursor,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_spaces(&scoped, refresh, limit, cursor.as_deref(), json)).await
        }
        Some(ConfluenceCommand::Pages {
            space,
            parent,
            limit,
            cursor,
            offline,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_pages(
                &scoped,
                space.as_deref(),
                parent.as_deref(),
                limit,
                cursor.as_deref(),
                offline,
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Workflow(command)) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(dispatch_confluence_workflow(&scoped, command)).await
        }
        Some(ConfluenceCommand::View {
            page_id,
            preview_images,
            offline,
            json,
            compact: _,
            full,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_view(
                &scoped,
                &page_id,
                preview_images,
                offline,
                json,
                !full,
            ))
            .await
        }
        Some(ConfluenceCommand::Create {
            title_pos,
            space,
            parent,
            title,
            body,
            body_file,
            json,
            full,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            let resolved_title = title.or(title_pos).unwrap_or_default();
            Box::pin(cmd_page_create(
                &scoped,
                space.as_deref(),
                parent.as_deref(),
                &resolved_title,
                body.as_deref(),
                body_file.as_deref(),
                json,
                full,
            ))
            .await
        }
        Some(ConfluenceCommand::Edit {
            page_id,
            title,
            body,
            prev_version,
            message,
            json,
            full,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_edit(
                &scoped,
                &page_id,
                title,
                body,
                prev_version,
                message,
                json,
                full,
            ))
            .await
        }
        Some(ConfluenceCommand::Comment {
            page_id,
            body,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_comment(&scoped, &page_id, &body, json)).await
        }
        Some(ConfluenceCommand::Attach {
            page_id,
            file,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_attach(&scoped, &page_id, &file, json)).await
        }
        Some(ConfluenceCommand::Download {
            page_id,
            attachment_id,
            dir,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_download(
                &scoped,
                &page_id,
                attachment_id.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        Some(ConfluenceCommand::Delete { page_id, yes, json }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_delete(&scoped, &page_id, yes, json)).await
        }
        Some(ConfluenceCommand::Search {
            query,
            cql,
            remote,
            limit,
            json,
        }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_search(&scoped, &query, cql, remote, limit, json)).await
        }
        Some(ConfluenceCommand::Open { page_id }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_page_open(&scoped, &page_id)).await
        }
        Some(ConfluenceCommand::Jira { command }) => {
            let scoped = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(dispatch_jira_subcommand(&scoped, command)).await
        }
        Some(ConfluenceCommand::Preview {
            target_id_or_key,
            attachment_id,
            cols,
            rows,
        }) => {
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
        Some(ConfluenceCommand::PreviewFile { path, cols, rows }) => {
            cmd_preview_file(&path, cols, rows)
        }
        Some(ConfluenceCommand::Sync { json }) => {
            Box::pin(cmd_sync(cfg, site_override.as_deref(), json)).await
        }
        Some(ConfluenceCommand::Daemon { action }) => Box::pin(cmd_daemon(cfg, action)).await,
        Some(ConfluenceCommand::Alias { action }) => shell_alias::cmd_alias(action),
        Some(ConfluenceCommand::Init {
            shell,
            jira,
            conf,
            force,
        }) => shell_alias::cmd_init(shell, &jira, &conf, force),
        Some(ConfluenceCommand::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(ConfluenceCommand::Tui) => {
            let mut run_cfg = apply_site_override(cfg, site_override.as_deref())?;
            Box::pin(cmd_tui(&mut run_cfg, persona)).await
        }
        Some(ConfluenceCommand::Serve {
            bind,
            port,
            token,
            cors_origin,
        }) => {
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
        Some(ConfluenceCommand::Mcp { read_only }) => {
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

async fn dispatch_wiki_subcommand(cfg: &AppConfig, command: WikiCommand) -> Result<()> {
    match command {
        WikiCommand::Spaces {
            refresh,
            limit,
            cursor,
            json,
        } => Box::pin(cmd_spaces(cfg, refresh, limit, cursor.as_deref(), json)).await,
        WikiCommand::Pages {
            space,
            parent,
            limit,
            cursor,
            offline,
            json,
        } => {
            Box::pin(cmd_pages(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                limit,
                cursor.as_deref(),
                offline,
                json,
            ))
            .await
        }
        WikiCommand::Workflow(command) => {
            Box::pin(dispatch_confluence_workflow(cfg, command)).await
        }
        WikiCommand::View {
            page_id,
            preview_images,
            offline,
            json,
            compact: _,
            full,
        } => {
            Box::pin(cmd_page_view(
                cfg,
                &page_id,
                preview_images,
                offline,
                json,
                !full,
            ))
            .await
        }
        WikiCommand::Create {
            title_pos,
            space,
            parent,
            title,
            body,
            body_file,
            json,
            full,
        } => {
            let resolved_title = title.or(title_pos).unwrap_or_default();
            Box::pin(cmd_page_create(
                cfg,
                space.as_deref(),
                parent.as_deref(),
                &resolved_title,
                body.as_deref(),
                body_file.as_deref(),
                json,
                full,
            ))
            .await
        }
        WikiCommand::Edit {
            page_id,
            title,
            body,
            prev_version,
            message,
            json,
            full,
        } => {
            Box::pin(cmd_page_edit(
                cfg,
                &page_id,
                title,
                body,
                prev_version,
                message,
                json,
                full,
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
        } => {
            Box::pin(cmd_issue_view(
                cfg,
                &key,
                preview_images,
                offline,
                json,
                !full,
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

// ---------------------------------------------------------------------------
// Runtime & Auth
// ---------------------------------------------------------------------------

struct LoginParams {
    url: Option<String>,
    confluence_url: Option<String>,
    email: Option<String>,
    token: Option<String>,
    pat: Option<String>,
    confluence_pat: Option<String>,
    confluence_account: Option<String>,
    deployment: Option<DeploymentKind>,
    oauth: bool,
    cloud_account: bool,
    client_id: Option<String>,
    client_secret: Option<String>,
    account: Option<String>,
    name: Option<String>,
}

fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, AtlassianClient, Storage)> {
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let (auth, client) = open_client(cfg, require_login)?;
    Ok((auth, client, storage))
}

fn open_client(cfg: &AppConfig, require_login: bool) -> Result<(AuthManager, AtlassianClient)> {
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

async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
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

async fn cmd_login(
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
fn decide_login_identity(persona: Persona, probe: &IdentityProbe) -> Result<crate::models::UserSummary> {
    probe.login_user(persona)
}

/// Warning for a Data Center login whose Jira credential Confluence did not accept.
fn confluence_login_warning(
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
fn identity_json(probe: &IdentityProbe) -> Value {
    let (jira, confluence) = probe.status_labels();
    json!({ "jira": jira, "confluence": confluence })
}

/// Next steps appended to a login failure (credential rejected, or the hidden prompt
/// returned nothing — common when pasting into Windows consoles).
fn login_failure_help(persona: Persona, site_url: &str, deployment: DeploymentKind) -> String {
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
fn with_login_help(err: anyhow::Error, help: &str) -> anyhow::Error {
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

async fn cmd_sites(cfg: &mut AppConfig, action: SitesCommand) -> Result<()> {
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
                    "{:<16} {:<18} {:<12} {:<10} {:<8} {:<8} {}",
                    "ID", "NAME", "DEPLOYMENT", "ENABLED", "ISSUES", "PAGES", "URL"
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

fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
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

async fn refresh_cli_page_cache(client: &AtlassianClient, storage: &Storage, page_id: &str) {
    let site_id = client.site_id();
    if let Ok((mut detail, meta)) = client
        .get_page_with_meta_in_cache(page_id, site_id, storage)
        .await
    {
        let _ = storage.upsert_page_detail_for_site_with_meta(site_id, &mut detail, meta);
    }
}

async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
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

/// "tls" object of the doctor report. Lists CA bundle paths and certificate counts only,
/// never certificate contents.
fn tls_report(sources: &[crate::atlassian::client::ExtraCaSource]) -> Value {
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

fn tls_roots_line(sources: &[crate::atlassian::client::ExtraCaSource]) -> String {
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
fn env_credentials_report(cfg: &AppConfig) -> Value {
    let env = crate::atlassian::client::PoolEnv::from_process_env();
    json!({
        "present": env.credentials_site_url.is_some(),
        "applies_to_site": env.credentials_site_id(&cfg.effective_sites()),
    })
}

/// "env:ATLASSIAN_CONFIG_DIR" when the override is set, else "default".
fn config_dir_source(env_override: Option<&str>) -> &'static str {
    if env_override.is_some_and(|v| !v.trim().is_empty()) {
        "env:ATLASSIAN_CONFIG_DIR"
    } else {
        "default"
    }
}

/// Everything "doctor --json" reports, gathered before printing.
struct DoctorFacts<'a> {
    persona: Persona,
    profile: &'a str,
    site_id: &'a str,
    credential_account: &'a str,
    confluence_credential_account: Option<&'a str>,
    env_credentials: Value,
    tls: Value,
    deployment: &'a str,
    site_url: &'a str,
    confluence_url: &'a str,
    logged_in: bool,
    auth_status: &'a str,
    config_dir: &'a Path,
    config_dir_source: &'a str,
    data_dir: &'a Path,
    config_path: &'a Path,
    credentials_fallback_path: &'a Path,
    db_path: &'a Path,
    download_dir: &'a Path,
    cache_counts: Value,
    /// "None" when "--probe" was not requested.
    probe: Option<&'a IdentityProbe>,
}

/// JSON body of "doctor --json". "probe_me" / "probe_error" describe Jira (as before);
/// "probe_confluence_me" / "probe_confluence_error" describe Confluence.
fn doctor_report(facts: &DoctorFacts<'_>) -> Value {
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

async fn cmd_doctor(
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
                p.key,
                p.name,
                p.project_type,
                p.lead_name.as_deref().unwrap_or("-")
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
                b.name,
                b.board_type,
                b.project_key.as_deref().unwrap_or("-")
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
                s.state,
                s.name,
                s.start_date.as_deref().unwrap_or("-"),
                s.end_date.as_deref().unwrap_or("-")
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

    if board.columns.is_empty() {
        if let Ok(cols) = client.get_board_columns(board.id).await {
            board.columns = cols;
        }
    }

    let sprints = client.list_board_sprints(board.id).await.unwrap_or_default();
    let _ = storage.upsert_sprints_for_site(site_id, &sprints);

    let active_sprint = if let Some(sid) = sprint_id {
        sprints.iter().find(|s| s.id == sid).cloned()
    } else {
        sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .cloned()
    };

    let chosen_sprint_id = sprint_id.or_else(|| active_sprint.as_ref().map(|s| s.id));
    let issues =
        sync::load_board_issues_for_site(&client, &storage, site_id, board.id, chosen_sprint_id)
            .await?;

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

const OFFLINE_JQL_ERROR: &str =
    "--jql cannot be evaluated offline; use --project/--status/--mine or \"atlassian search <text>\"";
const OFFLINE_MINE_ERROR: &str =
    "--mine --offline needs a cached identity; run \"atlassian whoami\" once online";

/// The cache filter for `issues --offline`: project (or the default project), status and, for
/// `--mine`, the signed-in user `whoami` saved. JQL needs a server, so it is refused.
fn offline_issue_filter<'a>(
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
                issue.key, issue.status.name, prio, assignee, issue.summary
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
        let body = if compact {
            let mut hint = format!("atlassian view {key} --json --full");
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
            refresh_cli_issue_cache(&client, &storage, &inward_key).await;
            refresh_cli_issue_cache(&client, &storage, &outward_key).await;
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
            for key in &confirmed {
                refresh_cli_issue_cache(&client, &storage, key).await;
            }
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

    if list || status.is_none() {
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
    }

    let target = status.unwrap();
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
        let raw = to.unwrap().trim();
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
    refresh_cli_issue_cache(&client, &storage, key).await;
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
    refresh_cli_issue_cache(&client, &storage, key).await;
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
    let detail = client.get_issue(key).await?;
    let attachment = select_attachment(&detail.attachments, attachment_id)
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

async fn cmd_spaces(
    cfg: &AppConfig,
    refresh: bool,
    limit: usize,
    cursor: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let offset = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
    let limit = serve::clamp_limit(limit);
    let (auth, client, storage) = open_runtime(cfg, refresh)?;
    let site_id = client.site_id();
    let cached = storage.list_spaces_for_site(site_id)?;
    let all = if refresh || cached.is_empty() {
        if !refresh && !auth.is_logged_in() {
            cached
        } else {
            let fetched = client.list_all_spaces().await?;
            let _ = storage.upsert_spaces_for_site(site_id, &fetched);
            let stored = storage.list_spaces_for_site(site_id)?;
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

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "spaces": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No spaces found.");
    } else {
        for s in &page.items {
            println!("{:<12} {:<32} [{}]", s.key, s.name, s.space_type);
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
        }
    }
    Ok(())
}

async fn cmd_pages(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let clean_cursor = cursor.map(str::trim).filter(|s| !s.is_empty());
    let first_page = clean_cursor.is_none();
    let offline_offset = if offline {
        parse_offset_cursor(clean_cursor).map_err(anyhow::Error::msg)?
    } else {
        0
    };
    let limit = serve::clamp_limit(limit);
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();

    let page = if let Some(parent_id) = parent {
        if offline {
            let rows = storage.list_child_pages_for_site(site_id, parent_id)?;
            slice_page(rows, offline_offset, limit)
        } else {
            let mut fetched = client
                .list_child_pages_page(parent_id, limit, clean_cursor)
                .await?;
            let _ = storage.record_child_pages_page_for_site(
                site_id,
                parent_id,
                &fetched,
                first_page,
            );
            let _ = storage.overlay_children_state_for_site(site_id, &mut fetched.items);
            fetched
        }
    } else if let Some(sp) = space.or(cfg.default_space_key.as_deref()) {
        if offline {
            let rows = storage.list_space_root_pages_for_site(site_id, sp)?;
            slice_page(rows, offline_offset, limit)
        } else {
            let mut fetched = client
                .list_space_root_pages_page(sp, limit, clean_cursor)
                .await?;
            let _ = storage.record_root_pages_page_for_site(site_id, sp, &fetched, first_page);
            let _ = storage.overlay_children_state_for_site(site_id, &mut fetched.items);
            fetched
        }
    } else if offline {
        let rows = storage.list_recent_pages_for_site(
            site_id,
            offline_offset.saturating_add(limit).saturating_add(1),
        )?;
        slice_page(rows, offline_offset, limit)
    } else {
        bail!("Specify a space key or `--parent <PAGE_ID>`.");
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "pages": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
            }))?
        );
    } else if page.items.is_empty() {
        println!("No pages found.");
    } else {
        for p in &page.items {
            let children_mark = if p.has_children == Some(true) {
                "▸"
            } else {
                "·"
            };
            println!(
                "{children_mark} {:<12} [{}] v{} {}",
                p.id, p.space_key, p.version_number, p.title
            );
        }
        if let Some(next) = page.next_cursor.as_deref() {
            eprintln!("Next page: --cursor {next}");
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
    compact: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, !offline)?;
    let site_id = client.site_id();
    let (detail, detail_stale) = if offline {
        let d = storage
            .get_page_detail_for_site(site_id, page_id)?
            .with_context(|| format!("Page '{page_id}' not found in offline cache."))?;
        let stale = storage
            .is_page_detail_stale_for_site(site_id, page_id)
            .unwrap_or(false);
        (d, stale)
    } else {
        let (mut fetched, meta) = match client
            .get_page_with_meta_in_cache(page_id, site_id, &storage)
            .await
        {
            Ok(res) => res,
            Err(err) => {
                if is_not_found_error(&err) {
                    let _ = storage.delete_page_for_site(site_id, page_id);
                }
                return Err(err);
            }
        };
        let _ = storage.upsert_page_detail_for_site_with_meta(site_id, &mut fetched, meta);
        (fetched, false)
    };

    if json_out {
        let body = if compact {
            let mut hint = format!("atlassian wiki view {page_id} --json --full");
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
                "page": body,
            }))?
        );
        return Ok(());
    }

    print_page_detail(&detail);
    if detail_stale {
        println!("\n[Note] Cached page body is from an earlier version. Fetch online to refresh.");
    }

    if preview_images && !offline {
        render_attachments_preview(&client, &detail.attachments, None, None).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_page_create(
    cfg: &AppConfig,
    space: Option<&str>,
    parent: Option<&str>,
    title: &str,
    body: Option<&str>,
    body_file: Option<&Path>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let space_id_or_key = space
        .or(cfg.default_space_key.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Space key or ID is required (`--space <KEY_OR_ID>`).")?;
    let body_markdown = read_body_arg(body, body_file)?.unwrap_or_default();

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let req = OutgoingPage {
        space_id_or_key: space_id_or_key.to_string(),
        parent_id: parent.map(str::to_string),
        title: title.to_string(),
        body_markdown,
    };
    let created = client.create_page(&req).await?;
    let _ = storage.upsert_page_detail_for_site(site_id, &created);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "site_id": site_id,
                "page": if full { created.output(false) } else { created.write_output() },
            }))?
        );
    } else {
        println!(
            "Page created: [{}] {} ({})",
            created.summary.id, created.summary.title, created.summary.web_url
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_confluence_workflow(cfg: &AppConfig, command: ConfluenceWorkflow) -> Result<()> {
    let (_, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    let (result, json_out, message) = match command {
        ConfluenceWorkflow::Move {
            page_id,
            parent_id,
            prev_version,
            json,
        } => {
            let page = client.move_page(&page_id, &parent_id, prev_version).await?;
            refresh_cli_page_cache(&client, &storage, &page_id).await;
            (
                json!({"ok": true, "site_id": site_id, "page": page}),
                json,
                format!("Page {page_id}: moved under parent {parent_id}"),
            )
        }
        ConfluenceWorkflow::Labels {
            page_id,
            add,
            remove,
            json,
        } => {
            let result = if let Some(label) = remove {
                client.remove_page_label(&page_id, &label).await?;
                json!({"ok": true, "site_id": site_id, "page_id": page_id, "removed": label.trim()})
            } else {
                let added = client.add_page_labels(&page_id, &add).await?;
                json!({"ok": true, "site_id": site_id, "page_id": page_id, "added": added})
            };
            refresh_cli_page_cache(&client, &storage, &page_id).await;
            (result, json, format!("Page {page_id}: labels updated"))
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
async fn cmd_page_edit(
    cfg: &AppConfig,
    page_id: &str,
    title: Option<String>,
    body: PageBodyArgs,
    prev_version: Option<i32>,
    message: Option<String>,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let dry_run = body.body.dry_run;
    let (full_body, append, replacement) = body.body.read()?;
    let section = body.replace_section.map(|values| {
        let mut values = values.into_iter();
        SectionReplacement {
            heading: values.next().expect("clap requires HEADING"),
            markdown: values.next().expect("clap requires MARKDOWN"),
        }
    });
    let edit = BodyEdit::from_inputs(
        full_body.as_deref(),
        append.as_deref(),
        replacement.as_ref(),
        section.as_ref(),
    )?;
    let (_auth, client) = open_client(cfg, true)?;
    let site_id = client.site_id();
    if dry_run {
        let preview = client
            .preview_page_edit(page_id, title, edit, prev_version, message)
            .await?;
        return print_edit_preview(site_id, preview);
    }
    let storage = Storage::open(&config::cache_db_path(&cfg.profile))?;
    let updated = client
        .edit_page(page_id, title, edit, prev_version, message)
        .await?;
    let _ = storage.upsert_page_detail_for_site(site_id, &updated);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "site_id": site_id,
                "page": if full { updated.output(false) } else { updated.write_output() },
            }))?
        );
    } else {
        println!(
            "Page updated: [{}] {} (v{})",
            updated.summary.id, updated.summary.title, updated.summary.version_number
        );
    }
    Ok(())
}

fn print_edit_preview(site_id: &str, preview: Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({ "ok": true, "site_id": site_id, "dry_run": true, "preview": preview })
        )?
    );
    Ok(())
}

async fn cmd_page_comment(
    cfg: &AppConfig,
    page_id: &str,
    body: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let comment = client.add_page_comment(page_id, body).await?;
    refresh_cli_page_cache(&client, &storage, page_id).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "comment": comment }))?
        );
    } else {
        println!("Page {page_id}: comment added (id={}).", comment.id);
    }
    Ok(())
}

async fn cmd_page_attach(
    cfg: &AppConfig,
    page_id: &str,
    file: &Path,
    json_out: bool,
) -> Result<()> {
    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let uploaded = client.upload_page_attachment(page_id, file).await?;
    refresh_cli_page_cache(&client, &storage, page_id).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "page_id": page_id, "attachments": uploaded }))?
        );
    } else {
        println!(
            "Page {page_id}: attached '{}' ({} file(s)).",
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
        .with_context(|| format!("No attachment found to download on page {page_id}."))?;
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

async fn cmd_page_delete(cfg: &AppConfig, page_id: &str, yes: bool, json_out: bool) -> Result<()> {
    let answer = if !yes && !json_out {
        Some(prompt_line(&format!(
            "Delete Confluence page '{page_id}'? [y/N]: "
        ))?)
    } else {
        None
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let (_auth, client, storage) = open_runtime(cfg, true)?;
    let site_id = client.site_id();
    client.delete_page(page_id).await?;
    let _ = storage.delete_page_for_site(site_id, page_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "site_id": site_id, "page_id": page_id }))?
        );
    } else {
        println!("Deleted Confluence page '{page_id}'.");
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
    let site_id = client.site_id();

    if use_remote {
        let effective_cql = if cql || serve::looks_like_cql_expr(query) {
            query.to_string()
        } else {
            format!("text ~ \"{}\"", serve::escape_jql_str(query))
        };
        let pages = client.search_confluence_cql(&effective_cql, limit).await?;
        let _ = storage.upsert_pages_for_site(site_id, &pages);

        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "remote": true,
                    "site_id": site_id,
                    "cql": effective_cql,
                    "pages": pages,
                }))?
            );
        } else if pages.is_empty() {
            println!("No results found.");
        } else {
            for p in &pages {
                println!("{:<12} [{}] {}", p.id, p.space_key, p.title);
            }
        }
    } else {
        let scope_site = if cfg.active_site.is_some() || cfg.effective_sites().len() <= 1 {
            Some(site_id)
        } else {
            None
        };
        let hits: Vec<_> = storage
            .search_pages_scoped(query, scope_site, limit)?
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
            println!("No results in local cache. (Use `--remote` to search Confluence server)");
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
    let (auth, client, storage) = open_runtime(cfg, false)?;
    let site_id = client.site_id();
    let url = if let Ok(Some(detail)) = storage.get_page_detail_for_site(site_id, page_id) {
        detail.summary.web_url
    } else if auth.is_logged_in() {
        client.get_page(page_id).await?.summary.web_url
    } else {
        let root = cfg.effective_confluence_root();
        if root.is_empty() {
            bail!("Cannot build web URL. Run `confluence login` first.");
        }
        format!("{root}/pages/{page_id}")
    };
    open::that(&url).with_context(|| format!("Failed to open browser: {url}"))?;
    println!("Opened in browser: {url}");
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

fn cmd_preview_file(path: &Path, cols: Option<u16>, rows: Option<u16>) -> Result<()> {
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

async fn cmd_sync(cfg: &AppConfig, site_filter: Option<&str>, json_out: bool) -> Result<()> {
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
        bail!("Standard input is closed.");
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
        "Status: [{}] ({}) · Type: {} · Priority: {}",
        s.status.name,
        s.status.category.badge_label(),
        s.issue_type.name,
        s.priority.as_ref().map(|p| p.name.as_str()).unwrap_or("-")
    );
    println!(
        "Assignee: {} · Reporter: {} · Sprint: {}",
        s.assignee
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("Unassigned"),
        s.reporter
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("-"),
        s.sprint_name.as_deref().unwrap_or("-")
    );
    if !s.labels.is_empty() {
        println!("Labels: {}", s.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }

    if !detail.description_markdown.trim().is_empty() {
        println!("\n--- Description ---\n{}", detail.description_markdown.trim());
    }
    if !detail.subtasks.is_empty() {
        println!("\n--- Subtasks ({}) ---", detail.subtasks.len());
        for sub in &detail.subtasks {
            println!("  {} [{}] {}", sub.key, sub.status.name, sub.summary);
        }
    }
    if !detail.linked_pages.is_empty() {
        println!("\n--- Linked Confluence Pages ({}) ---", detail.linked_pages.len());
        for page in &detail.linked_pages {
            println!("  [{}] {} ({})", page.page_id, page.title, page.web_url);
        }
    }
    if !detail.attachments.is_empty() {
        println!("\n--- Attachments ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- Comments ({}) ---", detail.comments.len());
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
        "Space: {} · Author: {} · Updated: {}",
        s.space_key,
        s.author_name.as_deref().unwrap_or("-"),
        s.updated_at
    );
    if !detail.ancestors.is_empty() {
        let trail: Vec<&str> = detail.ancestors.iter().map(|a| a.title.as_str()).collect();
        println!("Path: {}", trail.join(" > "));
    }
    if !detail.labels.is_empty() {
        println!("Labels: {}", detail.labels.join(", "));
    }
    if !s.web_url.is_empty() {
        println!("URL: {}", s.web_url);
    }
    if !detail.body_markdown.trim().is_empty() {
        println!("\n--- Body ---\n{}", detail.body_markdown.trim());
    }
    if !detail.linked_issue_keys.is_empty() {
        println!("\n--- Linked Jira Issues ---\n  {}", detail.linked_issue_keys.join(", "));
    }
    if !detail.attachments.is_empty() {
        println!("\n--- Attachments ({}) ---", detail.attachments.len());
        for att in &detail.attachments {
            println!("  [{}] {} ({}, {}B)", att.id, att.filename, att.mime_type, att.size);
        }
    }
    if !detail.comments.is_empty() {
        println!("\n--- Comments ({}) ---", detail.comments.len());
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
                        println!("\n[Image preview: {}]", att.filename);
                        println!("{rendered}");
                    }
                    Err(err) => {
                        eprintln!("Skipped image rendering ({}): {err:#}", att.filename);
                    }
                }
            }
            Err(err) => {
                eprintln!("Skipped image download ({}): {err:#}", att.filename);
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
                let txt = if i == 0 { "(no issues)" } else { "" };
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
            Some(Command::Login {
                confluence_pat,
                confluence_account,
                pat,
                ..
            }) => {
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
            Some(ConfluenceCommand::Login { confluence_pat: Some(_), .. })
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
            Some(Command::Serve { cors_origin, token, .. }) => {
                assert_eq!(cors_origin, vec!["http://localhost:5173", "http://localhost:3000"]);
                assert_eq!(token, None);
            }
            other => panic!("expected serve, got {other:?}"),
        }
        let plain = Cli::try_parse_from(["atlassian", "serve"]).unwrap();
        assert!(matches!(
            plain.command,
            Some(Command::Serve { cors_origin, .. }) if cors_origin.is_empty()
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
            Command::Serve {
                bind, port, token, ..
            } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 9000);
                assert_eq!(token.as_deref(), Some("sec"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let cli = Cli::try_parse_from(["jira", "mcp-server"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Mcp { read_only: false }
        ));

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

        // Multi-site global --site option, OAuth / Cloud Account login, and Sites subcommands
        let cli = Cli::try_parse_from(["jira", "--site", "acme-cloud", "issues", "--mine"]).unwrap();
        assert_eq!(cli.site.as_deref(), Some("acme-cloud"));
        assert!(matches!(cli.command.unwrap(), Command::Issues { mine: true, .. }));

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
            Command::Login {
                oauth: true,
                cloud_account: false,
                client_id: Some(_),
                client_secret: Some(_),
                account: Some(_),
                json: true,
                ..
            }
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
            Command::Login {
                oauth: false,
                cloud_account: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["jira", "sites", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sites { action } if matches!(*action, SitesCommand::List { json: true })
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
            Command::Sites { action } => match *action {
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
            Command::Sites { action } if matches!(*action, SitesCommand::Discover { json: true, .. })
        ));

        let cli = Cli::try_parse_from(["jira", "sites", "default", "acme"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sites { action } if matches!(*action, SitesCommand::Default { .. })
        ));

        let cli = Cli::try_parse_from(["jira", "sites", "enable", "acme"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sites { action } if matches!(*action, SitesCommand::Enable { .. })
        ));

        let cli = Cli::try_parse_from(["jira", "sites", "disable", "acme"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sites { action } if matches!(*action, SitesCommand::Disable { .. })
        ));

        let cli = Cli::try_parse_from(["jira", "sites", "remove", "acme", "--yes"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sites { action } if matches!(*action, SitesCommand::Remove { yes: true, .. })
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
            ConfluenceCommand::Sites { action } if matches!(*action, SitesCommand::List { json: true })
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
            Command::Edit { key, body, json, .. } => {
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
            Command::Edit { body, json, .. } => {
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
            Command::Edit { body, .. } => {
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
            Command::Edit { body, .. } => {
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
            Command::Edit { body, json, .. } => {
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
            Command::Create { summary, body, json, .. } => {
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
            Command::Comment { key, body, json } => {
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
            Command::Workflow(JiraWorkflow::LogWork { seconds, comment, json, .. }) => {
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
            Command::Transition { comment, json, .. } => {
                assert_eq!(comment.as_deref(), Some(bullet));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "edit", "FLS-14", "--summary", bullet])
                .unwrap();
        match cli.command.unwrap() {
            Command::Edit { summary, .. } => assert_eq!(summary.as_deref(), Some(bullet)),
            other => panic!("unexpected: {other:?}"),
        }

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "search", bullet, "--remote", "--json"])
                .unwrap();
        match cli.command.unwrap() {
            Command::Search { query, remote, json, .. } => {
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
            ConfluenceCommand::Edit { title, body, .. } => {
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
            ConfluenceCommand::Comment { page_id, body, json } => {
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
            ConfluenceCommand::Search { query, remote, json, .. } => {
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
            Command::Edit { body, json, .. } => {
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
            Command::Edit { body, .. } => assert_eq!(body.append.as_deref(), Some("--help")),
            other => panic!("unexpected: {other:?}"),
        }

        let cli: Cli = try_parse_cli_from(
            "atlassian",
            ["atlassian", "comment", "FLS-14", "--", "--json"],
        )
        .unwrap();
        match cli.command.unwrap() {
            Command::Comment { body, json, .. } => {
                assert_eq!(body, "--json");
                assert!(!json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "search", "--", "--help"]).unwrap();
        match cli.command.unwrap() {
            Command::Search { query, .. } => assert_eq!(query, "--help"),
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
            Command::Mcp { read_only: true }
        ));
        let conf: ConfluenceCli =
            try_parse_cli_from("confluence", ["confluence", "mcp", "--read-only"]).unwrap();
        assert!(matches!(
            conf.command.unwrap(),
            ConfluenceCommand::Mcp { read_only: true }
        ));
    }

    #[test]
    fn parses_smart_view_shortcuts_short_flags_and_aliases() {
        // 1. Smart Jira issue key shortcut (`atlassian ENG-42 -jf`, with global `--site`)
        let cli: Cli = try_parse_cli_from("atlassian", ["atlassian", "ENG-42", "-jf"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::View {
                ref key,
                json: true,
                full: true,
                offline: false,
                ..
            } if key == "ENG-42"
        ));

        let cli: Cli = try_parse_cli_from(
            "atlassian",
            ["atlassian", "--site", "acme", "PLAT_2-109", "-oj"],
        )
        .unwrap();
        assert_eq!(cli.site.as_deref(), Some("acme"));
        assert!(matches!(
            cli.command.unwrap(),
            Command::View {
                ref key,
                offline: true,
                json: true,
                ..
            } if key == "PLAT_2-109"
        ));

        // 2. Smart Confluence page ID shortcut (`conf 40211 -j`, `atlassian wiki 40211 -j`, `conf jira ENG-42 -j`)
        let conf: ConfluenceCli = try_parse_cli_from("conf", ["conf", "40211", "-jf"]).unwrap();
        assert!(matches!(
            conf.command.unwrap(),
            ConfluenceCommand::View {
                ref page_id,
                json: true,
                full: true,
                ..
            } if page_id == "40211"
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
            Command::Issues {
                mine: true,
                json: true,
                limit: 15,
                ref project,
                ..
            } if project.as_deref() == Some("ENG")
        ));

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "q", "canary", "-rj", "-n", "10"])
                .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Search {
                ref query,
                remote: true,
                json: true,
                limit: 10,
                ..
            } if query == "canary"
        ));

        let cli: Cli = try_parse_cli_from(
            "atlassian",
            ["atlassian", "mv", "ENG-42", "In Review", "-c", "LGTM", "-j"],
        )
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Transition {
                ref key,
                ref status,
                ref comment,
                json: true,
                ..
            } if key == "ENG-42"
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
            Command::Create {
                ref summary_pos,
                summary: None,
                ref project,
                ref assignee,
                json: true,
                ..
            } if summary_pos.as_deref() == Some("Fix login")
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
            ConfluenceCommand::Create {
                ref title_pos,
                title: None,
                ref space,
                json: true,
                ..
            } if title_pos.as_deref() == Some("Release Notes") && space.as_deref() == Some("ENG")
        ));

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "as", "ENG-42", "me", "-j"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Assign {
                ref key,
                ref assignee_pos,
                to: None,
                json: true,
                ..
            } if key == "ENG-42" && assignee_pos.as_deref() == Some("me")
        ));

        // 5. Alias & Init subcommands
        let cli: Cli = try_parse_cli_from(
            "atlassian",
            ["atlassian", "alias", "install", "--shell", "powershell", "--dry-run", "-j"],
        )
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Alias {
                action: AliasCommand::Install {
                    shell: Some(ShellKind::Powershell),
                    dry_run: true,
                    json: true,
                    ref jira,
                    ref conf,
                    ..
                }
            } if jira == "j" && conf == "cf"
        ));

        let cli: Cli =
            try_parse_cli_from("atlassian", ["atlassian", "init", "zsh"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Init {
                shell: ShellKind::Zsh,
                ref jira,
                ref conf,
                force: false,
            } if jira == "j" && conf == "cf"
        ));

        // 6. JSON detection helper covers `--json`, `-j`, and combined short flags
        let to_os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(argv_requests_json(&to_os(&["atlassian", "ENG-42", "--json"])));
        assert!(argv_requests_json(&to_os(&["atlassian", "ENG-42", "-j"])));
        assert!(argv_requests_json(&to_os(&["atlassian", "q", "bug", "-rj"])));
        assert!(argv_requests_json(&to_os(&["atlassian", "ls", "-mj"])));
        assert!(!argv_requests_json(&to_os(&["atlassian", "ENG-42"])));
        assert!(!argv_requests_json(&to_os(&["atlassian", "comment", "ENG-42", "--", "-j"])));
    }
}
