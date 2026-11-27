//! Clap argument and subcommand definitions for dual-persona (`atlassian` / `conf`) CLI.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    atlassian::body::TextReplacement,
    config::{DeploymentKind, DEFAULT_SERVE_PORT},
    shell_alias::{AliasCommand, ShellKind, DEFAULT_CONF_ALIAS, DEFAULT_JIRA_ALIAS},
};

use super::format::read_body_arg;

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
    pub(super) body: Option<String>,
    /// Replace the entire body from a Markdown file
    #[arg(long = "body-file")]
    pub(super) body_file: Option<PathBuf>,
    /// Append Markdown while preserving the existing body
    #[arg(
        long,
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append_file"]
    )]
    pub(super) append: Option<String>,
    /// Append a Markdown file while preserving the existing body
    #[arg(long = "append-file", conflicts_with_all = ["body", "body_file"])]
    pub(super) append_file: Option<PathBuf>,
    /// Replace one unique text occurrence in place
    #[arg(
        long,
        num_args = 2,
        value_names = ["OLD", "NEW"],
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append", "append_file"]
    )]
    pub(super) replace: Option<Vec<String>>,
    /// Preview the computed changes without saving (always prints JSON)
    #[arg(long)]
    pub(super) dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct PageBodyArgs {
    #[command(flatten)]
    pub(super) body: EditBodyArgs,
    /// Replace the section under an exact top-level or layout-column heading on a Confluence Cloud ADF page
    #[arg(
        long,
        num_args = 2,
        value_names = ["HEADING", "MARKDOWN"],
        allow_hyphen_values = true,
        conflicts_with_all = ["body", "body_file", "append", "append_file", "replace"]
    )]
    pub(super) replace_section: Option<Vec<String>>,
}

impl EditBodyArgs {
    pub(super) fn read(self) -> Result<(Option<String>, Option<String>, Option<TextReplacement>)> {
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


/// Commands shared identically across both `atlassian` (Jira-first) and `conf` (Confluence-first) personas.
#[derive(Subcommand, Debug)]
pub enum SharedCommand {
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
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    #[command(flatten)]
    Shared(SharedCommand),
    #[command(flatten)]
    Jira(JiraSubcommand),
    /// Confluence command namespace (`atlassian wiki <subcmd>`)
    #[command(visible_alias = "conf")]
    Wiki {
        #[command(subcommand)]
        command: WikiCommand,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ConfluenceCommand {
    #[command(flatten)]
    Shared(SharedCommand),
    #[command(flatten)]
    Wiki(WikiCommand),
    /// Jira command namespace (`confluence jira <subcmd>`)
    Jira {
        #[command(subcommand)]
        command: JiraSubcommand,
    },
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
        /// Comma-separated omitted standard fields, customfield_<id>, or custom field display names
        #[arg(long = "fields", value_delimiter = ',', requires = "json", conflicts_with = "full")]
        fields: Vec<String>,
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

