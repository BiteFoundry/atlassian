//! Lightweight single-instance background sync daemon for Atlassian (`atlassian daemon` / `confluence daemon`).
//!
//! Manages PID/lockfile coordination, authenticated local IPC (Windows named pipe / Unix domain socket /
//! loopback TCP fallback), broadcast `CoreEvent` streaming to attached TUI clients, and `SyncCommand` forwarding.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
#[cfg(any(unix, windows))]
use sha2::{Digest, Sha256};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader, ReadBuf,
    },
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, watch},
};

use crate::{
    atlassian::{AtlassianClient, SiteClientPool},
    auth::AuthManager,
    config::AppConfig,
    models::{
        BoardSummary, CoreEvent, IssueSummary, PageSummary, ProjectSummary, SpaceSummary,
        SprintSummary,
    },
    storage::Storage,
    sync::{self, SyncCommand, SyncController, POLL_RETRY_SUMMARY},
};

const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const TUI_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const FAST_TCP_PROBE_TIMEOUT: Duration = Duration::from_millis(25);
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_IPC_FRAME_BYTES: usize = 4 * 1024 * 1024;
const STARTUP_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(60);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_millis(1500);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(30);
const LOCK_STALE_AGE: Duration = Duration::from_secs(5);
const BROADCAST_CAPACITY: usize = 256;

/// Per-site synchronization status reported inside `DaemonStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DaemonSiteStatus {
    pub site_id: String,
    pub name: String,
    pub site_url: String,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub circuit_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub transport: Option<String>,
    pub endpoint: Option<String>,
    pub profile: Option<String>,
    pub site_url: Option<String>,
    pub uptime_secs: Option<u64>,
    pub attached_clients: usize,
    pub sync_busy: bool,
    pub sync_summary: String,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub sites: Vec<DaemonSiteStatus>,
}

/// Handle returned when a TUI client attaches to a live background sync daemon.
pub struct TuiDaemonConnection {
    pub sync_ctrl: SyncController,
    pub events_rx: mpsc::UnboundedReceiver<CoreEvent>,
    pub status: DaemonStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DaemonRuntimeInfo {
    pid: u32,
    version: String,
    transport: String,
    endpoint: String,
    secret: String,
    profile: String,
    site_url: String,
    started_at_unix: u64,
    started_at: String,
}

#[derive(Debug, Clone)]
struct DaemonPaths {
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    data_dir: PathBuf,
    state_file: PathBuf,
    lock_file: PathBuf,
}

impl DaemonPaths {
    fn from_data_dir(dir: &Path) -> Self {
        Self {
            data_dir: dir.to_path_buf(),
            state_file: dir.join("atlassian-daemon.json"),
            lock_file: dir.join("atlassian-daemon.lock"),
        }
    }

    fn for_config(cfg: &AppConfig) -> Self {
        let profile = if cfg.profile.trim().is_empty() {
            "default"
        } else {
            cfg.profile.trim()
        };
        Self::from_data_dir(&crate::config::data_dir(profile))
    }
}

/// Serde-serializable wire representation of `CoreEvent` so IPC framing requires no edits to `models.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
enum WireCoreEvent {
    SyncStarted,
    SyncFinished {
        summary: String,
    },
    ProjectsUpdated(WireSiteList<ProjectSummary>),
    BoardsUpdated(WireSiteList<BoardSummary>),
    BoardIssuesUpdated {
        #[serde(default)]
        site_id: String,
        board_id: i64,
        sprint: Option<SprintSummary>,
        issues: Vec<IssueSummary>,
    },
    IssuesUpdated {
        #[serde(default)]
        site_id: String,
        issues: Vec<IssueSummary>,
    },
    SpacesUpdated(WireSiteList<SpaceSummary>),
    SpacePagesUpdated {
        #[serde(default)]
        site_id: String,
        space_key: String,
        pages: Vec<PageSummary>,
    },
    OpenIssueRevision {
        #[serde(default)]
        site_id: String,
        summary: Box<IssueSummary>,
    },
    OpenPageRevision {
        #[serde(default)]
        site_id: String,
        summary: PageSummary,
    },
    SyncError(String),
}

/// Site-scoped list payload of `projects_updated` / `boards_updated` / `spaces_updated`.
///
/// Written as `{"site_id": .., "items": [..]}`. A daemon older than 0.8.1 wrote a bare array;
/// that shape still parses, with an empty site id that the TUI resolves to its effective site.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum WireSiteList<T> {
    Scoped {
        #[serde(default)]
        site_id: String,
        items: Vec<T>,
    },
    Legacy(Vec<T>),
}

impl<T> WireSiteList<T> {
    fn scoped(site_id: String, items: Vec<T>) -> Self {
        Self::Scoped { site_id, items }
    }

    fn into_parts(self) -> (String, Vec<T>) {
        match self {
            Self::Scoped { site_id, items } => (site_id, items),
            Self::Legacy(items) => (String::new(), items),
        }
    }
}

impl From<CoreEvent> for WireCoreEvent {
    fn from(ev: CoreEvent) -> Self {
        match ev {
            CoreEvent::SyncStarted => Self::SyncStarted,
            CoreEvent::SyncFinished { summary } => Self::SyncFinished { summary },
            CoreEvent::ProjectsUpdated { site_id, items } => {
                Self::ProjectsUpdated(WireSiteList::scoped(site_id, items))
            }
            CoreEvent::BoardsUpdated { site_id, items } => {
                Self::BoardsUpdated(WireSiteList::scoped(site_id, items))
            }
            CoreEvent::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            } => Self::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            },
            CoreEvent::IssuesUpdated { site_id, issues } => {
                Self::IssuesUpdated { site_id, issues }
            }
            CoreEvent::SpacesUpdated { site_id, items } => {
                Self::SpacesUpdated(WireSiteList::scoped(site_id, items))
            }
            CoreEvent::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            } => Self::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            },
            CoreEvent::OpenIssueRevision { site_id, summary } => {
                Self::OpenIssueRevision { site_id, summary }
            }
            CoreEvent::OpenPageRevision { site_id, summary } => {
                Self::OpenPageRevision { site_id, summary }
            }
            CoreEvent::SyncError(err) => Self::SyncError(err),
        }
    }
}

impl From<WireCoreEvent> for CoreEvent {
    fn from(wire: WireCoreEvent) -> Self {
        match wire {
            WireCoreEvent::SyncStarted => Self::SyncStarted,
            WireCoreEvent::SyncFinished { summary } => Self::SyncFinished { summary },
            WireCoreEvent::ProjectsUpdated(v) => {
                let (site_id, items) = v.into_parts();
                Self::ProjectsUpdated { site_id, items }
            }
            WireCoreEvent::BoardsUpdated(v) => {
                let (site_id, items) = v.into_parts();
                Self::BoardsUpdated { site_id, items }
            }
            WireCoreEvent::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            } => Self::BoardIssuesUpdated {
                site_id,
                board_id,
                sprint,
                issues,
            },
            WireCoreEvent::IssuesUpdated { site_id, issues } => {
                Self::IssuesUpdated { site_id, issues }
            }
            WireCoreEvent::SpacesUpdated(v) => {
                let (site_id, items) = v.into_parts();
                Self::SpacesUpdated { site_id, items }
            }
            WireCoreEvent::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            } => Self::SpacePagesUpdated {
                site_id,
                space_key,
                pages,
            },
            WireCoreEvent::OpenIssueRevision { site_id, summary } => {
                Self::OpenIssueRevision { site_id, summary }
            }
            WireCoreEvent::OpenPageRevision { site_id, summary } => {
                Self::OpenPageRevision { site_id, summary }
            }
            WireCoreEvent::SyncError(err) => Self::SyncError(err),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClientFrame {
    secret: String,
    op: ClientOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientOp {
    Status,
    Nudge { cmd: Option<SyncCommand> },
    Shutdown,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ClientStreamCmd {
    Command(SyncCommand),
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ServerFrame {
    Status(DaemonStatus),
    Ack { ok: bool, error: Option<String> },
    Subscribed(DaemonStatus),
    Event(Box<WireCoreEvent>),
    Pong,
    Error { message: String },
}

#[derive(Debug, Default)]
struct SharedSyncStatus {
    sync_busy: bool,
    sync_summary: String,
    last_sync_at: Option<String>,
    last_error: Option<String>,
    sites: Vec<DaemonSiteStatus>,
}

struct DaemonSharedState {
    pid: u32,
    version: String,
    transport: String,
    endpoint: String,
    profile: String,
    site_url: String,
    secret: String,
    started_instant: Instant,
    attached_clients: Arc<AtomicUsize>,
    sync_status: Mutex<SharedSyncStatus>,
}

impl DaemonSharedState {
    fn new(info: &DaemonRuntimeInfo, started_instant: Instant) -> Self {
        Self::with_sites(info, started_instant, Vec::new())
    }

    fn with_sites(
        info: &DaemonRuntimeInfo,
        started_instant: Instant,
        initial_sites: Vec<DaemonSiteStatus>,
    ) -> Self {
        Self {
            pid: info.pid,
            version: info.version.clone(),
            transport: info.transport.clone(),
            endpoint: info.endpoint.clone(),
            profile: info.profile.clone(),
            site_url: info.site_url.clone(),
            secret: info.secret.clone(),
            started_instant,
            attached_clients: Arc::new(AtomicUsize::new(0)),
            sync_status: Mutex::new(SharedSyncStatus {
                sites: initial_sites,
                ..Default::default()
            }),
        }
    }

    fn snapshot_status(&self) -> DaemonStatus {
        let guard = self
            .sync_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        DaemonStatus {
            running: true,
            pid: Some(self.pid),
            version: Some(self.version.clone()),
            transport: Some(self.transport.clone()),
            endpoint: Some(self.endpoint.clone()),
            profile: Some(self.profile.clone()),
            site_url: if self.site_url.is_empty() {
                None
            } else {
                Some(self.site_url.clone())
            },
            uptime_secs: Some(self.started_instant.elapsed().as_secs()),
            attached_clients: self.attached_clients.load(Ordering::SeqCst),
            sync_busy: guard.sync_busy,
            sync_summary: guard.sync_summary.clone(),
            last_sync_at: guard.last_sync_at.clone(),
            last_error: guard.last_error.clone(),
            sites: guard.sites.clone(),
        }
    }

    fn apply_site_results(&self, results: &[sync::SiteSyncResult]) {
        let mut guard = self
            .sync_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        for res in results {
            if let Some(existing) = guard
                .sites
                .iter_mut()
                .find(|s| s.site_id.eq_ignore_ascii_case(&res.site_id))
            {
                if !res.site_name.trim().is_empty() {
                    existing.name = res.site_name.clone();
                }
                if !res.site_url.trim().is_empty() {
                    existing.site_url = res.site_url.clone();
                }
                if res.succeeded {
                    existing.last_sync_at = Some(now_rfc3339.clone());
                    existing.last_error = None;
                } else if let Some(ref err) = res.error {
                    existing.last_error = Some(err.clone());
                }
                existing.circuit_open = res.circuit_open;
            } else {
                guard.sites.push(DaemonSiteStatus {
                    site_id: res.site_id.clone(),
                    name: res.site_name.clone(),
                    site_url: res.site_url.clone(),
                    last_sync_at: if res.succeeded {
                        Some(now_rfc3339.clone())
                    } else {
                        None
                    },
                    last_error: if res.succeeded {
                        None
                    } else {
                        res.error.clone()
                    },
                    circuit_open: res.circuit_open,
                });
            }
        }
    }

    fn apply_event(&self, ev: &CoreEvent) {
        let mut guard = self
            .sync_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match ev {
            CoreEvent::SyncStarted => {
                guard.sync_busy = true;
            }
            CoreEvent::SyncFinished { summary } => {
                guard.sync_busy = false;
                guard.sync_summary = summary.clone();
                if summary != POLL_RETRY_SUMMARY {
                    guard.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
                    guard.last_error = None;
                }
            }
            CoreEvent::ProjectsUpdated { .. }
            | CoreEvent::BoardsUpdated { .. }
            | CoreEvent::BoardIssuesUpdated { .. }
            | CoreEvent::IssuesUpdated { .. }
            | CoreEvent::SpacesUpdated { .. }
            | CoreEvent::SpacePagesUpdated { .. }
            | CoreEvent::OpenIssueRevision { .. }
            | CoreEvent::OpenPageRevision { .. } => {
                guard.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
                guard.last_error = None;
            }
            CoreEvent::SyncError(err) => {
                guard.sync_busy = false;
                guard.last_error = Some(err.clone());
            }
        }
    }
}

struct AttachedClientGuard {
    counter: Arc<AtomicUsize>,
}

impl AttachedClientGuard {
    fn acquire(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for AttachedClientGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

struct StartupLockGuard {
    path: PathBuf,
}

impl StartupLockGuard {
    async fn acquire(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let unavailable = |err: &io::Error| {
            anyhow!(
                "Could not acquire daemon startup lock {}: another process holds it or it cannot be removed ({err}). Delete the file manually or retry.",
                path.display()
            )
        };
        let mut removed_after_deadline = false;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if removed_after_deadline {
                        return Err(unavailable(&err));
                    }
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(modified) = meta.modified() {
                            // A stale lock that cannot be removed falls through to the sleep below.
                            if SystemTime::now()
                                .duration_since(modified)
                                .unwrap_or_default()
                                >= LOCK_STALE_AGE
                                && fs::remove_file(path).is_ok()
                            {
                                continue;
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        // One removal and one more `create_new`; never retry without yielding.
                        match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                            Err(err) => return Err(unavailable(&err)),
                        }
                        removed_after_deadline = true;
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to create daemon startup lock file: {}",
                            path.display()
                        )
                    });
                }
            }
        }
    }
}

impl Drop for StartupLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

enum IpcListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        server: tokio::net::windows::named_pipe::NamedPipeServer,
    },
}

impl IpcListener {
    async fn accept(&mut self) -> io::Result<IpcStream> {
        match self {
            IpcListener::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                let _ = stream.set_nodelay(true);
                Ok(IpcStream::Tcp(stream))
            }
            #[cfg(unix)]
            IpcListener::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(IpcStream::Unix(stream))
            }
            #[cfg(windows)]
            IpcListener::Pipe { name, server } => {
                server.connect().await?;
                let next_server =
                    tokio::net::windows::named_pipe::ServerOptions::new().create(name.as_str())?;
                let connected = std::mem::replace(server, next_server);
                Ok(IpcStream::PipeServer(connected))
            }
        }
    }
}

enum IpcStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            IpcStream::PipeServer(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            IpcStream::PipeClient(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStream::PipeServer(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStream::PipeClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStream::PipeServer(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStream::PipeClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStream::PipeServer(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStream::PipeClient(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    crate::encode_hex(bytes)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_profile_compatible(expected_profile: &str, actual_profile: &str) -> bool {
    let expected = expected_profile.trim();
    let actual = actual_profile.trim();
    if expected.is_empty() || actual.is_empty() {
        return true;
    }
    expected == actual
}

fn write_secret_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    ensure_parent(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Invalid state file path: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("atlassian-daemon.json");
    let tmp = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("Failed to create temporary state file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temporary state file: {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary state file: {}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!("Failed to set permissions on state file: {}", tmp.display())
        })?;
    }

    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("Failed to create temporary state file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temporary state file: {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary state file: {}", tmp.display()))?;
    }

    if let Err(err) = fs::rename(&tmp, path) {
        if path.exists() {
            fs::remove_file(path).with_context(|| {
                format!("Failed to replace existing state file: {}", path.display())
            })?;
            fs::rename(&tmp, path).with_context(|| {
                format!("Failed to atomically save state file: {}", path.display())
            })?;
        } else {
            let _ = fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("Failed to save state file: {}", path.display()));
        }
    }
    Ok(())
}

fn read_runtime_info_or_clean(paths: &DaemonPaths) -> Option<DaemonRuntimeInfo> {
    let raw = fs::read(&paths.state_file).ok()?;
    match serde_json::from_slice::<DaemonRuntimeInfo>(&raw) {
        Ok(info) => Some(info),
        Err(_) => {
            let _ = fs::remove_file(&paths.state_file);
            None
        }
    }
}

fn cleanup_stale_files(paths: &DaemonPaths, info: Option<&DaemonRuntimeInfo>) {
    let _ = fs::remove_file(&paths.state_file);
    #[cfg(unix)]
    if let Some(meta) = info {
        if meta.transport == "unix" && !meta.endpoint.is_empty() {
            let _ = fs::remove_file(&meta.endpoint);
        }
    }
    #[cfg(not(unix))]
    let _ = info;
}

fn cleanup_owned_files(paths: &DaemonPaths, our_secret: &str, sock_path: Option<&Path>) {
    if let Ok(raw) = fs::read(&paths.state_file) {
        if let Ok(info) = serde_json::from_slice::<DaemonRuntimeInfo>(&raw) {
            if constant_time_eq(info.secret.as_bytes(), our_secret.as_bytes()) {
                let _ = fs::remove_file(&paths.state_file);
            }
        }
    }
    #[cfg(unix)]
    if let Some(path) = sock_path {
        let _ = fs::remove_file(path);
    }
    #[cfg(not(unix))]
    let _ = sock_path;
}

#[cfg(any(unix, windows, test))]
fn data_dir_short_hash(data_dir: &Path) -> String {
    let canonical = crate::config::canonicalize_existing_ancestor(data_dir);
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(canonical.to_string_lossy().as_bytes());
    }
    let digest = crate::encode_hex(hasher.finalize());
    digest[..12].to_string()
}

#[cfg(any(unix, test))]
fn path_byte_len(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().len()
    }
}

#[cfg(windows)]
fn select_windows_pipe_name(data_dir: &Path) -> String {
    let short_hash = data_dir_short_hash(data_dir);
    format!(r"\\.\pipe\atlassian-cli-daemon-{short_hash}")
}

#[cfg(any(unix, test))]
fn select_unix_socket_path_with_xdg(data_dir: &Path, xdg_runtime_dir: Option<&str>) -> PathBuf {
    let candidate = data_dir.join("atlassian-daemon.sock");
    if path_byte_len(&candidate) <= 90 {
        return candidate;
    }

    let short_hash = data_dir_short_hash(data_dir);

    if let Some(trimmed) = xdg_runtime_dir.map(str::trim).filter(|s| !s.is_empty()) {
        let xdg_candidate = PathBuf::from(trimmed)
            .join("atlassian-cli")
            .join(format!("daemon-{short_hash}.sock"));
        if path_byte_len(&xdg_candidate) <= 90 {
            return xdg_candidate;
        }
    }

    PathBuf::from(format!("/tmp/atlassian-cli-{short_hash}/atlassian-daemon.sock"))
}

#[cfg(unix)]
fn select_unix_socket_path(data_dir: &Path) -> PathBuf {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    select_unix_socket_path_with_xdg(data_dir, xdg.as_deref())
}

#[cfg(unix)]
fn ensure_unix_socket_dir(sock_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "Failed to set permissions on socket directory: {}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

#[cfg_attr(not(any(unix, windows)), allow(unused_variables))]
async fn bind_ipc_listener(
    paths: &DaemonPaths,
) -> Result<(IpcListener, String, String, Option<PathBuf>)> {
    #[cfg(windows)]
    {
        let pipe_name = select_windows_pipe_name(&paths.data_dir);
        if let Ok(server) = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
        {
            return Ok((
                IpcListener::Pipe {
                    name: pipe_name.clone(),
                    server,
                },
                "pipe".to_string(),
                pipe_name,
                None,
            ));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let sock_path = select_unix_socket_path(&paths.data_dir);
        if ensure_unix_socket_dir(&sock_path).is_ok() {
            if sock_path.exists() {
                if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
                    bail!("Daemon socket is already active: {}", sock_path.display());
                }
                let _ = fs::remove_file(&sock_path);
            }
            if let Ok(listener) = tokio::net::UnixListener::bind(&sock_path) {
                let _ = fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600));
                let endpoint = sock_path.to_string_lossy().into_owned();
                return Ok((
                    IpcListener::Unix(listener),
                    "unix".to_string(),
                    endpoint,
                    Some(sock_path),
                ));
            }
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind local IPC TCP listener")?;
    let endpoint = listener
        .local_addr()
        .context("Failed to get local IPC TCP address")?
        .to_string();
    Ok((IpcListener::Tcp(listener), "tcp".to_string(), endpoint, None))
}

async fn connect_ipc(transport: &str, endpoint: &str, timeout: Duration) -> Result<IpcStream> {
    match transport {
        #[cfg(windows)]
        "pipe" => {
            if !endpoint.starts_with(r"\\.\pipe\atlassian-cli-daemon-") {
                bail!("Invalid Atlassian daemon pipe endpoint: {endpoint}");
            }
            let deadline = Instant::now() + timeout;
            loop {
                match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint) {
                    Ok(client) => return Ok(IpcStream::PipeClient(client)),
                    Err(err) if err.raw_os_error() == Some(231) && Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("Named pipe connection failed: {endpoint}"));
                    }
                }
            }
        }
        #[cfg(unix)]
        "unix" => {
            let stream = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(endpoint))
                .await
                .context("Unix socket connection timed out")?
                .context("Unix socket connection failed")?;
            Ok(IpcStream::Unix(stream))
        }
        "tcp" => {
            let addr: SocketAddr = endpoint
                .parse()
                .with_context(|| format!("Invalid TCP endpoint: {endpoint}"))?;
            if !addr.ip().is_loopback() {
                bail!("Refusing to connect to non-loopback IPC address for security: {endpoint}");
            }
            let connect_timeout = if cfg!(windows) {
                timeout.max(Duration::from_millis(2_500))
            } else {
                timeout
            };
            let stream = tokio::time::timeout(connect_timeout, TcpStream::connect(addr))
                .await
                .context("TCP IPC connection timed out")?
                .context("TCP IPC connection failed")?;
            let _ = stream.set_nodelay(true);
            Ok(IpcStream::Tcp(stream))
        }
        other => bail!("Unsupported IPC transport: {other}"),
    }
}

fn is_stale_connect_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|io_err| {
            matches!(
                io_err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::AddrNotAvailable
            )
        })
    })
}

async fn read_limited_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut line = String::new();
    let mut limited = reader.take(MAX_IPC_FRAME_BYTES as u64 + 1);
    let n = limited.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_IPC_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds 4 MiB limit",
        ));
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

async fn write_json_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    frame: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    tokio::time::timeout(IPC_WRITE_TIMEOUT, async {
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<_, io::Error>(())
    })
    .await
    .context("IPC write timed out")??;
    Ok(())
}

async fn ipc_roundtrip(
    paths: &DaemonPaths,
    op: ClientOp,
    timeout: Duration,
) -> Result<Option<ServerFrame>> {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return Ok(None);
    };
    let stream = match connect_ipc(&info.transport, &info.endpoint, timeout).await {
        Ok(s) => s,
        Err(err) => {
            if is_stale_connect_error(&err) {
                cleanup_stale_files(paths, Some(&info));
            }
            return Ok(None);
        }
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
    let req = ClientFrame {
        secret: info.secret.clone(),
        op,
    };
    if !matches!(
        tokio::time::timeout(timeout, write_json_line(&mut write_half, &req)).await,
        Ok(Ok(()))
    ) {
        return Ok(None);
    }

    let mut reader = BufReader::new(read_half);
    match tokio::time::timeout(timeout, read_limited_line(&mut reader)).await {
        Ok(Ok(Some(line))) => match serde_json::from_str::<ServerFrame>(&line) {
            Ok(ServerFrame::Error { .. }) | Err(_) => Ok(None),
            Ok(frame) => Ok(Some(frame)),
        },
        _ => Ok(None),
    }
}

/// Fast local check whether a live Atlassian sync daemon is running for `cfg.profile`.
pub fn is_daemon_alive_fast(cfg: &AppConfig) -> bool {
    is_daemon_alive_fast_at(&DaemonPaths::for_config(cfg), &cfg.profile)
}

fn is_daemon_alive_fast_at(paths: &DaemonPaths, expected_profile: &str) -> bool {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return false;
    };
    if info.version != env!("CARGO_PKG_VERSION") {
        return false;
    }
    if !is_profile_compatible(expected_profile, &info.profile) {
        return false;
    }

    match info.transport.as_str() {
        #[cfg(windows)]
        "pipe" => {
            if !info.endpoint.starts_with(r"\\.\pipe\atlassian-cli-daemon-") {
                cleanup_stale_files(paths, Some(&info));
                return false;
            }
            match OpenOptions::new().read(true).write(true).open(&info.endpoint) {
                Ok(_) => true,
                Err(err) if err.raw_os_error() == Some(231) => true,
                Err(_) => {
                    cleanup_stale_files(paths, Some(&info));
                    false
                }
            }
        }
        #[cfg(unix)]
        "unix" => {
            if !Path::new(&info.endpoint).exists() {
                cleanup_stale_files(paths, Some(&info));
                return false;
            }
            if std::os::unix::net::UnixStream::connect(&info.endpoint).is_ok() {
                true
            } else {
                cleanup_stale_files(paths, Some(&info));
                false
            }
        }
        "tcp" => {
            let Ok(addr) = info.endpoint.parse::<SocketAddr>() else {
                cleanup_stale_files(paths, Some(&info));
                return false;
            };
            if !addr.ip().is_loopback() {
                return false;
            }
            if std::net::TcpStream::connect_timeout(&addr, FAST_TCP_PROBE_TIMEOUT).is_ok() {
                true
            } else {
                cleanup_stale_files(paths, Some(&info));
                false
            }
        }
        _ => {
            cleanup_stale_files(paths, Some(&info));
            false
        }
    }
}

/// Queries live daemon status over IPC (cleaning up stale state files if unreachable).
pub async fn status(cfg: &AppConfig) -> Result<DaemonStatus> {
    query_daemon_status(cfg).await
}

/// Alias matching sibling CLIs (`teams` / `outlook`).
pub async fn query_daemon_status(cfg: &AppConfig) -> Result<DaemonStatus> {
    query_daemon_status_at(&DaemonPaths::for_config(cfg)).await
}

async fn query_daemon_status_at(paths: &DaemonPaths) -> Result<DaemonStatus> {
    match ipc_roundtrip(paths, ClientOp::Status, IPC_CONNECT_TIMEOUT).await? {
        Some(ServerFrame::Status(status)) => Ok(status),
        _ => Ok(DaemonStatus::default()),
    }
}

/// Ensures the background daemon process is running, spawning `<current_exe> daemon run` detached if needed.
pub async fn start(cfg: &AppConfig) -> Result<DaemonStatus> {
    ensure_daemon_started(cfg).await
}

/// Alias matching sibling CLIs (`teams` / `outlook`).
pub async fn ensure_daemon_started(cfg: &AppConfig) -> Result<DaemonStatus> {
    ensure_daemon_started_at(cfg, &DaemonPaths::for_config(cfg)).await
}

async fn ensure_daemon_started_at(cfg: &AppConfig, paths: &DaemonPaths) -> Result<DaemonStatus> {
    let current = query_daemon_status_at(paths).await?;
    if current.running {
        if current.version.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
            return Ok(current);
        }
        let _ = stop_daemon_at(paths).await?;
    }

    let exe = std::env::current_exe().context("Failed to locate current executable path")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }

    let mut child = cmd
        .spawn()
        .context("Failed to spawn background Atlassian daemon process")?;
    let deadline = Instant::now() + STARTUP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        let status = query_daemon_status_at(paths).await?;
        if status.running {
            return Ok(status);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            if !exit.success() {
                let auth = AuthManager::new(&cfg.profile);
                if !auth.is_logged_in() {
                    bail!("Not logged in. Run 'atlassian login' or 'confluence login' first.");
                }
                bail!("Background daemon exited unexpectedly (exit: {exit})");
            }
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }

    let final_status = query_daemon_status_at(paths).await?;
    if final_status.running {
        return Ok(final_status);
    }
    bail!("Background daemon startup timed out (3s)")
}

/// Gracefully stops the running background daemon over IPC, if running.
pub async fn stop(cfg: &AppConfig) -> Result<bool> {
    stop_daemon(cfg).await
}

/// Alias matching sibling CLIs (`teams` / `outlook`).
pub async fn stop_daemon(cfg: &AppConfig) -> Result<bool> {
    stop_daemon_at(&DaemonPaths::for_config(cfg)).await
}

async fn stop_daemon_at(paths: &DaemonPaths) -> Result<bool> {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return Ok(false);
    };

    let ack = ipc_roundtrip(paths, ClientOp::Shutdown, IPC_CONNECT_TIMEOUT).await?;
    let was_running = matches!(ack, Some(ServerFrame::Ack { ok: true, .. }));
    if !was_running {
        cleanup_stale_files(paths, Some(&info));
        return Ok(false);
    }

    let deadline = Instant::now() + STOP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if !paths.state_file.exists() {
            break;
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
    cleanup_stale_files(paths, Some(&info));
    Ok(true)
}

/// Restarts the background daemon cleanly (stops existing instance if running, then starts a fresh one).
pub async fn restart(cfg: &AppConfig) -> Result<DaemonStatus> {
    let _ = stop(cfg).await?;
    start(cfg).await
}

/// Sends a single fire-and-forget `SyncCommand` to the running daemon.
pub async fn nudge(cfg: &AppConfig, cmd: SyncCommand) -> Result<bool> {
    nudge_daemon(cfg, Some(cmd)).await
}

/// Sends a targeted `SyncCommand::RefreshSite` to the running daemon over authenticated IPC.
pub async fn nudge_site(cfg: &AppConfig, site_id: impl Into<String>) -> Result<bool> {
    nudge_daemon(
        cfg,
        Some(SyncCommand::RefreshSite {
            site_id: site_id.into(),
        }),
    )
    .await
}

/// Alias matching sibling CLIs (`teams` / `outlook`).
pub async fn nudge_daemon(cfg: &AppConfig, cmd: Option<SyncCommand>) -> Result<bool> {
    nudge_daemon_at(&DaemonPaths::for_config(cfg), cmd).await
}

async fn nudge_daemon_at(paths: &DaemonPaths, cmd: Option<SyncCommand>) -> Result<bool> {
    match ipc_roundtrip(paths, ClientOp::Nudge { cmd }, IPC_CONNECT_TIMEOUT).await? {
        Some(ServerFrame::Ack { ok, .. }) => Ok(ok),
        _ => Ok(false),
    }
}

/// Attaches a TUI client to a live background daemon, returning a forwarding `SyncController`
/// and a live `CoreEvent` stream.
pub async fn attach(cfg: &AppConfig) -> Option<TuiDaemonConnection> {
    try_connect_tui(cfg).await
}

/// Alias matching sibling CLIs (`teams` / `outlook`).
pub async fn try_connect_tui(cfg: &AppConfig) -> Option<TuiDaemonConnection> {
    try_connect_tui_at(cfg, &DaemonPaths::for_config(cfg)).await
}

async fn try_connect_tui_at(cfg: &AppConfig, paths: &DaemonPaths) -> Option<TuiDaemonConnection> {
    let info = read_runtime_info_or_clean(paths)?;
    if info.version != env!("CARGO_PKG_VERSION") {
        return None;
    }
    if !is_profile_compatible(&cfg.profile, &info.profile) {
        return None;
    }

    let stream = match connect_ipc(&info.transport, &info.endpoint, TUI_CONNECT_TIMEOUT).await {
        Ok(s) => s,
        Err(err) => {
            if is_stale_connect_error(&err) {
                cleanup_stale_files(paths, Some(&info));
            }
            return None;
        }
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
    let subscribe_frame = ClientFrame {
        secret: info.secret,
        op: ClientOp::Subscribe,
    };
    if !matches!(
        tokio::time::timeout(
            TUI_CONNECT_TIMEOUT,
            write_json_line(&mut write_half, &subscribe_frame),
        )
        .await,
        Ok(Ok(()))
    ) {
        return None;
    }

    let mut reader = BufReader::new(read_half);
    let initial_status =
        match tokio::time::timeout(TUI_CONNECT_TIMEOUT, read_limited_line(&mut reader)).await {
            Ok(Ok(Some(raw))) => match serde_json::from_str::<ServerFrame>(&raw) {
                Ok(ServerFrame::Subscribed(status)) => status,
                _ => return None,
            },
            _ => return None,
        };

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = SyncController::from_channels(cmd_tx);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = events_tx.closed() => break,
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        // Never shut down the shared background daemon when an attached TUI client exits.
                        // `UpdateTargets` is an in-process sync controller command, never sent over IPC.
                        Some(SyncCommand::Shutdown | SyncCommand::UpdateTargets(_)) => continue,
                        Some(cmd) => {
                            if write_json_line(&mut write_half, &ClientStreamCmd::Command(cmd))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                line = read_limited_line(&mut reader) => {
                    match line {
                        Ok(Some(raw)) => {
                            match serde_json::from_str::<ServerFrame>(&raw) {
                                Ok(ServerFrame::Event(wire_ev)) => {
                                    if events_tx.send((*wire_ev).into()).is_err() {
                                        break;
                                    }
                                }
                                Ok(ServerFrame::Pong) => {}
                                Ok(ServerFrame::Error { .. }) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }
    });

    Some(TuiDaemonConnection {
        sync_ctrl,
        events_rx,
        status: initial_status,
    })
}

/// Runs the background sync daemon in the foreground until a `Shutdown` RPC or Ctrl+C arrives.
/// What "daemon run" serves, decided from the configured sites.
pub(crate) enum DaemonMode {
    /// One logged-in site: the single-site engine with that site's pool client
    /// (which keeps its cloud_id and per-product credentials).
    Single(Box<AtlassianClient>),
    /// Two or more logged-in sites: the multi-site engine.
    Multi(Vec<sync::SiteSyncTarget>),
    NotLoggedIn,
}

pub(crate) fn plan_daemon_mode(pool: &SiteClientPool) -> DaemonMode {
    let mut targets: Vec<sync::SiteSyncTarget> = sync::sync_targets(pool)
        .into_iter()
        .filter(|t| t.client.auth().is_logged_in())
        .collect();
    match targets.len() {
        0 => DaemonMode::NotLoggedIn,
        1 => DaemonMode::Single(Box::new(targets.remove(0).client)),
        _ => DaemonMode::Multi(targets),
    }
}

pub async fn run_daemon_foreground(cfg: AppConfig) -> Result<()> {
    let pool = SiteClientPool::from_config(&cfg)?;
    let mode = plan_daemon_mode(&pool);
    if matches!(mode, DaemonMode::NotLoggedIn) {
        bail!("Not logged in. Run 'atlassian login' or 'confluence login' first.");
    }

    let db_path = crate::config::cache_db_path(&cfg.profile);
    let storage = Storage::open(&db_path)?;
    let paths = DaemonPaths::for_config(&cfg);

    match mode {
        DaemonMode::Single(client) => {
            run_daemon_inner(cfg, paths, *client, storage, true, true).await
        }
        DaemonMode::Multi(targets) => {
            run_daemon_multi_site_inner(
                cfg,
                paths,
                targets,
                storage,
                sync::DEFAULT_MULTI_SITE_CONCURRENCY,
                true,
            )
            .await
        }
        DaemonMode::NotLoggedIn => unreachable!("handled above"),
    }
}

async fn run_daemon_inner(
    cfg: AppConfig,
    paths: DaemonPaths,
    client: AtlassianClient,
    storage: Storage,
    run_bootstrap: bool,
    enable_signals: bool,
) -> Result<()> {
    let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = sync::spawn_sync_engine(
        cfg.clone(),
        client.clone(),
        storage.clone(),
        engine_event_tx.clone(),
    );

    if run_bootstrap {
        let boot_client = client;
        let boot_storage = storage.clone();
        let boot_tx = engine_event_tx.clone();
        tokio::spawn(async move {
            run_initial_bootstrap(boot_client, boot_storage, boot_tx).await;
        });
    }

    run_daemon_with_engine(
        paths,
        cfg.profile,
        cfg.site_url,
        storage,
        sync_ctrl,
        engine_event_rx,
        enable_signals,
    )
    .await
}

async fn run_initial_bootstrap(
    client: AtlassianClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) {
    let _ = event_tx.send(CoreEvent::SyncStarted);
    match sync::bootstrap_workspace(&client, &storage).await {
        Ok(summary) => {
            let site_id = client.site_id().to_string();
            let st_proj = storage.clone();
            let sid_proj = site_id.clone();
            if let Ok(Ok(projects)) =
                tokio::task::spawn_blocking(move || st_proj.list_projects_for_site(&sid_proj)).await
            {
                let _ = event_tx.send(CoreEvent::ProjectsUpdated {
                    site_id: site_id.clone(),
                    items: projects,
                });
            }
            let st_boards = storage.clone();
            let sid_boards = site_id.clone();
            if let Ok(Ok(boards)) =
                tokio::task::spawn_blocking(move || st_boards.list_boards_for_site(&sid_boards, None))
                    .await
            {
                let _ = event_tx.send(CoreEvent::BoardsUpdated {
                    site_id: site_id.clone(),
                    items: boards,
                });
            }
            let st_spaces = storage.clone();
            let sid_spaces = site_id.clone();
            if let Ok(Ok(spaces)) =
                tokio::task::spawn_blocking(move || st_spaces.list_spaces_for_site(&sid_spaces)).await
            {
                let _ = event_tx.send(CoreEvent::SpacesUpdated {
                    site_id: site_id.clone(),
                    items: spaces,
                });
            }
            let _ = event_tx.send(CoreEvent::SyncFinished { summary });
        }
        Err(_) => {
            let _ = event_tx.send(CoreEvent::SyncFinished {
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        }
    }
}

/// Runs a federated multi-site background sync daemon in the foreground until a `Shutdown` RPC or Ctrl+C arrives.
pub async fn run_daemon_multi_site_foreground(
    cfg: AppConfig,
    targets: Vec<sync::SiteSyncTarget>,
    storage: Storage,
    max_concurrency: usize,
) -> Result<()> {
    let paths = DaemonPaths::for_config(&cfg);
    run_daemon_multi_site_inner(cfg, paths, targets, storage, max_concurrency, true).await
}

async fn run_daemon_multi_site_inner(
    cfg: AppConfig,
    paths: DaemonPaths,
    targets: Vec<sync::SiteSyncTarget>,
    storage: Storage,
    max_concurrency: usize,
    enable_signals: bool,
) -> Result<()> {
    let initial_sites: Vec<DaemonSiteStatus> = targets
        .iter()
        .map(|t| DaemonSiteStatus {
            site_id: t.site_id.clone(),
            name: t.site_name.clone(),
            site_url: t.client.site_url().to_string(),
            last_sync_at: None,
            last_error: None,
            circuit_open: false,
        })
        .collect();

    let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (site_results_tx, site_results_rx) =
        mpsc::unbounded_channel::<Vec<sync::SiteSyncResult>>();
    let sync_ctrl = sync::spawn_multi_site_sync_engine_with_status(
        cfg.clone(),
        targets,
        storage.clone(),
        max_concurrency,
        engine_event_tx,
        Some(site_results_tx),
    );

    run_daemon_with_engine_and_sites(
        paths,
        cfg.profile,
        cfg.site_url,
        storage,
        sync_ctrl,
        engine_event_rx,
        initial_sites,
        Some(site_results_rx),
        enable_signals,
    )
    .await
}

async fn run_daemon_with_engine(
    paths: DaemonPaths,
    profile: String,
    site_url: String,
    storage: Storage,
    sync_ctrl: SyncController,
    engine_event_rx: mpsc::UnboundedReceiver<CoreEvent>,
    enable_signals: bool,
) -> Result<()> {
    run_daemon_with_engine_and_sites(
        paths,
        profile,
        site_url,
        storage,
        sync_ctrl,
        engine_event_rx,
        Vec::new(),
        None,
        enable_signals,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_daemon_with_engine_and_sites(
    paths: DaemonPaths,
    profile: String,
    site_url: String,
    storage: Storage,
    sync_ctrl: SyncController,
    mut engine_event_rx: mpsc::UnboundedReceiver<CoreEvent>,
    initial_sites: Vec<DaemonSiteStatus>,
    mut site_results_rx: Option<mpsc::UnboundedReceiver<Vec<sync::SiteSyncResult>>>,
    enable_signals: bool,
) -> Result<()> {
    let lock_guard = StartupLockGuard::acquire(&paths.lock_file).await?;
    if query_daemon_status_at(&paths).await?.running {
        sync_ctrl.request(SyncCommand::Shutdown);
        return Ok(());
    }

    let (mut listener, transport, endpoint, sock_path) = bind_ipc_listener(&paths).await?;
    let now = chrono::Utc::now();
    let info = DaemonRuntimeInfo {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        transport,
        endpoint,
        secret: generate_secret(),
        profile,
        site_url,
        started_at_unix: now.timestamp().max(0) as u64,
        started_at: now.to_rfc3339(),
    };
    let serialized = serde_json::to_vec_pretty(&info)?;
    if let Err(err) = write_secret_file_atomic(&paths.state_file, &serialized) {
        cleanup_owned_files(&paths, &info.secret, sock_path.as_deref());
        sync_ctrl.request(SyncCommand::Shutdown);
        return Err(err);
    }
    drop(lock_guard);

    let shared = if initial_sites.is_empty() {
        Arc::new(DaemonSharedState::new(&info, Instant::now()))
    } else {
        Arc::new(DaemonSharedState::with_sites(
            &info,
            Instant::now(),
            initial_sites,
        ))
    };
    let (broadcast_tx, _) = broadcast::channel::<CoreEvent>(BROADCAST_CAPACITY);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let fanout_shared = Arc::clone(&shared);
    let fanout_tx = broadcast_tx.clone();
    let mut fanout_shutdown = shutdown_rx.clone();
    let fanout_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = fanout_shutdown.changed() => {
                    if *fanout_shutdown.borrow() {
                        break;
                    }
                }
                maybe_ev = engine_event_rx.recv() => {
                    match maybe_ev {
                        Some(ev) => {
                            fanout_shared.apply_event(&ev);
                            let _ = fanout_tx.send(ev);
                        }
                        None => break,
                    }
                }
                maybe_sites = async {
                    if let Some(ref mut rx) = site_results_rx {
                        rx.recv().await
                    } else {
                        std::future::pending::<Option<Vec<sync::SiteSyncResult>>>().await
                    }
                }, if site_results_rx.is_some() => {
                    match maybe_sites {
                        Some(results) => fanout_shared.apply_site_results(&results),
                        None => {
                            site_results_rx = None;
                        }
                    }
                }
            }
        }
    });

    let mut ctrl_c_fut = if enable_signals {
        Some(Box::pin(tokio::signal::ctrl_c()))
    } else {
        None
    };

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = async {
                if let Some(fut) = ctrl_c_fut.as_mut() {
                    let _ = fut.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if ctrl_c_fut.is_some() => {
                let _ = shutdown_tx.send(true);
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok(stream) => {
                        let conn_shared = Arc::clone(&shared);
                        let conn_storage = storage.clone();
                        let conn_ctrl = sync_ctrl.clone();
                        let conn_broadcast = broadcast_tx.clone();
                        let conn_shutdown_tx = shutdown_tx.clone();
                        let conn_shutdown_rx = shutdown_rx.clone();
                        tokio::spawn(async move {
                            let _ = handle_client_connection(
                                stream,
                                conn_shared,
                                conn_storage,
                                conn_ctrl,
                                conn_broadcast,
                                conn_shutdown_tx,
                                conn_shutdown_rx,
                            )
                            .await;
                        });
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }

    cleanup_owned_files(&paths, &info.secret, sock_path.as_deref());
    sync_ctrl.request(SyncCommand::Shutdown);
    fanout_handle.abort();
    Ok(())
}

async fn handle_client_connection(
    stream: IpcStream,
    shared: Arc<DaemonSharedState>,
    storage: Storage,
    sync_ctrl: SyncController,
    broadcast_tx: broadcast::Sender<CoreEvent>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let Some(first_line) = read_limited_line(&mut reader).await? else {
        return Ok(());
    };
    let Ok(frame) = serde_json::from_str::<ClientFrame>(&first_line) else {
        let _ = write_json_line(
            &mut write_half,
            &ServerFrame::Error {
                message: "invalid_frame".into(),
            },
        )
        .await;
        return Ok(());
    };

    if !constant_time_eq(frame.secret.as_bytes(), shared.secret.as_bytes()) {
        let _ = write_json_line(
            &mut write_half,
            &ServerFrame::Error {
                message: "unauthorized".into(),
            },
        )
        .await;
        return Ok(());
    }

    match frame.op {
        ClientOp::Status => {
            let status = shared.snapshot_status();
            write_json_line(&mut write_half, &ServerFrame::Status(status)).await?;
        }
        ClientOp::Nudge { cmd } => {
            if let Some(c) = cmd {
                match c {
                    SyncCommand::Shutdown | SyncCommand::UpdateTargets(_) => {}
                    SyncCommand::RefreshSite { ref site_id } if site_id.trim().is_empty() => {
                        write_json_line(
                            &mut write_half,
                            &ServerFrame::Ack {
                                ok: false,
                                error: Some("site_id must not be empty".into()),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    other => {
                        sync_ctrl.request(other);
                    }
                }
            }
            write_json_line(
                &mut write_half,
                &ServerFrame::Ack {
                    ok: true,
                    error: None,
                },
            )
            .await?;
        }
        ClientOp::Shutdown => {
            let _ = shutdown_tx.send(true);
            write_json_line(
                &mut write_half,
                &ServerFrame::Ack {
                    ok: true,
                    error: None,
                },
            )
            .await?;
        }
        ClientOp::Subscribe => {
            let _guard = AttachedClientGuard::acquire(&shared.attached_clients);
            sync_ctrl.notify_activity();
            let mut broadcast_rx = broadcast_tx.subscribe();
            let status = shared.snapshot_status();
            write_json_line(&mut write_half, &ServerFrame::Subscribed(status)).await?;

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    line = read_limited_line(&mut reader) => {
                        match line {
                            Ok(Some(raw)) => {
                                match serde_json::from_str::<ClientStreamCmd>(&raw) {
                                    Ok(ClientStreamCmd::Command(cmd)) => match cmd {
                                        SyncCommand::Shutdown | SyncCommand::UpdateTargets(_) => {}
                                        SyncCommand::RefreshSite { ref site_id }
                                            if site_id.trim().is_empty() => {}
                                        other => {
                                            sync_ctrl.request(other);
                                        }
                                    },
                                    Ok(ClientStreamCmd::Ping) => {
                                        if write_json_line(&mut write_half, &ServerFrame::Pong).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                    ev_res = broadcast_rx.recv() => {
                        match ev_res {
                            Ok(ev) => {
                                let wire = ServerFrame::Event(Box::new(ev.into()));
                                if write_json_line(&mut write_half, &wire).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                if send_lag_recovery_snapshots(&storage, &mut write_half).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn send_lag_recovery_snapshots<W: AsyncWrite + Unpin>(
    storage: &Storage,
    write_half: &mut W,
) -> Result<()> {
    // One snapshot per cached site, so a TUI scoped to one site keeps only its own lists.
    let st_proj = storage.clone();
    if let Ok(Ok(projects)) =
        tokio::task::spawn_blocking(move || st_proj.list_projects_all_sites()).await
    {
        for (site_id, items) in group_rows_by_site(projects) {
            write_json_line(
                write_half,
                &ServerFrame::Event(Box::new(WireCoreEvent::ProjectsUpdated(
                    WireSiteList::scoped(site_id, items),
                ))),
            )
            .await?;
        }
    }
    let st_boards = storage.clone();
    if let Ok(Ok(boards)) =
        tokio::task::spawn_blocking(move || st_boards.list_boards_all_sites(None)).await
    {
        for (site_id, items) in group_rows_by_site(boards) {
            write_json_line(
                write_half,
                &ServerFrame::Event(Box::new(WireCoreEvent::BoardsUpdated(
                    WireSiteList::scoped(site_id, items),
                ))),
            )
            .await?;
        }
    }
    let st_spaces = storage.clone();
    if let Ok(Ok(spaces)) =
        tokio::task::spawn_blocking(move || st_spaces.list_spaces_all_sites()).await
    {
        for (site_id, items) in group_rows_by_site(spaces) {
            write_json_line(
                write_half,
                &ServerFrame::Event(Box::new(WireCoreEvent::SpacesUpdated(
                    WireSiteList::scoped(site_id, items),
                ))),
            )
            .await?;
        }
    }
    Ok(())
}

/// Groups `(site_id, row)` pairs by site, keeping first-seen site order and row order.
fn group_rows_by_site<T>(rows: Vec<(String, T)>) -> Vec<(String, Vec<T>)> {
    let mut out: Vec<(String, Vec<T>)> = Vec::new();
    for (site_id, row) in rows {
        match out.iter_mut().find(|(sid, _)| *sid == site_id) {
            Some((_, items)) => items.push(row),
            None => out.push((site_id, vec![row])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDaemonHarness {
        _dir: tempfile::TempDir,
        paths: DaemonPaths,
        cmd_rx: mpsc::UnboundedReceiver<SyncCommand>,
        engine_event_tx: mpsc::UnboundedSender<CoreEvent>,
        handle: Option<tokio::task::JoinHandle<Result<()>>>,
    }

    impl TestDaemonHarness {
        async fn start(profile: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = DaemonPaths::from_data_dir(dir.path());
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
            let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
            let sync_ctrl = SyncController::from_channels(cmd_tx);

            let task_paths = paths.clone();
            let prof = profile.to_string();
            let storage = Storage::open_in_memory().unwrap();
            let handle = tokio::spawn(async move {
                run_daemon_with_engine(
                    task_paths,
                    prof,
                    "https://acme.atlassian.net".to_string(),
                    storage,
                    sync_ctrl,
                    engine_event_rx,
                    false,
                )
                .await
            });

            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if paths.state_file.exists() && is_daemon_alive_fast_at(&paths, profile) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            Self {
                _dir: dir,
                paths,
                cmd_rx,
                engine_event_tx,
                handle: Some(handle),
            }
        }

        async fn wait_exit(&mut self) -> Result<()> {
            if let Some(handle) = self.handle.take() {
                handle.await??;
            }
            Ok(())
        }
    }

    impl Drop for TestDaemonHarness {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    fn daemon_test_pool(sites: Vec<crate::config::SiteProfile>, logged_in: &[&str]) -> (tempfile::TempDir, SiteClientPool) {
        use crate::auth::StoredCredentials;
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");
        for site in &sites {
            if logged_in.contains(&site.id.as_str()) {
                AuthManager::for_account(&site.account_id)
                    .with_fallback_path(cred_path.clone())
                    .save_credentials(&StoredCredentials::ApiToken {
                        site_url: site.site_url.clone(),
                        email: "me@example.com".into(),
                        api_token: "tok".into(),
                        deployment: site.deployment,
                    })
                    .unwrap();
            }
        }
        let cfg = AppConfig {
            sites,
            ..AppConfig::default()
        };
        let pool = SiteClientPool::from_config_with_options(&cfg, Some(cred_path), None, None).unwrap();
        (dir, pool)
    }

    fn daemon_test_site(id: &str, cloud_id: Option<&str>) -> crate::config::SiteProfile {
        crate::config::SiteProfile {
            id: id.into(),
            name: id.into(),
            site_url: format!("https://{id}.atlassian.net"),
            account_id: id.into(),
            cloud_id: cloud_id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn daemon_mode_is_multi_for_two_logged_in_sites() {
        let (_dir, pool) = daemon_test_pool(
            vec![daemon_test_site("acme", None), daemon_test_site("beta", None)],
            &["acme", "beta"],
        );
        match plan_daemon_mode(&pool) {
            DaemonMode::Multi(targets) => {
                let ids: Vec<&str> = targets.iter().map(|t| t.site_id.as_str()).collect();
                assert_eq!(ids, ["acme", "beta"]);
            }
            _ => panic!("two logged-in sites must run the multi-site engine"),
        }
    }

    #[test]
    fn daemon_mode_single_uses_pool_client_with_cloud_id() {
        // "beta" has no credential, so only "acme" is served, by its own pool client.
        let (_dir, pool) = daemon_test_pool(
            vec![daemon_test_site("acme", Some("cloud-123")), daemon_test_site("beta", None)],
            &["acme"],
        );
        match plan_daemon_mode(&pool) {
            DaemonMode::Single(client) => {
                assert_eq!(client.site_id(), "acme");
                assert_eq!(client.cloud_id(), Some("cloud-123"));
            }
            _ => panic!("one logged-in site must run the single-site engine"),
        }
    }

    #[test]
    fn daemon_mode_not_logged_in_when_no_site_has_credentials() {
        let (_dir, pool) = daemon_test_pool(
            vec![daemon_test_site("acme", None), daemon_test_site("beta", None)],
            &[],
        );
        assert!(matches!(plan_daemon_mode(&pool), DaemonMode::NotLoggedIn));
    }

    #[test]
    fn test_daemon_status_and_ipc_frame_serialization_roundtrip() {
        let status = DaemonStatus {
            running: true,
            pid: Some(4242),
            version: Some("0.1.1".into()),
            transport: Some("tcp".into()),
            endpoint: Some("127.0.0.1:54321".into()),
            profile: Some("work".into()),
            site_url: Some("https://acme.atlassian.net".into()),
            uptime_secs: Some(128),
            attached_clients: 2,
            sync_busy: false,
            sync_summary: "Synced 3 projects, 2 boards, 10 issues, 4 spaces".into(),
            last_sync_at: Some("2026-11-19T13:00:00Z".into()),
            last_error: None,
            sites: Vec::new(),
        };
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: DaemonStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, status);

        // ClientFrame roundtrip with SyncCommand
        let client_frame = ClientFrame {
            secret: "abcd".repeat(16),
            op: ClientOp::Nudge {
                cmd: Some(SyncCommand::RefreshBoard {
                    board_id: 77,
                    sprint_id: Some(12),
                    site_id: None,
                }),
            },
        };
        let cf_json = serde_json::to_string(&client_frame).unwrap();
        let cf_back: ClientFrame = serde_json::from_str(&cf_json).unwrap();
        assert_eq!(cf_back.secret, client_frame.secret);
        assert!(matches!(
            cf_back.op,
            ClientOp::Nudge {
                cmd: Some(SyncCommand::RefreshBoard {
                    board_id: 77,
                    sprint_id: Some(12),
                    site_id: None,
                })
            }
        ));

        // ClientStreamCmd roundtrip (Command + Ping)
        let stream_cmd = ClientStreamCmd::Command(SyncCommand::RefreshSpace {
            space_key: "ENG".into(),
            site_id: None,
        });
        let sc_json = serde_json::to_string(&stream_cmd).unwrap();
        let sc_back: ClientStreamCmd = serde_json::from_str(&sc_json).unwrap();
        assert!(matches!(
            sc_back,
            ClientStreamCmd::Command(SyncCommand::RefreshSpace { ref space_key, .. }) if space_key == "ENG"
        ));
        let ping_json = serde_json::to_string(&ClientStreamCmd::Ping).unwrap();
        let ping_back: ClientStreamCmd = serde_json::from_str(&ping_json).unwrap();
        assert!(matches!(ping_back, ClientStreamCmd::Ping));

        // ServerFrame + WireCoreEvent roundtrip
        let events = vec![
            CoreEvent::SyncStarted,
            CoreEvent::SyncFinished {
                summary: "Synced 1 projects, 1 boards, 2 issues, 1 spaces".into(),
            },
            CoreEvent::ProjectsUpdated {
                site_id: "default".into(),
                items: vec![ProjectSummary {
                    id: "1".into(),
                    key: "ENG".into(),
                    name: "Engineering".into(),
                    ..Default::default()
                }],
            },
            CoreEvent::BoardsUpdated {
                site_id: "default".into(),
                items: vec![BoardSummary {
                    id: 10,
                    name: "ENG Board".into(),
                    ..Default::default()
                }],
            },
            CoreEvent::BoardIssuesUpdated {
                site_id: "default".into(),
                board_id: 10,
                sprint: Some(SprintSummary {
                    id: 5,
                    board_id: 10,
                    name: "Sprint 5".into(),
                    state: "active".into(),
                    ..Default::default()
                }),
                issues: vec![],
            },
            CoreEvent::IssuesUpdated {
                site_id: "default".into(),
                issues: vec![],
            },
            CoreEvent::SpacesUpdated {
                site_id: "default".into(),
                items: vec![SpaceSummary {
                    id: "sp-1".into(),
                    key: "ENG".into(),
                    name: "Engineering".into(),
                    ..Default::default()
                }],
            },
            CoreEvent::SpacePagesUpdated {
                site_id: "default".into(),
                space_key: "ENG".into(),
                pages: vec![],
            },
            CoreEvent::SyncError("401 Unauthorized".into()),
        ];

        for ev in events {
            let sf = ServerFrame::Event(Box::new(ev.into()));
            let raw = serde_json::to_string(&sf).unwrap();
            let parsed: ServerFrame = serde_json::from_str(&raw).unwrap();
            assert!(matches!(parsed, ServerFrame::Event(_)));
        }
    }

    #[test]
    fn wire_core_event_roundtrips_site_id() {
        let ev = CoreEvent::BoardsUpdated {
            site_id: "site-b".into(),
            items: vec![BoardSummary {
                id: 10,
                name: "B board".into(),
                ..Default::default()
            }],
        };
        let raw = serde_json::to_string(&ServerFrame::Event(Box::new(ev.into()))).unwrap();
        assert!(raw.contains("\"site_id\":\"site-b\""), "{raw}");
        let ServerFrame::Event(wire) = serde_json::from_str::<ServerFrame>(&raw).unwrap() else {
            panic!("expected event frame: {raw}");
        };
        match CoreEvent::from(*wire) {
            CoreEvent::BoardsUpdated { site_id, items } => {
                assert_eq!(site_id, "site-b");
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, 10);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        for ev in [
            CoreEvent::ProjectsUpdated {
                site_id: "site-a".into(),
                items: vec![],
            },
            CoreEvent::SpacesUpdated {
                site_id: "site-a".into(),
                items: vec![],
            },
        ] {
            let raw = serde_json::to_string(&WireCoreEvent::from(ev)).unwrap();
            let back: CoreEvent = serde_json::from_str::<WireCoreEvent>(&raw).unwrap().into();
            match back {
                CoreEvent::ProjectsUpdated { site_id, .. } | CoreEvent::SpacesUpdated { site_id, .. } => {
                    assert_eq!(site_id, "site-a")
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn wire_core_event_accepts_legacy_shape() {
        // Pre-0.8.1 daemons sent the list as a bare array payload.
        let legacy_space = SpaceSummary {
            id: "sp-1".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            ..Default::default()
        };
        let raw = serde_json::json!({ "event": "spaces_updated", "payload": [legacy_space] });
        let wire: WireCoreEvent = serde_json::from_value(raw).unwrap();
        match CoreEvent::from(wire) {
            CoreEvent::SpacesUpdated { site_id, items } => {
                assert_eq!(site_id, "");
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].key, "ENG");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let raw = r#"{"event":"projects_updated","payload":[]}"#;
        let wire: WireCoreEvent = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            CoreEvent::from(wire),
            CoreEvent::ProjectsUpdated { ref site_id, ref items } if site_id.is_empty() && items.is_empty()
        ));
    }

    #[test]
    fn group_rows_by_site_keeps_site_and_row_order() {
        let grouped = group_rows_by_site(vec![
            ("b".to_string(), 1),
            ("a".to_string(), 2),
            ("b".to_string(), 3),
        ]);
        assert_eq!(
            grouped,
            vec![("b".to_string(), vec![1, 3]), ("a".to_string(), vec![2])]
        );
    }

    #[tokio::test]
    async fn test_read_limited_line_trims_crlf_and_enforces_frame_limit() {
        let mut ok_reader = BufReader::new(&b"{\"type\":\"pong\"}\r\n"[..]);
        let line = read_limited_line(&mut ok_reader).await.unwrap().unwrap();
        assert_eq!(line, "{\"type\":\"pong\"}");

        let oversized = vec![b'a'; MAX_IPC_FRAME_BYTES + 16];
        let mut bad_reader = BufReader::new(oversized.as_slice());
        let err = read_limited_line(&mut bad_reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_daemon_lifecycle_auth_and_graceful_shutdown() {
        let mut harness = TestDaemonHarness::start("work").await;

        assert!(is_daemon_alive_fast_at(&harness.paths, "work"));
        assert!(is_daemon_alive_fast_at(&harness.paths, ""));
        assert!(!is_daemon_alive_fast_at(&harness.paths, "other"));

        let status = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(status.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(status.profile.as_deref(), Some("work"));
        assert_eq!(
            status.site_url.as_deref(),
            Some("https://acme.atlassian.net")
        );
        assert_eq!(status.attached_clients, 0);

        // Unauthorized secret is rejected without crashing daemon.
        let info = read_runtime_info_or_clean(&harness.paths).unwrap();
        assert_eq!(info.secret.len(), 64);
        let stream = connect_ipc(&info.transport, &info.endpoint, Duration::from_millis(500))
            .await
            .unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        write_json_line(
            &mut write_half,
            &ClientFrame {
                secret: "0".repeat(64),
                op: ClientOp::Status,
            },
        )
        .await
        .unwrap();
        let mut lines = BufReader::new(read_half).lines();
        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: ServerFrame = serde_json::from_str(&resp_line).unwrap();
        assert!(matches!(resp, ServerFrame::Error { ref message } if message == "unauthorized"));

        // Nudge forwards SyncCommand cleanly.
        assert!(
            nudge_daemon_at(&harness.paths, Some(SyncCommand::RefreshAll))
                .await
                .unwrap()
        );
        let nudged = tokio::time::timeout(Duration::from_secs(1), harness.cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nudged, SyncCommand::RefreshAll);

        // Graceful shutdown removes state file and exits cleanly.
        assert!(stop_daemon_at(&harness.paths).await.unwrap());
        harness.wait_exit().await.unwrap();

        assert!(!harness.paths.state_file.exists());
        assert!(!is_daemon_alive_fast_at(&harness.paths, "work"));
        let after = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(!after.running);
    }

    #[tokio::test]
    async fn test_tui_attach_broadcast_and_disconnect_keeps_daemon_alive() {
        let mut harness = TestDaemonHarness::start("default").await;
        let cfg = AppConfig {
            profile: "default".into(),
            ..Default::default()
        };

        let mut conn = try_connect_tui_at(&cfg, &harness.paths)
            .await
            .expect("should attach to running daemon");
        assert!(conn.status.running);
        assert_eq!(conn.status.attached_clients, 1);

        // Initial NotifyUserActivity is emitted on subscribe.
        let first_cmd = tokio::time::timeout(Duration::from_secs(1), harness.cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_cmd, SyncCommand::NotifyUserActivity);

        // Forward SyncCommand from attached TUI client -> daemon.
        assert!(conn.sync_ctrl.request(SyncCommand::RefreshSpace {
            space_key: "ENG".into(),
            site_id: None,
        }));
        let forwarded = tokio::time::timeout(Duration::from_secs(1), harness.cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            forwarded,
            SyncCommand::RefreshSpace {
                space_key: "ENG".into(),
                site_id: None,
            }
        );

        // Broadcast CoreEvents from daemon -> attached TUI + status snapshot updates.
        harness.engine_event_tx.send(CoreEvent::SyncStarted).unwrap();
        harness
            .engine_event_tx
            .send(CoreEvent::SyncFinished {
                summary: "Synced 2 projects, 1 boards, 5 issues, 1 spaces".into(),
            })
            .unwrap();

        let ev1 = tokio::time::timeout(Duration::from_secs(1), conn.events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, CoreEvent::SyncStarted));

        let ev2 = tokio::time::timeout(Duration::from_secs(1), conn.events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev2, CoreEvent::SyncFinished { .. }));

        let updated = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(!updated.sync_busy);
        assert_eq!(
            updated.sync_summary,
            "Synced 2 projects, 1 boards, 5 issues, 1 spaces"
        );
        assert!(updated.last_sync_at.is_some());

        // TUI sending Shutdown & disconnecting MUST NOT kill the shared daemon.
        let _ = conn.sync_ctrl.request(SyncCommand::Shutdown);
        drop(conn);

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut detached = false;
        while Instant::now() < deadline {
            let st = query_daemon_status_at(&harness.paths).await.unwrap();
            assert!(st.running);
            if st.attached_clients == 0 {
                detached = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(detached, "attached_clients should return to 0 after disconnect");

        assert!(stop_daemon_at(&harness.paths).await.unwrap());
        harness.wait_exit().await.unwrap();
    }

    #[test]
    fn test_daemon_status_multi_site_serialization_roundtrip_and_backwards_compat() {
        let status = DaemonStatus {
            running: true,
            pid: Some(9001),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            transport: Some("tcp".into()),
            endpoint: Some("127.0.0.1:55555".into()),
            profile: Some("default".into()),
            site_url: Some("https://acme.atlassian.net".into()),
            uptime_secs: Some(42),
            attached_clients: 1,
            sync_busy: false,
            sync_summary: "Synced 1/2 sites (2 projects, 1 boards, 5 issues, 1 spaces)".into(),
            last_sync_at: Some("2026-11-19T14:00:00Z".into()),
            last_error: None,
            sites: vec![
                DaemonSiteStatus {
                    site_id: "acme-cloud".into(),
                    name: "Acme Cloud".into(),
                    site_url: "https://acme.atlassian.net".into(),
                    last_sync_at: Some("2026-11-19T14:00:00Z".into()),
                    last_error: None,
                    circuit_open: false,
                },
                DaemonSiteStatus {
                    site_id: "corp-dc".into(),
                    name: "Corp Data Center".into(),
                    site_url: "https://jira.corp.local".into(),
                    last_sync_at: None,
                    last_error: Some("HTTP 403 Forbidden".into()),
                    circuit_open: true,
                },
            ],
        };

        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: DaemonStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, status);
        assert_eq!(decoded.sites.len(), 2);
        assert!(!decoded.sites[0].circuit_open);
        assert!(decoded.sites[1].circuit_open);

        // Legacy JSON payload without "sites" field deserializes cleanly via #[serde(default)].
        let legacy_json = r#"{
            "running": true,
            "pid": 1234,
            "version": "0.2.0",
            "transport": "tcp",
            "endpoint": "127.0.0.1:12345",
            "profile": "default",
            "site_url": "https://acme.atlassian.net",
            "uptime_secs": 10,
            "attached_clients": 0,
            "sync_busy": false,
            "sync_summary": "",
            "last_sync_at": null,
            "last_error": null
        }"#;
        let legacy: DaemonStatus = serde_json::from_str(legacy_json).unwrap();
        assert!(legacy.running);
        assert!(legacy.sites.is_empty());

        // RefreshSite wire frame roundtrip.
        let refresh_frame = ClientFrame {
            secret: "ab".repeat(32),
            op: ClientOp::Nudge {
                cmd: Some(SyncCommand::RefreshSite {
                    site_id: "corp-dc".into(),
                }),
            },
        };
        let raw = serde_json::to_string(&refresh_frame).unwrap();
        let parsed: ClientFrame = serde_json::from_str(&raw).unwrap();
        assert!(matches!(
            parsed.op,
            ClientOp::Nudge {
                cmd: Some(SyncCommand::RefreshSite { ref site_id })
            } if site_id == "corp-dc"
        ));
    }

    #[tokio::test]
    async fn startup_lock_fails_instead_of_spinning_when_lock_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        // The lock has to exist for `create_new` (AlreadyExists) and resist `remove_file`.
        // Windows: a file held open without FILE_SHARE_DELETE. A directory would not do there,
        // because `create_new` on it fails with access denied instead of AlreadyExists.
        #[cfg(windows)]
        let _holder = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ_WRITE)
                .open(&lock_path)
                .unwrap()
        };
        // Elsewhere: a directory, which `remove_file` cannot unlink.
        #[cfg(not(windows))]
        fs::create_dir(&lock_path).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            StartupLockGuard::acquire(&lock_path),
        )
        .await
        .expect("acquire must give up instead of spinning");
        let err = result
            .err()
            .expect("the lock cannot be acquired")
            .to_string();
        assert!(
            err.contains("Could not acquire daemon startup lock"),
            "{err}"
        );
        assert!(err.contains("Delete the file manually or retry."), "{err}");
        assert!(lock_path.exists());
    }

    #[tokio::test]
    async fn startup_lock_replaces_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let stale = fs::File::create(&lock_path).unwrap();
        stale
            .set_modified(SystemTime::now() - (LOCK_STALE_AGE + Duration::from_secs(5)))
            .unwrap();
        drop(stale);

        let started = Instant::now();
        let guard = StartupLockGuard::acquire(&lock_path).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a stale lock is replaced without waiting for the deadline"
        );
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(guard);
        assert!(!lock_path.exists());
    }

    #[tokio::test]
    async fn test_daemon_forwards_refresh_site_ipc_and_tracks_site_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::from_data_dir(dir.path());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let (_engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
        let (site_results_tx, site_results_rx) =
            mpsc::unbounded_channel::<Vec<sync::SiteSyncResult>>();
        let sync_ctrl = SyncController::from_channels(cmd_tx);

        let initial_sites = vec![
            DaemonSiteStatus {
                site_id: "acme-cloud".into(),
                name: "Acme Cloud".into(),
                site_url: "https://acme.atlassian.net".into(),
                last_sync_at: None,
                last_error: None,
                circuit_open: false,
            },
            DaemonSiteStatus {
                site_id: "corp-dc".into(),
                name: "Corp DC".into(),
                site_url: "https://jira.corp.local".into(),
                last_sync_at: None,
                last_error: None,
                circuit_open: false,
            },
        ];

        let task_paths = paths.clone();
        let storage = Storage::open_in_memory().unwrap();
        let handle = tokio::spawn(async move {
            run_daemon_with_engine_and_sites(
                task_paths,
                "default".into(),
                "https://acme.atlassian.net".into(),
                storage,
                sync_ctrl,
                engine_event_rx,
                initial_sites,
                Some(site_results_rx),
                false,
            )
            .await
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if paths.state_file.exists() && is_daemon_alive_fast_at(&paths, "default") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Empty site_id is rejected cleanly without crashing daemon.
        assert!(
            !nudge_daemon_at(
                &paths,
                Some(SyncCommand::RefreshSite {
                    site_id: "   ".into(),
                }),
            )
            .await
            .unwrap()
        );

        // Nudge with RefreshSite forwards cleanly over authenticated IPC.
        assert!(
            nudge_daemon_at(
                &paths,
                Some(SyncCommand::RefreshSite {
                    site_id: "acme-cloud".into(),
                }),
            )
            .await
            .unwrap()
        );
        let forwarded = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            forwarded,
            SyncCommand::RefreshSite {
                site_id: "acme-cloud".into(),
            }
        );

        // Attached TUI client can also forward RefreshSite over the subscribe stream.
        let cfg = AppConfig {
            profile: "default".into(),
            ..Default::default()
        };
        let conn = try_connect_tui_at(&cfg, &paths)
            .await
            .expect("should attach to running multi-site daemon");
        assert_eq!(conn.status.sites.len(), 2);
        let _notify = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(conn.sync_ctrl.request(SyncCommand::RefreshSite {
            site_id: "corp-dc".into(),
        }));
        let streamed = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            streamed,
            SyncCommand::RefreshSite {
                site_id: "corp-dc".into(),
            }
        );

        // Site sync results update DaemonStatus.sites.
        site_results_tx
            .send(vec![
                sync::SiteSyncResult {
                    site_id: "acme-cloud".into(),
                    site_name: "Acme Cloud".into(),
                    site_url: "https://acme.atlassian.net".into(),
                    succeeded: true,
                    attempted_steps: 4,
                    succeeded_steps: 4,
                    skipped_steps: 0,
                    projects: 2,
                    boards: 1,
                    issues: 5,
                    spaces: 1,
                    summary: "Synced 2 projects, 1 boards, 5 issues, 1 spaces".into(),
                    error: None,
                    circuit_open: false,
                },
                sync::SiteSyncResult {
                    site_id: "corp-dc".into(),
                    site_name: "Corp DC".into(),
                    site_url: "https://jira.corp.local".into(),
                    succeeded: false,
                    attempted_steps: 4,
                    succeeded_steps: 0,
                    skipped_steps: 0,
                    projects: 0,
                    boards: 0,
                    issues: 0,
                    spaces: 0,
                    summary: "Synced 0 projects, 0 boards, 0 issues, 0 spaces".into(),
                    error: Some("HTTP 403 Forbidden".into()),
                    circuit_open: true,
                },
            ])
            .unwrap();

        let status_deadline = Instant::now() + Duration::from_secs(1);
        let mut updated = DaemonStatus::default();
        while Instant::now() < status_deadline {
            updated = query_daemon_status_at(&paths).await.unwrap();
            if updated
                .sites
                .iter()
                .any(|s| s.site_id == "corp-dc" && s.circuit_open)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let acme = updated
            .sites
            .iter()
            .find(|s| s.site_id == "acme-cloud")
            .unwrap();
        assert!(acme.last_sync_at.is_some());
        assert!(acme.last_error.is_none());
        assert!(!acme.circuit_open);

        let corp = updated
            .sites
            .iter()
            .find(|s| s.site_id == "corp-dc")
            .unwrap();
        assert!(corp.last_sync_at.is_none());
        assert_eq!(corp.last_error.as_deref(), Some("HTTP 403 Forbidden"));
        assert!(corp.circuit_open);

        drop(conn);
        assert!(stop_daemon_at(&paths).await.unwrap());
        handle.await.unwrap().unwrap();
    }

    #[test]
    fn select_unix_socket_path_xdg_distinguishes_long_profiles() {
        let long_base = format!("/home/{}", "verylongdirectorysegment/".repeat(5));
        let profile_a = PathBuf::from(&long_base).join("profile-alpha");
        let profile_b = PathBuf::from(&long_base).join("profile-beta");

        let sock_a = select_unix_socket_path_with_xdg(&profile_a, Some("/run/user/1000"));
        let sock_b = select_unix_socket_path_with_xdg(&profile_b, Some("/run/user/1000"));

        assert_ne!(sock_a, sock_b, "distinct profiles must not collide in XDG_RUNTIME_DIR");
        let hash_a = data_dir_short_hash(&profile_a);
        let hash_b = data_dir_short_hash(&profile_b);
        assert!(
            sock_a.to_string_lossy().contains(&hash_a),
            "expected hash {hash_a} in {}",
            sock_a.display()
        );
        assert!(
            sock_b.to_string_lossy().contains(&hash_b),
            "expected hash {hash_b} in {}",
            sock_b.display()
        );
        assert!(path_byte_len(&sock_a) <= 90);
        assert!(path_byte_len(&sock_b) <= 90);
    }

    #[test]
    fn data_dir_short_hash_is_stable_across_symlinks_and_canonical_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let real_home = tmp.path().join("real-home");
        fs::create_dir(&real_home).unwrap();

        let real_data = real_home.join("atlassian-cli").join("default");
        let non_canon_data = real_home
            .join(".")
            .join("..")
            .join("real-home")
            .join("atlassian-cli")
            .join("default");
        assert_eq!(
            data_dir_short_hash(&real_data),
            data_dir_short_hash(&non_canon_data)
        );

        let sym_home = tmp.path().join("sym-home");
        let linked = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_home, &sym_home).unwrap();
                true
            }
            #[cfg(windows)]
            {
                match std::os::windows::fs::symlink_dir(&real_home, &sym_home) {
                    Ok(()) => true,
                    Err(e) if e.raw_os_error() == Some(1314) => false,
                    Err(e) => panic!("unexpected symlink_dir error: {e}"),
                }
            }
        };
        if linked {
            let sym_data = sym_home.join("atlassian-cli").join("default");
            assert_eq!(
                data_dir_short_hash(&real_data),
                data_dir_short_hash(&sym_data)
            );
            let long_suffix = "a".repeat(95);
            let long_real = real_home.join(&long_suffix);
            let long_sym = sym_home.join(&long_suffix);
            assert_eq!(
                select_unix_socket_path_with_xdg(&long_real, Some("/run/user/1000")),
                select_unix_socket_path_with_xdg(&long_sym, Some("/run/user/1000"))
            );
        }
    }
}
