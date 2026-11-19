//! Background synchronization engine for Jira projects, boards, active sprint issues,
//! and Confluence spaces & root page trees, with per-endpoint circuit breakers,
//! `Retry-After` / exponential backoff, and quiet transient-error handling.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use rand::Rng;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc,
    time::{interval_at, Instant as TokioInstant, MissedTickBehavior},
};

use crate::{
    atlassian::{client::HttpStatusError, AtlassianClient},
    config::AppConfig,
    models::{BoardSummary, CoreEvent, IssueSummary, SpaceSummary, SprintSummary},
    storage::Storage,
};

/// Non-intrusive status summary emitted when background polling encounters transient failures.
pub const POLL_RETRY_SUMMARY: &str = "Sync paused; retrying automatically";

const AUTH_EXPIRY_PAUSE_SECS: u64 = 60;
const FORBIDDEN_COOLDOWN_SECS: u64 = 300;
const NOT_FOUND_COOLDOWN_SECS: u64 = 300;
const RATE_LIMIT_BASE_COOLDOWN_SECS: u64 = 30;
const TRANSIENT_BASE_BACKOFF_MS: u64 = 15_000;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const MIN_RETRY_AFTER_SECS: u64 = 5;
const MAX_RETRY_AFTER_SECS: u64 = 300;

/// Synced API domains tracked by independent circuit breakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncEndpoint {
    JiraProjects,
    JiraAgileBoards,
    JiraIssues,
    ConfluenceSpaces,
    ConfluencePages,
}

impl SyncEndpoint {
    pub fn label(self) -> &'static str {
        match self {
            Self::JiraProjects => "Jira projects",
            Self::JiraAgileBoards => "Jira Agile boards",
            Self::JiraIssues => "Jira issues",
            Self::ConfluenceSpaces => "Confluence spaces",
            Self::ConfluencePages => "Confluence pages",
        }
    }
}

/// Classification of sync failures for circuit breaker and retry policy routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncErrorKind {
    Unauthorized401,
    Forbidden403,
    NotFound404,
    RateLimited429 { retry_after_secs: Option<u64> },
    Server5xx,
    Transient,
}

impl SyncErrorKind {
    pub fn is_quiet_transient(self) -> bool {
        matches!(
            self,
            Self::RateLimited429 { .. } | Self::Server5xx | Self::Transient
        )
    }
}

/// Reason why a circuit breaker opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitOpenReason {
    Unauthorized401,
    Forbidden403,
    NotFound404,
    RateLimited429,
    TransientBackoff,
}

/// Observable circuit breaker state at a given point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open {
        reason: CircuitOpenReason,
        cooldown_until: Instant,
    },
    HalfOpen,
}

/// Result returned when recording a failure against an endpoint circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitTripResult {
    pub newly_opened: bool,
    pub cooldown: Duration,
    pub reason: CircuitOpenReason,
}

/// Parses an optional `Retry-After` duration in seconds from an error message string.
pub fn parse_retry_after_secs(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    for needle in ["retry-after:", "retry-after=", "retry after "] {
        if let Some(idx) = lower.find(needle) {
            let rest = lower[idx + needle.len()..].trim_start();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(secs) = digits.parse::<u64>() {
                return Some(secs);
            }
        }
    }
    None
}

/// Classifies an `anyhow::Error` from `AtlassianClient` into a structured `SyncErrorKind`.
pub fn classify_sync_error(err: &anyhow::Error) -> SyncErrorKind {
    let full_msg = format!("{err:#}");
    let retry_after = parse_retry_after_secs(&full_msg);

    if let Some(http_err) = err.downcast_ref::<HttpStatusError>() {
        return match http_err.status {
            StatusCode::UNAUTHORIZED => SyncErrorKind::Unauthorized401,
            StatusCode::FORBIDDEN => SyncErrorKind::Forbidden403,
            StatusCode::NOT_FOUND => SyncErrorKind::NotFound404,
            StatusCode::TOO_MANY_REQUESTS => SyncErrorKind::RateLimited429 {
                retry_after_secs: retry_after,
            },
            s if s.is_server_error() => SyncErrorKind::Server5xx,
            _ => SyncErrorKind::Transient,
        };
    }

    let lower = full_msg.to_ascii_lowercase();
    if lower.contains("http 401")
        || lower.contains("401 unauthorized")
        || lower.contains("unauthorized")
        || lower.contains("token expired")
    {
        return SyncErrorKind::Unauthorized401;
    }
    if lower.contains("http 403")
        || lower.contains("403 forbidden")
        || lower.contains("forbidden")
        || lower.contains("access denied")
        || lower.contains("insufficient")
    {
        return SyncErrorKind::Forbidden403;
    }
    if lower.contains("http 404") || lower.contains("404 not found") {
        return SyncErrorKind::NotFound404;
    }
    if lower.contains("http 429")
        || lower.contains("429 too many requests")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
    {
        return SyncErrorKind::RateLimited429 {
            retry_after_secs: retry_after,
        };
    }
    if lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
    {
        return SyncErrorKind::Server5xx;
    }

    SyncErrorKind::Transient
}

/// Computes exponential backoff in milliseconds with bounded jitter.
pub fn compute_backoff_ms(base_ms: u64, failures: u32, jitter_factor: f64) -> u64 {
    let shift = failures.saturating_sub(1).min(6);
    let raw_ms = base_ms
        .max(1)
        .saturating_mul(1u64 << shift)
        .min(MAX_ERROR_BACKOFF_MS);
    let jittered = (raw_ms as f64 * jitter_factor).round() as u64;
    jittered.clamp(1, MAX_ERROR_BACKOFF_MS)
}

/// Per-endpoint circuit breaker with cooldown timers and half-open probe transitions.
#[derive(Debug, Clone)]
pub struct EndpointCircuitBreaker {
    endpoint: SyncEndpoint,
    consecutive_failures: u32,
    open_reason: Option<CircuitOpenReason>,
    cooldown_until: Option<Instant>,
    half_open_probe_active: bool,
}

impl EndpointCircuitBreaker {
    pub fn new(endpoint: SyncEndpoint) -> Self {
        Self {
            endpoint,
            consecutive_failures: 0,
            open_reason: None,
            cooldown_until: None,
            half_open_probe_active: false,
        }
    }

    pub fn endpoint(&self) -> SyncEndpoint {
        self.endpoint
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Evaluates the current circuit state at `now` without mutating internal state.
    pub fn state_at(&self, now: Instant) -> CircuitState {
        match (self.open_reason, self.cooldown_until) {
            (Some(reason), Some(until)) if now < until => CircuitState::Open {
                reason,
                cooldown_until: until,
            },
            (Some(_), Some(_)) => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }

    /// Returns remaining cooldown duration if the breaker is currently open at `now`.
    pub fn remaining_cooldown(&self, now: Instant) -> Option<Duration> {
        match self.state_at(now) {
            CircuitState::Open { cooldown_until, .. } => {
                Some(cooldown_until.saturating_duration_since(now))
            }
            _ => None,
        }
    }

    /// Checks whether a request is allowed at `now`.
    /// When cooldown expires, transitions into a single half-open probe attempt.
    pub fn should_allow(&mut self, now: Instant) -> bool {
        match self.state_at(now) {
            CircuitState::Closed => true,
            CircuitState::Open { .. } => false,
            CircuitState::HalfOpen => {
                self.half_open_probe_active = true;
                true
            }
        }
    }

    /// Records a successful call, closing the circuit and clearing failure counters.
    /// Returns `true` if the circuit recovered from an open/half-open/failing state.
    pub fn record_success(&mut self) -> bool {
        let was_tripped = self.open_reason.is_some() || self.consecutive_failures > 0;
        self.consecutive_failures = 0;
        self.open_reason = None;
        self.cooldown_until = None;
        self.half_open_probe_active = false;
        was_tripped
    }

    /// Records a failure and trips or extends the circuit cooldown according to `kind`.
    pub fn record_failure(&mut self, kind: SyncErrorKind, now: Instant) -> CircuitTripResult {
        let jitter = rand::thread_rng().gen_range(0.85_f64..=1.15_f64);
        self.record_failure_with_jitter(kind, now, jitter)
    }

    /// Deterministic failure recorder for tests and callers providing explicit jitter.
    pub fn record_failure_with_jitter(
        &mut self,
        kind: SyncErrorKind,
        now: Instant,
        jitter_factor: f64,
    ) -> CircuitTripResult {
        let was_open = matches!(self.state_at(now), CircuitState::Open { .. });
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.half_open_probe_active = false;

        let (reason, cooldown) = match kind {
            SyncErrorKind::Unauthorized401 => (
                CircuitOpenReason::Unauthorized401,
                Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS),
            ),
            SyncErrorKind::Forbidden403 => (
                CircuitOpenReason::Forbidden403,
                Duration::from_secs(FORBIDDEN_COOLDOWN_SECS),
            ),
            SyncErrorKind::NotFound404 => (
                CircuitOpenReason::NotFound404,
                Duration::from_secs(NOT_FOUND_COOLDOWN_SECS),
            ),
            SyncErrorKind::RateLimited429 { retry_after_secs } => {
                let cooldown = if let Some(secs) = retry_after_secs {
                    Duration::from_secs(secs.clamp(MIN_RETRY_AFTER_SECS, MAX_RETRY_AFTER_SECS))
                } else {
                    let ms = compute_backoff_ms(
                        RATE_LIMIT_BASE_COOLDOWN_SECS * 1000,
                        self.consecutive_failures,
                        jitter_factor,
                    );
                    Duration::from_millis(ms)
                };
                (CircuitOpenReason::RateLimited429, cooldown)
            }
            SyncErrorKind::Server5xx | SyncErrorKind::Transient => {
                let ms = compute_backoff_ms(
                    TRANSIENT_BASE_BACKOFF_MS,
                    self.consecutive_failures,
                    jitter_factor,
                );
                (CircuitOpenReason::TransientBackoff, Duration::from_millis(ms))
            }
        };

        self.open_reason = Some(reason);
        self.cooldown_until = Some(now + cooldown);

        CircuitTripResult {
            newly_opened: !was_open,
            cooldown,
            reason,
        }
    }

    /// Resets the breaker immediately (e.g., on explicit user-triggered refresh).
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.open_reason = None;
        self.cooldown_until = None;
        self.half_open_probe_active = false;
    }
}

/// Manages per-endpoint circuit breakers and global auth-expiry cooldown state.
#[derive(Debug, Default)]
pub struct CircuitBreakerRegistry {
    breakers: HashMap<SyncEndpoint, EndpointCircuitBreaker>,
    auth_pause_until: Option<Instant>,
    degraded: bool,
}

impl CircuitBreakerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn breaker_mut(&mut self, endpoint: SyncEndpoint) -> &mut EndpointCircuitBreaker {
        self.breakers
            .entry(endpoint)
            .or_insert_with(|| EndpointCircuitBreaker::new(endpoint))
    }

    pub fn is_auth_paused(&self, now: Instant) -> bool {
        self.auth_pause_until.is_some_and(|until| now < until)
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    pub fn mark_degraded(&mut self) -> bool {
        if !self.degraded {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    pub fn clear_degraded(&mut self) -> bool {
        if self.degraded {
            self.degraded = false;
            true
        } else {
            false
        }
    }

    pub fn state_at(&self, endpoint: SyncEndpoint, now: Instant) -> CircuitState {
        if self.is_auth_paused(now) {
            if let Some(until) = self.auth_pause_until {
                return CircuitState::Open {
                    reason: CircuitOpenReason::Unauthorized401,
                    cooldown_until: until,
                };
            }
        }
        self.breakers
            .get(&endpoint)
            .map(|b| b.state_at(now))
            .unwrap_or(CircuitState::Closed)
    }

    pub fn should_allow(&mut self, endpoint: SyncEndpoint, now: Instant) -> bool {
        if self.is_auth_paused(now) {
            return false;
        }
        self.breaker_mut(endpoint).should_allow(now)
    }

    pub fn record_success(&mut self, endpoint: SyncEndpoint) -> bool {
        self.auth_pause_until = None;
        self.breaker_mut(endpoint).record_success()
    }

    pub fn record_failure(
        &mut self,
        endpoint: SyncEndpoint,
        err: &anyhow::Error,
        now: Instant,
    ) -> (SyncErrorKind, CircuitTripResult) {
        let kind = classify_sync_error(err);
        let trip = self.record_failure_kind(endpoint, kind, now);
        (kind, trip)
    }

    pub fn record_failure_kind(
        &mut self,
        endpoint: SyncEndpoint,
        kind: SyncErrorKind,
        now: Instant,
    ) -> CircuitTripResult {
        let trip = self.breaker_mut(endpoint).record_failure(kind, now);
        if kind == SyncErrorKind::Unauthorized401 {
            self.auth_pause_until = Some(now + trip.cooldown);
        }
        trip
    }

    pub fn reset_endpoint(&mut self, endpoint: SyncEndpoint) {
        self.auth_pause_until = None;
        if let Some(breaker) = self.breakers.get_mut(&endpoint) {
            breaker.reset();
        }
    }

    pub fn reset_all(&mut self) {
        self.auth_pause_until = None;
        for breaker in self.breakers.values_mut() {
            breaker.reset();
        }
    }

    pub fn open_count(&self, now: Instant) -> usize {
        self.breakers
            .values()
            .filter(|b| matches!(b.state_at(now), CircuitState::Open { .. }))
            .count()
    }
}

struct SyncSharedActivity {
    epoch: Instant,
    last_activity_ms: AtomicU64,
}

impl SyncSharedActivity {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncCommand {
    RefreshAll,
    RefreshBoard {
        board_id: i64,
        sprint_id: Option<i64>,
    },
    RefreshProjectIssues {
        project_key: String,
    },
    RefreshSpace {
        space_key: String,
    },
    NotifyUserActivity,
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    tx: mpsc::UnboundedSender<SyncCommand>,
    activity: Arc<SyncSharedActivity>,
}

impl SyncController {
    pub fn from_channels(tx: mpsc::UnboundedSender<SyncCommand>) -> Self {
        Self {
            tx,
            activity: Arc::new(SyncSharedActivity::new()),
        }
    }

    pub fn request(&self, cmd: SyncCommand) -> bool {
        if !matches!(cmd, SyncCommand::Shutdown) {
            self.activity.record_activity(Instant::now());
        }
        self.tx.send(cmd).is_ok()
    }

    pub fn send(&self, cmd: SyncCommand) -> bool {
        self.request(cmd)
    }

    pub fn notify_activity(&self) {
        self.activity.record_activity(Instant::now());
        let _ = self.tx.send(SyncCommand::NotifyUserActivity);
    }
}

pub async fn bootstrap_workspace(client: &AtlassianClient, storage: &Storage) -> Result<String> {
    let mut breakers = CircuitBreakerRegistry::new();
    let outcome = run_full_sync(client, storage, None, &mut breakers).await;
    if outcome.succeeded_steps == 0 {
        if let Some(err) = outcome.first_error {
            return Err(err);
        }
    }
    Ok(outcome.summary)
}

pub fn spawn_sync_engine(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let activity = Arc::new(SyncSharedActivity::new());
    let engine_activity = Arc::clone(&activity);

    tokio::spawn(async move {
        let secs = config.sync_interval_secs.max(5);
        let period = Duration::from_secs(secs);
        let mut ticker = interval_at(TokioInstant::now() + period, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut breakers = CircuitBreakerRegistry::new();

        loop {
            tokio::select! {
                maybe_cmd = rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        break;
                    };
                    match cmd {
                        SyncCommand::Shutdown => break,
                        SyncCommand::NotifyUserActivity => {
                            engine_activity.record_activity(Instant::now());
                        }
                        SyncCommand::RefreshAll => {
                            breakers.reset_all();
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            let outcome = run_full_sync(&client, &storage, Some(&core_tx), &mut breakers).await;
                            emit_full_sync_events(&core_tx, outcome, &mut breakers, false);
                        }
                        SyncCommand::RefreshBoard { board_id, sprint_id } => {
                            breakers.reset_endpoint(SyncEndpoint::JiraAgileBoards);
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_board(&client, &storage, board_id, sprint_id, Some(&core_tx)).await {
                                Ok(_) => {
                                    breakers.record_success(SyncEndpoint::JiraAgileBoards);
                                    breakers.clear_degraded();
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!("Synced board #{board_id}"),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure(
                                        SyncEndpoint::JiraAgileBoards,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded();
                                        let _ = core_tx.send(CoreEvent::SyncFinished {
                                            summary: POLL_RETRY_SUMMARY.to_string(),
                                        });
                                    } else {
                                        let _ = core_tx.send(CoreEvent::SyncError(format!(
                                            "Failed to sync board #{board_id}: {err:#}"
                                        )));
                                    }
                                }
                            }
                        }
                        SyncCommand::RefreshProjectIssues { project_key } => {
                            breakers.reset_endpoint(SyncEndpoint::JiraIssues);
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_project_issues(&client, &storage, &project_key, Some(&core_tx)).await {
                                Ok(count) => {
                                    breakers.record_success(SyncEndpoint::JiraIssues);
                                    breakers.clear_degraded();
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!(
                                            "Synced {count} issues for project {project_key}"
                                        ),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure(
                                        SyncEndpoint::JiraIssues,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded();
                                        let _ = core_tx.send(CoreEvent::SyncFinished {
                                            summary: POLL_RETRY_SUMMARY.to_string(),
                                        });
                                    } else {
                                        let _ = core_tx.send(CoreEvent::SyncError(format!(
                                            "Failed to sync issues for project {project_key}: {err:#}"
                                        )));
                                    }
                                }
                            }
                        }
                        SyncCommand::RefreshSpace { space_key } => {
                            breakers.reset_endpoint(SyncEndpoint::ConfluencePages);
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_space_pages(&client, &storage, &space_key, Some(&core_tx)).await {
                                Ok(count) => {
                                    breakers.record_success(SyncEndpoint::ConfluencePages);
                                    breakers.clear_degraded();
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!(
                                            "Synced {count} pages for space {space_key}"
                                        ),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure(
                                        SyncEndpoint::ConfluencePages,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded();
                                        let _ = core_tx.send(CoreEvent::SyncFinished {
                                            summary: POLL_RETRY_SUMMARY.to_string(),
                                        });
                                    } else {
                                        let _ = core_tx.send(CoreEvent::SyncError(format!(
                                            "Failed to sync space {space_key}: {err:#}"
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    if breakers.is_auth_paused(now) {
                        continue;
                    }
                    let outcome = run_full_sync(&client, &storage, Some(&core_tx), &mut breakers).await;
                    if outcome.attempted_steps == 0 {
                        // All endpoints are currently in cooldown; stay quiet until cooldown expires.
                        continue;
                    }
                    emit_full_sync_events(&core_tx, outcome, &mut breakers, true);
                }
            }
        }
    });

    SyncController { tx, activity }
}

fn emit_full_sync_events(
    core_tx: &mpsc::UnboundedSender<CoreEvent>,
    outcome: FullSyncOutcome,
    breakers: &mut CircuitBreakerRegistry,
    quiet_background: bool,
) {
    if outcome.succeeded_steps > 0 {
        breakers.clear_degraded();
        let _ = core_tx.send(CoreEvent::SyncFinished {
            summary: outcome.summary,
        });
        return;
    }

    if let Some(err) = outcome.first_error {
        let kind = outcome
            .first_error_kind
            .unwrap_or_else(|| classify_sync_error(&err));
        if quiet_background || kind.is_quiet_transient() {
            if breakers.mark_degraded() || !quiet_background {
                let _ = core_tx.send(CoreEvent::SyncFinished {
                    summary: POLL_RETRY_SUMMARY.to_string(),
                });
            }
        } else {
            let _ = core_tx.send(CoreEvent::SyncError(format!("Sync failed: {err:#}")));
        }
    } else if !quiet_background {
        let _ = core_tx.send(CoreEvent::SyncFinished {
            summary: outcome.summary,
        });
    }
}

struct FullSyncOutcome {
    summary: String,
    attempted_steps: usize,
    succeeded_steps: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    skipped_steps: usize,
    first_error: Option<anyhow::Error>,
    first_error_kind: Option<SyncErrorKind>,
}

async fn run_full_sync(
    client: &AtlassianClient,
    storage: &Storage,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
    breakers: &mut CircuitBreakerRegistry,
) -> FullSyncOutcome {
    let mut project_count = 0usize;
    let mut board_count = 0usize;
    let mut issue_count = 0usize;
    let mut space_count = 0usize;
    let mut attempted_steps = 0usize;
    let mut succeeded_steps = 0usize;
    let mut skipped_steps = 0usize;
    let mut first_error: Option<anyhow::Error> = None;
    let mut first_error_kind: Option<SyncErrorKind> = None;

    let mut record_err = |err: anyhow::Error, kind: SyncErrorKind| {
        if first_error.is_none() {
            first_error = Some(err);
            first_error_kind = Some(kind);
        }
    };

    // 1. Jira projects
    if breakers.should_allow(SyncEndpoint::JiraProjects, Instant::now()) {
        attempted_steps += 1;
        match client.list_projects().await {
            Ok(projects) => {
                breakers.record_success(SyncEndpoint::JiraProjects);
                succeeded_steps += 1;
                project_count = projects.len();
                if let Err(err) = storage.upsert_projects(&projects) {
                    record_err(err, SyncErrorKind::Transient);
                } else if let Some(tx) = core_tx {
                    let stored = storage.list_projects().unwrap_or(projects);
                    let _ = tx.send(CoreEvent::ProjectsUpdated(stored));
                }
            }
            Err(err) => {
                let (kind, _) =
                    breakers.record_failure(SyncEndpoint::JiraProjects, &err, Instant::now());
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 2. Jira Agile boards + top/default board columns, sprints, active sprint issues
    if breakers.should_allow(SyncEndpoint::JiraAgileBoards, Instant::now()) {
        attempted_steps += 1;
        match client.list_boards(None).await {
            Ok(mut boards) => {
                breakers.record_success(SyncEndpoint::JiraAgileBoards);
                succeeded_steps += 1;
                board_count = boards.len();
                if let Err(err) = storage.upsert_boards(&boards) {
                    record_err(err, SyncErrorKind::Transient);
                }
                if let Some(target_board) = select_default_board(&boards, client.config()) {
                    let board_id = target_board.id;
                    if let Ok(cols) = client.get_board_columns(board_id).await {
                        if let Some(b) = boards.iter_mut().find(|b| b.id == board_id) {
                            b.columns = cols;
                        }
                        let _ = storage.upsert_boards(&boards);
                    }
                    if let Ok((_, count)) =
                        sync_board_details(client, storage, board_id, None, core_tx).await
                    {
                        issue_count += count;
                    }
                }
                if let Some(tx) = core_tx {
                    let _ = tx.send(CoreEvent::BoardsUpdated(boards));
                }
            }
            Err(err) => {
                let (kind, _) =
                    breakers.record_failure(SyncEndpoint::JiraAgileBoards, &err, Instant::now());
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 3. My open Jira issues
    if breakers.should_allow(SyncEndpoint::JiraIssues, Instant::now()) {
        attempted_steps += 1;
        match client
            .search_issues_jql(AtlassianClient::my_open_issues_jql(), 50, None, None)
            .await
        {
            Ok(page) => {
                breakers.record_success(SyncEndpoint::JiraIssues);
                succeeded_steps += 1;
                let fetched = page.issues.len();
                if fetched > issue_count {
                    issue_count = fetched;
                }
                if let Err(err) = storage.upsert_issues(&page.issues) {
                    record_err(err, SyncErrorKind::Transient);
                } else if let Some(tx) = core_tx {
                    let _ = tx.send(CoreEvent::IssuesUpdated(page.issues));
                }
            }
            Err(err) => {
                let (kind, _) =
                    breakers.record_failure(SyncEndpoint::JiraIssues, &err, Instant::now());
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 4. Confluence spaces + default/first space root pages
    if breakers.should_allow(SyncEndpoint::ConfluenceSpaces, Instant::now()) {
        attempted_steps += 1;
        match client.list_spaces(50).await {
            Ok(spaces) => {
                breakers.record_success(SyncEndpoint::ConfluenceSpaces);
                succeeded_steps += 1;
                space_count = spaces.len();
                if let Err(err) = storage.upsert_spaces(&spaces) {
                    record_err(err, SyncErrorKind::Transient);
                } else if let Some(tx) = core_tx {
                    let stored = storage.list_spaces().unwrap_or_else(|_| spaces.clone());
                    let _ = tx.send(CoreEvent::SpacesUpdated(stored));
                }
                if let Some(target_space) = select_default_space(&spaces, client.config()) {
                    if breakers.should_allow(SyncEndpoint::ConfluencePages, Instant::now()) {
                        match sync_space_pages(client, storage, &target_space.key, core_tx).await {
                            Ok(_) => {
                                breakers.record_success(SyncEndpoint::ConfluencePages);
                            }
                            Err(err) => {
                                breakers.record_failure(
                                    SyncEndpoint::ConfluencePages,
                                    &err,
                                    Instant::now(),
                                );
                            }
                        }
                    }
                }
            }
            Err(err) => {
                let (kind, _) =
                    breakers.record_failure(SyncEndpoint::ConfluenceSpaces, &err, Instant::now());
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    let summary = format_sync_summary(project_count, board_count, issue_count, space_count);
    FullSyncOutcome {
        summary,
        attempted_steps,
        succeeded_steps,
        skipped_steps,
        first_error,
        first_error_kind,
    }
}

async fn sync_board(
    client: &AtlassianClient,
    storage: &Storage,
    board_id: i64,
    sprint_id: Option<i64>,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<(Option<SprintSummary>, usize)> {
    if let Ok(cols) = client.get_board_columns(board_id).await {
        let mut boards = storage.list_boards(None).unwrap_or_default();
        if let Some(b) = boards.iter_mut().find(|b| b.id == board_id) {
            b.columns = cols;
            let _ = storage.upsert_boards(&boards);
            if let Some(tx) = core_tx {
                let _ = tx.send(CoreEvent::BoardsUpdated(boards));
            }
        }
    }
    sync_board_details(client, storage, board_id, sprint_id, core_tx).await
}

async fn sync_board_details(
    client: &AtlassianClient,
    storage: &Storage,
    board_id: i64,
    explicit_sprint_id: Option<i64>,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<(Option<SprintSummary>, usize)> {
    let sprints = client.list_board_sprints(board_id).await.unwrap_or_default();
    if !sprints.is_empty() {
        storage.upsert_sprints(&sprints)?;
    }

    let active_sprint = if let Some(sid) = explicit_sprint_id {
        sprints.iter().find(|s| s.id == sid).cloned()
    } else {
        sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .cloned()
    };
    let target_sprint_id = explicit_sprint_id.or_else(|| active_sprint.as_ref().map(|s| s.id));

    let issues: Vec<IssueSummary> = client
        .list_board_issues(board_id, target_sprint_id, 50)
        .await?;
    let count = issues.len();
    storage.upsert_board_issues(board_id, target_sprint_id, &issues)?;

    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::BoardIssuesUpdated {
            board_id,
            sprint: active_sprint.clone(),
            issues,
        });
    }
    Ok((active_sprint, count))
}

async fn sync_project_issues(
    client: &AtlassianClient,
    storage: &Storage,
    project_key: &str,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<usize> {
    let jql = format!("project = \"{project_key}\" ORDER BY updated DESC");
    let page = client.search_issues_jql(&jql, 50, None, None).await?;
    let count = page.issues.len();
    storage.upsert_issues(&page.issues)?;
    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::IssuesUpdated(page.issues));
    }
    Ok(count)
}

async fn sync_space_pages(
    client: &AtlassianClient,
    storage: &Storage,
    space_key: &str,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<usize> {
    let pages = client.list_space_root_pages(space_key, 50).await?;
    let count = pages.len();
    storage.upsert_pages(&pages)?;
    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::SpacePagesUpdated {
            space_key: space_key.to_string(),
            pages,
        });
    }
    Ok(count)
}

fn select_default_board<'a>(
    boards: &'a [BoardSummary],
    config: &AppConfig,
) -> Option<&'a BoardSummary> {
    if let Some(bid) = config.default_board_id {
        if let Some(b) = boards.iter().find(|b| b.id == bid) {
            return Some(b);
        }
    }
    if let Some(ref pk) = config.default_project {
        if let Some(b) = boards.iter().find(|b| {
            b.project_key
                .as_deref()
                .map(|k| k.eq_ignore_ascii_case(pk))
                .unwrap_or(false)
        }) {
            return Some(b);
        }
    }
    boards.first()
}

fn select_default_space<'a>(
    spaces: &'a [SpaceSummary],
    config: &AppConfig,
) -> Option<&'a SpaceSummary> {
    if let Some(ref sk) = config.default_space_key {
        if let Some(s) = spaces.iter().find(|s| s.key.eq_ignore_ascii_case(sk)) {
            return Some(s);
        }
    }
    spaces.first()
}

pub(crate) fn format_sync_summary(
    projects: usize,
    boards: usize,
    issues: usize,
    spaces: usize,
) -> String {
    format!("Synced {projects} projects, {boards} boards, {issues} issues, {spaces} spaces")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use anyhow::anyhow;
    use axum::{http::StatusCode as AxumStatus, response::IntoResponse, routing::{get, post}, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    use crate::{
        auth::{AuthManager, StoredCredentials},
        config::DeploymentKind,
    };

    #[test]
    fn test_format_sync_summary_and_default_selection() {
        assert_eq!(
            format_sync_summary(3, 2, 28, 4),
            "Synced 3 projects, 2 boards, 28 issues, 4 spaces"
        );

        let cfg = AppConfig {
            default_board_id: Some(20),
            default_space_key: Some("ENG".into()),
            ..Default::default()
        };
        let boards = vec![
            BoardSummary {
                id: 10,
                name: "First".into(),
                ..Default::default()
            },
            BoardSummary {
                id: 20,
                name: "Target".into(),
                ..Default::default()
            },
        ];
        assert_eq!(select_default_board(&boards, &cfg).unwrap().id, 20);

        let spaces = vec![
            SpaceSummary {
                key: "OPS".into(),
                ..Default::default()
            },
            SpaceSummary {
                key: "ENG".into(),
                ..Default::default()
            },
        ];
        assert_eq!(select_default_space(&spaces, &cfg).unwrap().key, "ENG");
    }

    #[test]
    fn test_classify_sync_error_http_and_string_variants() {
        let err_401: anyhow::Error = HttpStatusError {
            status: StatusCode::UNAUTHORIZED,
            message: "Unauthorized".into(),
            body: String::new(),
        }
        .into();
        assert_eq!(classify_sync_error(&err_401), SyncErrorKind::Unauthorized401);

        let err_403: anyhow::Error = HttpStatusError {
            status: StatusCode::FORBIDDEN,
            message: "Forbidden".into(),
            body: String::new(),
        }
        .into();
        assert_eq!(classify_sync_error(&err_403), SyncErrorKind::Forbidden403);

        let err_404: anyhow::Error = HttpStatusError {
            status: StatusCode::NOT_FOUND,
            message: "Not Found".into(),
            body: String::new(),
        }
        .into();
        assert_eq!(classify_sync_error(&err_404), SyncErrorKind::NotFound404);

        let err_429: anyhow::Error = HttpStatusError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limited (Retry-After: 42)".into(),
            body: String::new(),
        }
        .into();
        assert_eq!(
            classify_sync_error(&err_429),
            SyncErrorKind::RateLimited429 {
                retry_after_secs: Some(42)
            }
        );

        let err_503: anyhow::Error = HttpStatusError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Service Unavailable".into(),
            body: String::new(),
        }
        .into();
        assert_eq!(classify_sync_error(&err_503), SyncErrorKind::Server5xx);

        let err_net = anyhow!("connection reset by peer");
        assert_eq!(classify_sync_error(&err_net), SyncErrorKind::Transient);
        assert!(classify_sync_error(&err_net).is_quiet_transient());
        assert!(!classify_sync_error(&err_403).is_quiet_transient());
    }

    #[test]
    fn test_circuit_breaker_state_transitions_and_cooldown_expiry() {
        let base = Instant::now();
        let mut breaker = EndpointCircuitBreaker::new(SyncEndpoint::JiraAgileBoards);

        // Initially Closed and allows requests.
        assert_eq!(breaker.state_at(base), CircuitState::Closed);
        assert!(breaker.should_allow(base));
        assert!(breaker.remaining_cooldown(base).is_none());

        // 403 Forbidden trips breaker into Open with 300s cooldown.
        let trip = breaker.record_failure_with_jitter(SyncErrorKind::Forbidden403, base, 1.0);
        assert!(trip.newly_opened);
        assert_eq!(trip.reason, CircuitOpenReason::Forbidden403);
        assert_eq!(trip.cooldown, Duration::from_secs(FORBIDDEN_COOLDOWN_SECS));

        // Before cooldown expires: Open and blocked.
        let mid = base + Duration::from_secs(120);
        assert!(matches!(
            breaker.state_at(mid),
            CircuitState::Open {
                reason: CircuitOpenReason::Forbidden403,
                ..
            }
        ));
        assert!(!breaker.should_allow(mid));
        assert_eq!(
            breaker.remaining_cooldown(mid),
            Some(Duration::from_secs(FORBIDDEN_COOLDOWN_SECS - 120))
        );

        // Once cooldown expires: transitions to HalfOpen and allows a probe.
        let after = base + Duration::from_secs(FORBIDDEN_COOLDOWN_SECS);
        assert_eq!(breaker.state_at(after), CircuitState::HalfOpen);
        assert!(breaker.should_allow(after));

        // Failed probe re-opens the circuit.
        let retrip = breaker.record_failure_with_jitter(SyncErrorKind::Forbidden403, after, 1.0);
        assert!(retrip.newly_opened);
        assert!(!breaker.should_allow(after + Duration::from_secs(1)));

        // After second cooldown expires and probe succeeds: returns to Closed.
        let after2 = after + Duration::from_secs(FORBIDDEN_COOLDOWN_SECS);
        assert!(breaker.should_allow(after2));
        assert!(breaker.record_success());
        assert_eq!(breaker.state_at(after2), CircuitState::Closed);
        assert_eq!(breaker.consecutive_failures(), 0);
        assert!(!breaker.record_success());
    }

    #[test]
    fn test_circuit_breaker_rate_limit_retry_after_and_exponential_backoff() {
        let base = Instant::now();
        let mut breaker = EndpointCircuitBreaker::new(SyncEndpoint::JiraIssues);

        // Explicit Retry-After is respected and clamped to [5s, 300s].
        let t1 = breaker.record_failure_with_jitter(
            SyncErrorKind::RateLimited429 {
                retry_after_secs: Some(45),
            },
            base,
            1.0,
        );
        assert_eq!(t1.reason, CircuitOpenReason::RateLimited429);
        assert_eq!(t1.cooldown, Duration::from_secs(45));

        let t_clamped = breaker.record_failure_with_jitter(
            SyncErrorKind::RateLimited429 {
                retry_after_secs: Some(9999),
            },
            base,
            1.0,
        );
        assert_eq!(t_clamped.cooldown, Duration::from_secs(MAX_RETRY_AFTER_SECS));

        // Reset and check exponential backoff on repeated transient / 5xx errors.
        breaker.reset();
        let b1 = breaker.record_failure_with_jitter(SyncErrorKind::Server5xx, base, 1.0);
        let b2 = breaker.record_failure_with_jitter(SyncErrorKind::Server5xx, base, 1.0);
        let b3 = breaker.record_failure_with_jitter(SyncErrorKind::Transient, base, 1.0);
        assert_eq!(b1.cooldown, Duration::from_millis(15_000));
        assert_eq!(b2.cooldown, Duration::from_millis(30_000));
        assert_eq!(b3.cooldown, Duration::from_millis(60_000));
    }

    #[test]
    fn test_registry_auth_pause_blocks_all_endpoints_until_reset_or_expiry() {
        let now = Instant::now();
        let mut reg = CircuitBreakerRegistry::new();

        reg.record_failure_kind(
            SyncEndpoint::JiraProjects,
            SyncErrorKind::Unauthorized401,
            now,
        );
        assert!(reg.is_auth_paused(now));
        assert!(!reg.should_allow(SyncEndpoint::JiraProjects, now));
        assert!(!reg.should_allow(SyncEndpoint::ConfluenceSpaces, now));

        let after_pause = now + Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS + 1);
        assert!(!reg.is_auth_paused(after_pause));
        assert!(reg.should_allow(SyncEndpoint::ConfluenceSpaces, after_pause));
        assert!(reg.should_allow(SyncEndpoint::JiraProjects, after_pause));

        // Explicit reset clears auth pause immediately.
        reg.record_failure_kind(
            SyncEndpoint::JiraIssues,
            SyncErrorKind::Unauthorized401,
            after_pause,
        );
        assert!(reg.is_auth_paused(after_pause));
        reg.reset_all();
        assert!(!reg.is_auth_paused(after_pause));
        assert!(reg.should_allow(SyncEndpoint::JiraIssues, after_pause));
    }

    #[tokio::test]
    async fn test_circuit_breaker_skips_403_and_404_endpoints_across_poll_cycles() {
        let project_hits = Arc::new(AtomicUsize::new(0));
        let board_hits = Arc::new(AtomicUsize::new(0));
        let issue_hits = Arc::new(AtomicUsize::new(0));
        let space_hits = Arc::new(AtomicUsize::new(0));

        let p_hits = project_hits.clone();
        let b_hits = board_hits.clone();
        let i_hits = issue_hits.clone();
        let s_hits = space_hits.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/project/search",
                get(move || {
                    let hits = p_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "values": [{
                                "id": "10001",
                                "key": "ENG",
                                "name": "Engineering",
                                "projectTypeKey": "software"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board",
                get(move || {
                    let hits = b_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            AxumStatus::FORBIDDEN,
                            Json(json!({"errorMessages": ["Agile disabled"]})),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/rest/api/3/search/jql",
                post(move || {
                    let hits = i_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "issues": [] }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(move || {
                    let hits = s_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            AxumStatus::NOT_FOUND,
                            Json(json!({"message": "Confluence not installed"})),
                        )
                            .into_response()
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base_url = format!("http://{addr}");
        let cfg = AppConfig {
            site_url: base_url.clone(),
            deployment: DeploymentKind::Cloud,
            sync_interval_secs: 300,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url,
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let mut breakers = CircuitBreakerRegistry::new();

        // Cycle 1: hits all 4 endpoints; projects & issues succeed, boards (403) & spaces (404) trip breakers.
        let out1 = run_full_sync(&client, &storage, None, &mut breakers).await;
        assert_eq!(out1.attempted_steps, 4);
        assert_eq!(out1.succeeded_steps, 2);
        assert_eq!(out1.skipped_steps, 0);
        assert_eq!(project_hits.load(Ordering::SeqCst), 1);
        assert_eq!(board_hits.load(Ordering::SeqCst), 1);
        assert_eq!(issue_hits.load(Ordering::SeqCst), 1);
        assert_eq!(space_hits.load(Ordering::SeqCst), 1);
        assert_eq!(breakers.open_count(Instant::now()), 2);

        // Cycle 2: boards & spaces are skipped while projects & issues keep syncing cleanly.
        let out2 = run_full_sync(&client, &storage, None, &mut breakers).await;
        assert_eq!(out2.attempted_steps, 2);
        assert_eq!(out2.succeeded_steps, 2);
        assert_eq!(out2.skipped_steps, 2);
        assert_eq!(project_hits.load(Ordering::SeqCst), 2);
        assert_eq!(board_hits.load(Ordering::SeqCst), 1);
        assert_eq!(issue_hits.load(Ordering::SeqCst), 2);
        assert_eq!(space_hits.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn test_sync_engine_shutdown_and_quiet_transient_error_handling() {
        // Offline bootstrap returns Err rather than panicking.
        let offline_cfg = AppConfig {
            site_url: "http://127.0.0.1:1".into(),
            deployment: DeploymentKind::Cloud,
            sync_interval_secs: 300,
            ..Default::default()
        };
        let offline_auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::ApiToken {
                site_url: offline_cfg.site_url.clone(),
                email: "user@example.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let offline_client = AtlassianClient::new(offline_cfg, offline_auth).unwrap();
        let offline_storage = Storage::open_in_memory().unwrap();
        assert!(bootstrap_workspace(&offline_client, &offline_storage)
            .await
            .is_err());

        // Mock server returning transient 503 errors on all endpoints so the test runs
        // deterministically without waiting on Windows loopback WSAECONNREFUSED delays.
        let app = Router::new()
            .route(
                "/rest/api/3/project/search",
                get(|| async { AxumStatus::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/rest/agile/1.0/board",
                get(|| async { AxumStatus::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/rest/api/3/search/jql",
                post(|| async { AxumStatus::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(|| async { AxumStatus::SERVICE_UNAVAILABLE }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            site_url: format!("http://{addr}"),
            deployment: DeploymentKind::Cloud,
            sync_interval_secs: 300,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::ApiToken {
                site_url: cfg.site_url.clone(),
                email: "user@example.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, mut core_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(cfg, client, storage, core_tx);
        assert!(ctrl.request(SyncCommand::RefreshAll));

        let ev1 = tokio::time::timeout(Duration::from_secs(2), core_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, CoreEvent::SyncStarted));

        // Transient failure emits a quiet retry summary rather than an alarming SyncError toast.
        let ev2 = tokio::time::timeout(Duration::from_secs(5), core_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(ev2, CoreEvent::SyncFinished { ref summary } if summary == POLL_RETRY_SUMMARY),
            "expected quiet retry summary, got: {ev2:?}"
        );

        assert!(ctrl.request(SyncCommand::Shutdown));
        server.abort();
    }
}
