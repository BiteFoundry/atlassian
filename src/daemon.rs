//! Lightweight single-instance background sync daemon for Atlassian (`jira daemon` / `confluence daemon`).
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
    atlassian::AtlassianClient,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Default for DaemonStatus {
    fn default() -> Self {
        Self {
            running: false,
            pid: None,
            version: None,
            transport: None,
            endpoint: None,
            profile: None,
            site_url: None,
            uptime_secs: None,
            attached_clients: 0,
            sync_busy: false,
            sync_summary: String::new(),
            last_sync_at: None,
            last_error: None,
        }
    }
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
    ProjectsUpdated(Vec<ProjectSummary>),
    BoardsUpdated(Vec<BoardSummary>),
    BoardIssuesUpdated {
        board_id: i64,
        sprint: Option<SprintSummary>,
        issues: Vec<IssueSummary>,
    },
    IssuesUpdated(Vec<IssueSummary>),
    SpacesUpdated(Vec<SpaceSummary>),
    SpacePagesUpdated {
        space_key: String,
        pages: Vec<PageSummary>,
    },
    SyncError(String),
}

impl From<CoreEvent> for WireCoreEvent {
    fn from(ev: CoreEvent) -> Self {
        match ev {
            CoreEvent::SyncStarted => Self::SyncStarted,
            CoreEvent::SyncFinished { summary } => Self::SyncFinished { summary },
            CoreEvent::ProjectsUpdated(v) => Self::ProjectsUpdated(v),
            CoreEvent::BoardsUpdated(v) => Self::BoardsUpdated(v),
            CoreEvent::BoardIssuesUpdated {
                board_id,
                sprint,
                issues,
            } => Self::BoardIssuesUpdated {
                board_id,
                sprint,
                issues,
            },
            CoreEvent::IssuesUpdated(v) => Self::IssuesUpdated(v),
            CoreEvent::SpacesUpdated(v) => Self::SpacesUpdated(v),
            CoreEvent::SpacePagesUpdated { space_key, pages } => {
                Self::SpacePagesUpdated { space_key, pages }
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
            WireCoreEvent::ProjectsUpdated(v) => Self::ProjectsUpdated(v),
            WireCoreEvent::BoardsUpdated(v) => Self::BoardsUpdated(v),
            WireCoreEvent::BoardIssuesUpdated {
                board_id,
                sprint,
                issues,
            } => Self::BoardIssuesUpdated {
                board_id,
                sprint,
                issues,
            },
            WireCoreEvent::IssuesUpdated(v) => Self::IssuesUpdated(v),
            WireCoreEvent::SpacesUpdated(v) => Self::SpacesUpdated(v),
            WireCoreEvent::SpacePagesUpdated { space_key, pages } => {
                Self::SpacePagesUpdated { space_key, pages }
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
    Event(WireCoreEvent),
    Pong,
    Error { message: String },
}

#[derive(Debug, Default)]
struct SharedSyncStatus {
    sync_busy: bool,
    sync_summary: String,
    last_sync_at: Option<String>,
    last_error: Option<String>,
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
            sync_status: Mutex::new(SharedSyncStatus::default()),
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
            CoreEvent::ProjectsUpdated(_)
            | CoreEvent::BoardsUpdated(_)
            | CoreEvent::BoardIssuesUpdated { .. }
            | CoreEvent::IssuesUpdated(_)
            | CoreEvent::SpacesUpdated(_)
            | CoreEvent::SpacePagesUpdated { .. } => {
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
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(modified) = meta.modified() {
                            if SystemTime::now()
                                .duration_since(modified)
                                .unwrap_or_default()
                                >= LOCK_STALE_AGE
                            {
                                let _ = fs::remove_file(path);
                                continue;
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = fs::remove_file(path);
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
    hex::encode(bytes)
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

#[cfg(windows)]
fn select_windows_pipe_name(data_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data_dir.to_string_lossy().as_bytes());
    let digest = hex::encode(hasher.finalize());
    let short_hash = &digest[..12];
    format!(r"\\.\pipe\atlassian-cli-daemon-{short_hash}")
}

#[cfg(unix)]
fn select_unix_socket_path(data_dir: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    let candidate = data_dir.join("atlassian-daemon.sock");
    if candidate.as_os_str().as_bytes().len() <= 90 {
        return candidate;
    }

    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            let xdg_candidate = PathBuf::from(trimmed)
                .join("atlassian-cli")
                .join("atlassian-daemon.sock");
            if xdg_candidate.as_os_str().as_bytes().len() <= 90 {
                return xdg_candidate;
            }
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(data_dir.as_os_str().as_bytes());
    let digest = hex::encode(hasher.finalize());
    let short_hash = &digest[..12];
    PathBuf::from(format!("/tmp/atlassian-cli-{short_hash}/atlassian-daemon.sock"))
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
                    bail!("Not logged in. Run 'jira login' or 'confluence login' first.");
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
                        Some(SyncCommand::Shutdown) => continue,
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
                                    if events_tx.send(wire_ev.into()).is_err() {
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
pub async fn run_daemon_foreground(cfg: AppConfig) -> Result<()> {
    let auth = AuthManager::new(&cfg.profile);
    if !auth.is_logged_in() {
        bail!("Not logged in. Run 'jira login' or 'confluence login' first.");
    }

    let db_path = crate::config::cache_db_path(&cfg.profile);
    let storage = Storage::open(&db_path)?;
    let client = AtlassianClient::new(cfg.clone(), auth)?;
    let paths = DaemonPaths::for_config(&cfg);

    run_daemon_inner(cfg, paths, client, storage, true, true).await
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
            let st_proj = storage.clone();
            if let Ok(Ok(projects)) =
                tokio::task::spawn_blocking(move || st_proj.list_projects()).await
            {
                let _ = event_tx.send(CoreEvent::ProjectsUpdated(projects));
            }
            let st_boards = storage.clone();
            if let Ok(Ok(boards)) =
                tokio::task::spawn_blocking(move || st_boards.list_boards(None)).await
            {
                let _ = event_tx.send(CoreEvent::BoardsUpdated(boards));
            }
            let st_spaces = storage.clone();
            if let Ok(Ok(spaces)) =
                tokio::task::spawn_blocking(move || st_spaces.list_spaces()).await
            {
                let _ = event_tx.send(CoreEvent::SpacesUpdated(spaces));
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

async fn run_daemon_with_engine(
    paths: DaemonPaths,
    profile: String,
    site_url: String,
    storage: Storage,
    sync_ctrl: SyncController,
    mut engine_event_rx: mpsc::UnboundedReceiver<CoreEvent>,
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

    let shared = Arc::new(DaemonSharedState::new(&info, Instant::now()));
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
                if !matches!(c, SyncCommand::Shutdown) {
                    sync_ctrl.request(c);
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
                                    Ok(ClientStreamCmd::Command(cmd)) => {
                                        if !matches!(cmd, SyncCommand::Shutdown) {
                                            sync_ctrl.request(cmd);
                                        }
                                    }
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
                                let wire = ServerFrame::Event(ev.into());
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
    let st_proj = storage.clone();
    if let Ok(Ok(projects)) = tokio::task::spawn_blocking(move || st_proj.list_projects()).await {
        write_json_line(
            write_half,
            &ServerFrame::Event(WireCoreEvent::ProjectsUpdated(projects)),
        )
        .await?;
    }
    let st_boards = storage.clone();
    if let Ok(Ok(boards)) = tokio::task::spawn_blocking(move || st_boards.list_boards(None)).await {
        write_json_line(
            write_half,
            &ServerFrame::Event(WireCoreEvent::BoardsUpdated(boards)),
        )
        .await?;
    }
    let st_spaces = storage.clone();
    if let Ok(Ok(spaces)) = tokio::task::spawn_blocking(move || st_spaces.list_spaces()).await {
        write_json_line(
            write_half,
            &ServerFrame::Event(WireCoreEvent::SpacesUpdated(spaces)),
        )
        .await?;
    }
    Ok(())
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
                    sprint_id: Some(12)
                })
            }
        ));

        // ClientStreamCmd roundtrip (Command + Ping)
        let stream_cmd = ClientStreamCmd::Command(SyncCommand::RefreshSpace {
            space_key: "ENG".into(),
        });
        let sc_json = serde_json::to_string(&stream_cmd).unwrap();
        let sc_back: ClientStreamCmd = serde_json::from_str(&sc_json).unwrap();
        assert!(matches!(
            sc_back,
            ClientStreamCmd::Command(SyncCommand::RefreshSpace { ref space_key }) if space_key == "ENG"
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
            CoreEvent::ProjectsUpdated(vec![ProjectSummary {
                id: "1".into(),
                key: "ENG".into(),
                name: "Engineering".into(),
                ..Default::default()
            }]),
            CoreEvent::BoardsUpdated(vec![BoardSummary {
                id: 10,
                name: "ENG Board".into(),
                ..Default::default()
            }]),
            CoreEvent::BoardIssuesUpdated {
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
            CoreEvent::IssuesUpdated(vec![]),
            CoreEvent::SpacesUpdated(vec![SpaceSummary {
                id: "sp-1".into(),
                key: "ENG".into(),
                name: "Engineering".into(),
                ..Default::default()
            }]),
            CoreEvent::SpacePagesUpdated {
                space_key: "ENG".into(),
                pages: vec![],
            },
            CoreEvent::SyncError("401 Unauthorized".into()),
        ];

        for ev in events {
            let sf = ServerFrame::Event(ev.into());
            let raw = serde_json::to_string(&sf).unwrap();
            let parsed: ServerFrame = serde_json::from_str(&raw).unwrap();
            assert!(matches!(parsed, ServerFrame::Event(_)));
        }
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
        }));
        let forwarded = tokio::time::timeout(Duration::from_secs(1), harness.cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            forwarded,
            SyncCommand::RefreshSpace {
                space_key: "ENG".into()
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
}
