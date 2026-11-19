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
    sync::{mpsc, Mutex as AsyncMutex, Semaphore},
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

/// Default site identifier used by single-site callers for backwards compatibility.
pub const DEFAULT_SITE_ID: &str = "default";

/// Default bounded concurrency for parallel multi-site synchronization.
pub const DEFAULT_MULTI_SITE_CONCURRENCY: usize = 4;

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

fn normalize_site_id(site_id: &str) -> String {
    let trimmed = site_id.trim();
    if trimmed.is_empty() {
        DEFAULT_SITE_ID.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Composite circuit breaker key scoping endpoint health to a specific Atlassian site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteEndpointKey {
    pub site_id: String,
    pub endpoint: SyncEndpoint,
}

impl SiteEndpointKey {
    pub fn new(site_id: impl Into<String>, endpoint: SyncEndpoint) -> Self {
        Self {
            site_id: normalize_site_id(&site_id.into()),
            endpoint,
        }
    }
}

/// Manages per-site, per-endpoint circuit breakers and per-site auth-expiry cooldown state.
#[derive(Debug, Default, Clone)]
pub struct CircuitBreakerRegistry {
    breakers: HashMap<SiteEndpointKey, EndpointCircuitBreaker>,
    auth_pause_until: HashMap<String, Instant>,
    degraded: HashMap<String, bool>,
}

/// Type alias for multi-site circuit breaker tracking keyed by `(site_id, endpoint)`.
pub type CircuitBreakerMap = CircuitBreakerRegistry;

impl CircuitBreakerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn breaker_mut_key(&mut self, key: SiteEndpointKey) -> &mut EndpointCircuitBreaker {
        let endpoint = key.endpoint;
        self.breakers
            .entry(key)
            .or_insert_with(|| EndpointCircuitBreaker::new(endpoint))
    }

    pub fn is_auth_paused_for_site(&self, site_id: &str, now: Instant) -> bool {
        let site = normalize_site_id(site_id);
        self.auth_pause_until
            .get(&site)
            .is_some_and(|until| now < *until)
    }

    pub fn is_auth_paused(&self, now: Instant) -> bool {
        self.is_auth_paused_for_site(DEFAULT_SITE_ID, now)
    }

    pub fn is_degraded_for_site(&self, site_id: &str) -> bool {
        let site = normalize_site_id(site_id);
        self.degraded.get(&site).copied().unwrap_or(false)
    }

    pub fn is_degraded(&self) -> bool {
        self.is_degraded_for_site(DEFAULT_SITE_ID)
    }

    pub fn mark_degraded_for_site(&mut self, site_id: &str) -> bool {
        let site = normalize_site_id(site_id);
        let entry = self.degraded.entry(site).or_insert(false);
        if !*entry {
            *entry = true;
            true
        } else {
            false
        }
    }

    pub fn mark_degraded(&mut self) -> bool {
        self.mark_degraded_for_site(DEFAULT_SITE_ID)
    }

    pub fn clear_degraded_for_site(&mut self, site_id: &str) -> bool {
        let site = normalize_site_id(site_id);
        if let Some(flag) = self.degraded.get_mut(&site) {
            if *flag {
                *flag = false;
                return true;
            }
        }
        false
    }

    pub fn clear_degraded(&mut self) -> bool {
        self.clear_degraded_for_site(DEFAULT_SITE_ID)
    }

    pub fn state_at_key(&self, key: &SiteEndpointKey, now: Instant) -> CircuitState {
        let site = normalize_site_id(&key.site_id);
        if let Some(&until) = self.auth_pause_until.get(&site) {
            if now < until {
                return CircuitState::Open {
                    reason: CircuitOpenReason::Unauthorized401,
                    cooldown_until: until,
                };
            }
        }
        let normalized_key = SiteEndpointKey {
            site_id: site,
            endpoint: key.endpoint,
        };
        self.breakers
            .get(&normalized_key)
            .map(|b| b.state_at(now))
            .unwrap_or(CircuitState::Closed)
    }

    pub fn state_at_site(
        &self,
        site_id: &str,
        endpoint: SyncEndpoint,
        now: Instant,
    ) -> CircuitState {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.state_at_key(&key, now)
    }

    pub fn state_at(&self, endpoint: SyncEndpoint, now: Instant) -> CircuitState {
        self.state_at_site(DEFAULT_SITE_ID, endpoint, now)
    }

    pub fn should_allow_key(&mut self, key: &SiteEndpointKey, now: Instant) -> bool {
        let normalized = SiteEndpointKey::new(&key.site_id, key.endpoint);
        if self.is_auth_paused_for_site(&normalized.site_id, now) {
            return false;
        }
        self.breaker_mut_key(normalized).should_allow(now)
    }

    pub fn should_allow_site(
        &mut self,
        site_id: &str,
        endpoint: SyncEndpoint,
        now: Instant,
    ) -> bool {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.should_allow_key(&key, now)
    }

    pub fn should_allow(&mut self, endpoint: SyncEndpoint, now: Instant) -> bool {
        self.should_allow_site(DEFAULT_SITE_ID, endpoint, now)
    }

    pub fn record_success_key(&mut self, key: &SiteEndpointKey) -> bool {
        let normalized = SiteEndpointKey::new(&key.site_id, key.endpoint);
        self.auth_pause_until.remove(&normalized.site_id);
        self.breaker_mut_key(normalized).record_success()
    }

    pub fn record_success_site(&mut self, site_id: &str, endpoint: SyncEndpoint) -> bool {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.record_success_key(&key)
    }

    pub fn record_success(&mut self, endpoint: SyncEndpoint) -> bool {
        self.record_success_site(DEFAULT_SITE_ID, endpoint)
    }

    pub fn record_failure_key(
        &mut self,
        key: &SiteEndpointKey,
        err: &anyhow::Error,
        now: Instant,
    ) -> (SyncErrorKind, CircuitTripResult) {
        let kind = classify_sync_error(err);
        let trip = self.record_failure_kind_key(key, kind, now);
        (kind, trip)
    }

    pub fn record_failure_site(
        &mut self,
        site_id: &str,
        endpoint: SyncEndpoint,
        err: &anyhow::Error,
        now: Instant,
    ) -> (SyncErrorKind, CircuitTripResult) {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.record_failure_key(&key, err, now)
    }

    pub fn record_failure(
        &mut self,
        endpoint: SyncEndpoint,
        err: &anyhow::Error,
        now: Instant,
    ) -> (SyncErrorKind, CircuitTripResult) {
        self.record_failure_site(DEFAULT_SITE_ID, endpoint, err, now)
    }

    pub fn record_failure_kind_key(
        &mut self,
        key: &SiteEndpointKey,
        kind: SyncErrorKind,
        now: Instant,
    ) -> CircuitTripResult {
        let normalized = SiteEndpointKey::new(&key.site_id, key.endpoint);
        let site_id = normalized.site_id.clone();
        let trip = self.breaker_mut_key(normalized).record_failure(kind, now);
        if kind == SyncErrorKind::Unauthorized401 {
            self.auth_pause_until.insert(site_id, now + trip.cooldown);
        }
        trip
    }

    pub fn record_failure_kind_site(
        &mut self,
        site_id: &str,
        endpoint: SyncEndpoint,
        kind: SyncErrorKind,
        now: Instant,
    ) -> CircuitTripResult {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.record_failure_kind_key(&key, kind, now)
    }

    pub fn record_failure_kind(
        &mut self,
        endpoint: SyncEndpoint,
        kind: SyncErrorKind,
        now: Instant,
    ) -> CircuitTripResult {
        self.record_failure_kind_site(DEFAULT_SITE_ID, endpoint, kind, now)
    }

    pub fn reset_site_endpoint(&mut self, site_id: &str, endpoint: SyncEndpoint) {
        let key = SiteEndpointKey::new(site_id, endpoint);
        self.auth_pause_until.remove(&key.site_id);
        if let Some(breaker) = self.breakers.get_mut(&key) {
            breaker.reset();
        }
    }

    pub fn reset_endpoint(&mut self, endpoint: SyncEndpoint) {
        self.reset_site_endpoint(DEFAULT_SITE_ID, endpoint);
    }

    pub fn reset_site(&mut self, site_id: &str) {
        let site = normalize_site_id(site_id);
        self.auth_pause_until.remove(&site);
        self.degraded.remove(&site);
        for (key, breaker) in &mut self.breakers {
            if key.site_id == site {
                breaker.reset();
            }
        }
    }

    pub fn reset_all(&mut self) {
        self.auth_pause_until.clear();
        self.degraded.clear();
        for breaker in self.breakers.values_mut() {
            breaker.reset();
        }
    }

    pub fn open_count_for_site(&self, site_id: &str, now: Instant) -> usize {
        let site = normalize_site_id(site_id);
        self.breakers
            .iter()
            .filter(|(k, b)| {
                k.site_id == site && matches!(b.state_at(now), CircuitState::Open { .. })
            })
            .count()
    }

    pub fn has_open_circuit_for_site(&self, site_id: &str, now: Instant) -> bool {
        self.is_auth_paused_for_site(site_id, now) || self.open_count_for_site(site_id, now) > 0
    }

    pub fn open_count(&self, now: Instant) -> usize {
        self.breakers
            .values()
            .filter(|b| matches!(b.state_at(now), CircuitState::Open { .. }))
            .count()
    }

    fn extract_site(&self, site_id: &str) -> Self {
        let site = normalize_site_id(site_id);
        let breakers = self
            .breakers
            .iter()
            .filter(|(k, _)| k.site_id == site)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut auth_pause_until = HashMap::new();
        if let Some(&until) = self.auth_pause_until.get(&site) {
            auth_pause_until.insert(site.clone(), until);
        }
        let mut degraded = HashMap::new();
        if let Some(&flag) = self.degraded.get(&site) {
            degraded.insert(site, flag);
        }
        Self {
            breakers,
            auth_pause_until,
            degraded,
        }
    }

    fn merge_site(&mut self, site_id: &str, other: Self) {
        let site = normalize_site_id(site_id);
        self.breakers.retain(|k, _| k.site_id != site);
        for (k, v) in other.breakers {
            if k.site_id == site {
                self.breakers.insert(k, v);
            }
        }
        if let Some(&until) = other.auth_pause_until.get(&site) {
            self.auth_pause_until.insert(site.clone(), until);
        } else {
            self.auth_pause_until.remove(&site);
        }
        if let Some(&flag) = other.degraded.get(&site) {
            self.degraded.insert(site, flag);
        } else {
            self.degraded.remove(&site);
        }
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
    RefreshSite {
        site_id: String,
    },
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

/// Target descriptor for a single Atlassian site participating in federated multi-site sync.
#[derive(Clone)]
pub struct SiteSyncTarget {
    pub site_id: String,
    pub site_name: String,
    pub client: AtlassianClient,
}

/// Per-site outcome emitted by `run_multi_site_sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteSyncResult {
    pub site_id: String,
    pub site_name: String,
    pub site_url: String,
    pub succeeded: bool,
    pub attempted_steps: usize,
    pub succeeded_steps: usize,
    pub skipped_steps: usize,
    pub projects: usize,
    pub boards: usize,
    pub issues: usize,
    pub spaces: usize,
    pub summary: String,
    pub error: Option<String>,
    pub circuit_open: bool,
}

/// Aggregate outcome returned by `run_multi_site_sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiSiteSyncOutcome {
    pub total_sites: usize,
    pub succeeded_sites: usize,
    pub per_site: Vec<SiteSyncResult>,
    pub summary: String,
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
                        SyncCommand::RefreshSite { site_id: _ } => {
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

/// Synchronizes multiple Atlassian sites in parallel with bounded concurrency (`Semaphore`)
/// and strict per-site circuit breaker & failure isolation.
pub async fn run_multi_site_sync(
    targets: &[SiteSyncTarget],
    storage: &Storage,
    breakers: &mut CircuitBreakerMap,
    max_concurrency: usize,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> MultiSiteSyncOutcome {
    if targets.is_empty() {
        return MultiSiteSyncOutcome {
            total_sites: 0,
            succeeded_sites: 0,
            per_site: Vec::new(),
            summary: "No sites configured to sync".to_string(),
        };
    }

    let concurrency = if max_concurrency == 0 {
        DEFAULT_MULTI_SITE_CONCURRENCY
    } else {
        max_concurrency
    };
    let semaphore = Arc::new(Semaphore::new(concurrency));

    let mut per_site_breakers: HashMap<String, Arc<AsyncMutex<CircuitBreakerMap>>> =
        HashMap::new();
    for target in targets {
        let site_id = normalize_site_id(&target.site_id);
        per_site_breakers.entry(site_id.clone()).or_insert_with(|| {
            Arc::new(AsyncMutex::new(breakers.extract_site(&site_id)))
        });
    }

    let mut futs = Vec::with_capacity(targets.len());
    for target in targets {
        let site_id = normalize_site_id(&target.site_id);
        let site_name = if target.site_name.trim().is_empty() {
            site_id.clone()
        } else {
            target.site_name.clone()
        };
        let site_url = target.client.site_url().to_string();
        let sem = Arc::clone(&semaphore);
        let site_breaker_lock = Arc::clone(
            per_site_breakers
                .get(&site_id)
                .expect("per-site breaker map initialized"),
        );

        futs.push(async move {
            let _permit = sem.acquire_owned().await.expect("sync semaphore closed");
            let mut site_breakers = site_breaker_lock.lock().await;
            let outcome = run_full_sync_for_site(
                &site_id,
                &target.client,
                storage,
                core_tx,
                &mut site_breakers,
            )
            .await;

            let circuit_open = site_breakers.has_open_circuit_for_site(&site_id, Instant::now());
            let succeeded = outcome.succeeded_steps > 0;
            let error = if succeeded {
                None
            } else {
                outcome.first_error.as_ref().map(|e| format!("{e:#}"))
            };
            if succeeded {
                site_breakers.clear_degraded_for_site(&site_id);
            } else if outcome.first_error.is_some() {
                site_breakers.mark_degraded_for_site(&site_id);
            }

            SiteSyncResult {
                site_id,
                site_name,
                site_url,
                succeeded,
                attempted_steps: outcome.attempted_steps,
                succeeded_steps: outcome.succeeded_steps,
                skipped_steps: outcome.skipped_steps,
                projects: outcome.project_count,
                boards: outcome.board_count,
                issues: outcome.issue_count,
                spaces: outcome.space_count,
                summary: outcome.summary,
                error,
                circuit_open,
            }
        });
    }

    let per_site = futures::future::join_all(futs).await;

    for (site_id, lock) in per_site_breakers {
        let updated = match Arc::try_unwrap(lock) {
            Ok(mutex) => mutex.into_inner(),
            Err(arc) => arc.lock().await.clone(),
        };
        breakers.merge_site(&site_id, updated);
    }

    let total_sites = per_site.len();
    let succeeded_sites = per_site.iter().filter(|r| r.succeeded).count();
    let total_projects: usize = per_site.iter().map(|r| r.projects).sum();
    let total_boards: usize = per_site.iter().map(|r| r.boards).sum();
    let total_issues: usize = per_site.iter().map(|r| r.issues).sum();
    let total_spaces: usize = per_site.iter().map(|r| r.spaces).sum();
    let summary = format!(
        "Synced {succeeded_sites}/{total_sites} sites ({total_projects} projects, {total_boards} boards, {total_issues} issues, {total_spaces} spaces)"
    );

    MultiSiteSyncOutcome {
        total_sites,
        succeeded_sites,
        per_site,
        summary,
    }
}

/// Spawns a federated multi-site background sync engine that polls all configured sites
/// concurrently with bounded parallelism and per-site circuit breaker isolation.
pub fn spawn_multi_site_sync_engine(
    config: AppConfig,
    targets: Vec<SiteSyncTarget>,
    storage: Storage,
    max_concurrency: usize,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    spawn_multi_site_sync_engine_with_status(
        config,
        targets,
        storage,
        max_concurrency,
        core_tx,
        None,
    )
}

/// Spawns a federated multi-site sync engine and optionally streams per-site sync outcomes
/// to `site_results_tx` (used by the background daemon to maintain `DaemonStatus.sites`).
pub fn spawn_multi_site_sync_engine_with_status(
    config: AppConfig,
    targets: Vec<SiteSyncTarget>,
    storage: Storage,
    max_concurrency: usize,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
    site_results_tx: Option<mpsc::UnboundedSender<Vec<SiteSyncResult>>>,
) -> SyncController {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let activity = Arc::new(SyncSharedActivity::new());
    let engine_activity = Arc::clone(&activity);

    tokio::spawn(async move {
        let secs = config.sync_interval_secs.max(5);
        let period = Duration::from_secs(secs);
        let mut ticker = interval_at(TokioInstant::now() + period, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut breakers = CircuitBreakerMap::new();
        let concurrency = if max_concurrency == 0 {
            DEFAULT_MULTI_SITE_CONCURRENCY
        } else {
            max_concurrency
        };

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
                            let outcome = run_multi_site_sync(
                                &targets,
                                &storage,
                                &mut breakers,
                                concurrency,
                                Some(&core_tx),
                            )
                            .await;
                            if let Some(ref results_tx) = site_results_tx {
                                let _ = results_tx.send(outcome.per_site.clone());
                            }
                            emit_multi_site_sync_events(&core_tx, outcome, false);
                        }
                        SyncCommand::RefreshSite { site_id } => {
                            let target_key = site_id.trim();
                            let matched = targets.iter().find(|t| {
                                t.site_id.eq_ignore_ascii_case(target_key)
                                    || t.site_name.eq_ignore_ascii_case(target_key)
                            });
                            let Some(target) = matched else {
                                let _ = core_tx.send(CoreEvent::SyncError(format!(
                                    "Unknown site '{site_id}' for sync refresh"
                                )));
                                continue;
                            };
                            breakers.reset_site(&target.site_id);
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            let outcome = run_multi_site_sync(
                                std::slice::from_ref(target),
                                &storage,
                                &mut breakers,
                                1,
                                Some(&core_tx),
                            )
                            .await;
                            if let Some(ref results_tx) = site_results_tx {
                                let _ = results_tx.send(outcome.per_site.clone());
                            }
                            emit_multi_site_sync_events(&core_tx, outcome, false);
                        }
                        SyncCommand::RefreshBoard { board_id, sprint_id } => {
                            let Some(primary) = targets.first() else {
                                let _ = core_tx.send(CoreEvent::SyncError(
                                    "No sites configured to sync board".to_string(),
                                ));
                                continue;
                            };
                            breakers.reset_site_endpoint(
                                &primary.site_id,
                                SyncEndpoint::JiraAgileBoards,
                            );
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_board(
                                &primary.client,
                                &storage,
                                board_id,
                                sprint_id,
                                Some(&core_tx),
                            )
                            .await
                            {
                                Ok(_) => {
                                    breakers.record_success_site(
                                        &primary.site_id,
                                        SyncEndpoint::JiraAgileBoards,
                                    );
                                    breakers.clear_degraded_for_site(&primary.site_id);
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!("Synced board #{board_id}"),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure_site(
                                        &primary.site_id,
                                        SyncEndpoint::JiraAgileBoards,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded_for_site(&primary.site_id);
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
                            let Some(primary) = targets.first() else {
                                let _ = core_tx.send(CoreEvent::SyncError(
                                    "No sites configured to sync project issues".to_string(),
                                ));
                                continue;
                            };
                            breakers.reset_site_endpoint(
                                &primary.site_id,
                                SyncEndpoint::JiraIssues,
                            );
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_project_issues(
                                &primary.client,
                                &storage,
                                &project_key,
                                Some(&core_tx),
                            )
                            .await
                            {
                                Ok(count) => {
                                    breakers.record_success_site(
                                        &primary.site_id,
                                        SyncEndpoint::JiraIssues,
                                    );
                                    breakers.clear_degraded_for_site(&primary.site_id);
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!(
                                            "Synced {count} issues for project {project_key}"
                                        ),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure_site(
                                        &primary.site_id,
                                        SyncEndpoint::JiraIssues,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded_for_site(&primary.site_id);
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
                            let Some(primary) = targets.first() else {
                                let _ = core_tx.send(CoreEvent::SyncError(
                                    "No sites configured to sync space".to_string(),
                                ));
                                continue;
                            };
                            breakers.reset_site_endpoint(
                                &primary.site_id,
                                SyncEndpoint::ConfluencePages,
                            );
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_space_pages(
                                &primary.client,
                                &storage,
                                &space_key,
                                Some(&core_tx),
                            )
                            .await
                            {
                                Ok(count) => {
                                    breakers.record_success_site(
                                        &primary.site_id,
                                        SyncEndpoint::ConfluencePages,
                                    );
                                    breakers.clear_degraded_for_site(&primary.site_id);
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!(
                                            "Synced {count} pages for space {space_key}"
                                        ),
                                    });
                                }
                                Err(err) => {
                                    let (kind, _) = breakers.record_failure_site(
                                        &primary.site_id,
                                        SyncEndpoint::ConfluencePages,
                                        &err,
                                        Instant::now(),
                                    );
                                    if kind.is_quiet_transient() {
                                        breakers.mark_degraded_for_site(&primary.site_id);
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
                    if targets.is_empty() {
                        continue;
                    }
                    let outcome = run_multi_site_sync(
                        &targets,
                        &storage,
                        &mut breakers,
                        concurrency,
                        Some(&core_tx),
                    )
                    .await;
                    if let Some(ref results_tx) = site_results_tx {
                        let _ = results_tx.send(outcome.per_site.clone());
                    }
                    let total_attempted: usize =
                        outcome.per_site.iter().map(|r| r.attempted_steps).sum();
                    if total_attempted == 0 {
                        continue;
                    }
                    emit_multi_site_sync_events(&core_tx, outcome, true);
                }
            }
        }
    });

    SyncController { tx, activity }
}

fn emit_multi_site_sync_events(
    core_tx: &mpsc::UnboundedSender<CoreEvent>,
    outcome: MultiSiteSyncOutcome,
    quiet_background: bool,
) {
    if outcome.succeeded_sites > 0 {
        let _ = core_tx.send(CoreEvent::SyncFinished {
            summary: outcome.summary,
        });
        return;
    }

    let first_err = outcome
        .per_site
        .iter()
        .find_map(|r| r.error.as_ref().map(|e| (r.site_id.as_str(), e.as_str())));

    if let Some((site_id, err_msg)) = first_err {
        let kind = classify_sync_error(&anyhow::anyhow!("{err_msg}"));
        if quiet_background || kind.is_quiet_transient() {
            let _ = core_tx.send(CoreEvent::SyncFinished {
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        } else {
            let _ = core_tx.send(CoreEvent::SyncError(format!(
                "Sync failed for site '{site_id}': {err_msg}"
            )));
        }
    } else if !quiet_background {
        let _ = core_tx.send(CoreEvent::SyncFinished {
            summary: outcome.summary,
        });
    }
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
    project_count: usize,
    board_count: usize,
    issue_count: usize,
    space_count: usize,
    attempted_steps: usize,
    succeeded_steps: usize,
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
    run_full_sync_for_site(DEFAULT_SITE_ID, client, storage, core_tx, breakers).await
}

async fn run_full_sync_for_site(
    site_id: &str,
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
    if breakers.should_allow_site(site_id, SyncEndpoint::JiraProjects, Instant::now()) {
        attempted_steps += 1;
        match client.list_projects().await {
            Ok(projects) => {
                breakers.record_success_site(site_id, SyncEndpoint::JiraProjects);
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
                let (kind, _) = breakers.record_failure_site(
                    site_id,
                    SyncEndpoint::JiraProjects,
                    &err,
                    Instant::now(),
                );
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 2. Jira Agile boards + top/default board columns, sprints, active sprint issues
    if breakers.should_allow_site(site_id, SyncEndpoint::JiraAgileBoards, Instant::now()) {
        attempted_steps += 1;
        match client.list_boards(None).await {
            Ok(mut boards) => {
                breakers.record_success_site(site_id, SyncEndpoint::JiraAgileBoards);
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
                let (kind, _) = breakers.record_failure_site(
                    site_id,
                    SyncEndpoint::JiraAgileBoards,
                    &err,
                    Instant::now(),
                );
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 3. My open Jira issues
    if breakers.should_allow_site(site_id, SyncEndpoint::JiraIssues, Instant::now()) {
        attempted_steps += 1;
        match client
            .search_issues_jql(AtlassianClient::my_open_issues_jql(), 50, None, None)
            .await
        {
            Ok(page) => {
                breakers.record_success_site(site_id, SyncEndpoint::JiraIssues);
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
                let (kind, _) = breakers.record_failure_site(
                    site_id,
                    SyncEndpoint::JiraIssues,
                    &err,
                    Instant::now(),
                );
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    // 4. Confluence spaces + default/first space root pages
    if breakers.should_allow_site(site_id, SyncEndpoint::ConfluenceSpaces, Instant::now()) {
        attempted_steps += 1;
        match client.list_spaces(50).await {
            Ok(spaces) => {
                breakers.record_success_site(site_id, SyncEndpoint::ConfluenceSpaces);
                succeeded_steps += 1;
                space_count = spaces.len();
                if let Err(err) = storage.upsert_spaces(&spaces) {
                    record_err(err, SyncErrorKind::Transient);
                } else if let Some(tx) = core_tx {
                    let stored = storage.list_spaces().unwrap_or_else(|_| spaces.clone());
                    let _ = tx.send(CoreEvent::SpacesUpdated(stored));
                }
                if let Some(target_space) = select_default_space(&spaces, client.config()) {
                    if breakers.should_allow_site(
                        site_id,
                        SyncEndpoint::ConfluencePages,
                        Instant::now(),
                    ) {
                        match sync_space_pages(client, storage, &target_space.key, core_tx).await {
                            Ok(_) => {
                                breakers.record_success_site(site_id, SyncEndpoint::ConfluencePages);
                            }
                            Err(err) => {
                                breakers.record_failure_site(
                                    site_id,
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
                let (kind, _) = breakers.record_failure_site(
                    site_id,
                    SyncEndpoint::ConfluenceSpaces,
                    &err,
                    Instant::now(),
                );
                record_err(err, kind);
            }
        }
    } else {
        skipped_steps += 1;
    }

    let summary = format_sync_summary(project_count, board_count, issue_count, space_count);
    FullSyncOutcome {
        summary,
        project_count,
        board_count,
        issue_count,
        space_count,
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

    fn make_mock_client(base_url: &str) -> AtlassianClient {
        let cfg = AppConfig {
            site_url: base_url.to_string(),
            deployment: DeploymentKind::Cloud,
            sync_interval_secs: 300,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.to_string(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        AtlassianClient::new(cfg, auth).unwrap()
    }

    async fn spawn_healthy_mock_site(
        project_key: &'static str,
        space_key: &'static str,
        delay_ms: u64,
        in_flight: Option<Arc<AtomicUsize>>,
        peak_in_flight: Option<Arc<AtomicUsize>>,
        hits: Arc<AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let p_hits = hits.clone();
        let p_in = in_flight.clone();
        let p_peak = peak_in_flight.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/project/search",
                get(move || {
                    let hits = p_hits.clone();
                    let in_f = p_in.clone();
                    let peak = p_peak.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if let (Some(cur_ref), Some(peak_ref)) = (&in_f, &peak) {
                            let cur = cur_ref.fetch_add(1, Ordering::SeqCst) + 1;
                            let mut prev = peak_ref.load(Ordering::SeqCst);
                            while cur > prev {
                                match peak_ref.compare_exchange(
                                    prev,
                                    cur,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => prev = actual,
                                }
                            }
                        }
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        if let Some(cur_ref) = &in_f {
                            cur_ref.fetch_sub(1, Ordering::SeqCst);
                        }
                        Json(json!({
                            "values": [{
                                "id": format!("id-{project_key}"),
                                "key": project_key,
                                "name": format!("{project_key} Project"),
                                "projectTypeKey": "software"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board",
                get(|| async { Json(json!({ "values": [] })) }),
            )
            .route(
                "/rest/api/3/search/jql",
                post(|| async { Json(json!({ "issues": [] })) }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(move || async move {
                    Json(json!({
                        "results": [{
                            "id": format!("sp-{space_key}"),
                            "key": space_key,
                            "name": format!("{space_key} Space"),
                            "type": "global"
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/spaces/{space_id}/pages",
                get(|| async { Json(json!({ "results": [] })) }),
            )
            .route(
                "/wiki/rest/api/space/{space_key}/content/page",
                get(|| async { Json(json!({ "results": [] })) }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    async fn spawn_failing_mock_site(
        status: AxumStatus,
        hits: Arc<AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let h1 = hits.clone();
        let h2 = hits.clone();
        let h3 = hits.clone();
        let h4 = hits.clone();
        let app = Router::new()
            .route(
                "/rest/api/3/project/search",
                get(move || {
                    let h = h1.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        status
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board",
                get(move || {
                    let h = h2.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        status
                    }
                }),
            )
            .route(
                "/rest/api/3/search/jql",
                post(move || {
                    let h = h3.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        status
                    }
                }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(move || {
                    let h = h4.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        status
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn test_per_site_circuit_breaker_isolation() {
        let now = Instant::now();
        let mut breakers = CircuitBreakerMap::new();

        // Site A encounters 403 on JiraAgileBoards and 401 on JiraProjects.
        let key_a_boards = SiteEndpointKey::new("acme-cloud", SyncEndpoint::JiraAgileBoards);
        let key_b_boards = SiteEndpointKey::new("corp-dc", SyncEndpoint::JiraAgileBoards);

        breakers.record_failure_kind_key(&key_a_boards, SyncErrorKind::Forbidden403, now);
        assert!(!breakers.should_allow_key(&key_a_boards, now));
        assert!(matches!(
            breakers.state_at_key(&key_a_boards, now),
            CircuitState::Open {
                reason: CircuitOpenReason::Forbidden403,
                ..
            }
        ));

        // Site B MUST remain completely unaffected.
        assert!(breakers.should_allow_key(&key_b_boards, now));
        assert_eq!(breakers.state_at_key(&key_b_boards, now), CircuitState::Closed);
        assert_eq!(breakers.open_count_for_site("acme-cloud", now), 1);
        assert_eq!(breakers.open_count_for_site("corp-dc", now), 0);
        assert!(breakers.has_open_circuit_for_site("acme-cloud", now));
        assert!(!breakers.has_open_circuit_for_site("corp-dc", now));

        // Site A encountering 401 pauses all endpoints for Site A ONLY, never Site B.
        breakers.record_failure_kind_site(
            "acme-cloud",
            SyncEndpoint::JiraProjects,
            SyncErrorKind::Unauthorized401,
            now,
        );
        assert!(breakers.is_auth_paused_for_site("acme-cloud", now));
        assert!(!breakers.should_allow_site("acme-cloud", SyncEndpoint::ConfluenceSpaces, now));
        assert!(!breakers.is_auth_paused_for_site("corp-dc", now));
        assert!(breakers.should_allow_site("corp-dc", SyncEndpoint::JiraProjects, now));
        assert!(breakers.should_allow_site("corp-dc", SyncEndpoint::ConfluenceSpaces, now));

        // Site B 429 rate-limit is isolated to Site B, and resetting Site A preserves Site B state.
        breakers.record_failure_kind_site(
            "corp-dc",
            SyncEndpoint::JiraIssues,
            SyncErrorKind::RateLimited429 {
                retry_after_secs: Some(30),
            },
            now,
        );
        breakers.reset_site("acme-cloud");
        assert!(!breakers.has_open_circuit_for_site("acme-cloud", now));
        assert!(breakers.should_allow_key(&key_a_boards, now));
        assert!(!breakers.should_allow_site("corp-dc", SyncEndpoint::JiraIssues, now));
    }

    #[tokio::test]
    async fn test_run_multi_site_sync_partial_failure_resilience_and_bounded_concurrency() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let hits_a = Arc::new(AtomicUsize::new(0));
        let hits_b = Arc::new(AtomicUsize::new(0));
        let hits_c = Arc::new(AtomicUsize::new(0));
        let hits_bad = Arc::new(AtomicUsize::new(0));

        let (url_a, srv_a) = spawn_healthy_mock_site(
            "ACME",
            "DOCS",
            60,
            Some(in_flight.clone()),
            Some(peak_in_flight.clone()),
            hits_a.clone(),
        )
        .await;
        let (url_b, srv_b) = spawn_healthy_mock_site(
            "SIDE",
            "WIKI",
            60,
            Some(in_flight.clone()),
            Some(peak_in_flight.clone()),
            hits_b.clone(),
        )
        .await;
        let (url_c, srv_c) = spawn_healthy_mock_site(
            "PART",
            "KB",
            60,
            Some(in_flight.clone()),
            Some(peak_in_flight.clone()),
            hits_c.clone(),
        )
        .await;
        let (url_bad, srv_bad) =
            spawn_failing_mock_site(AxumStatus::FORBIDDEN, hits_bad.clone()).await;

        let targets = vec![
            SiteSyncTarget {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                client: make_mock_client(&url_a),
            },
            SiteSyncTarget {
                site_id: "offline-dc".into(),
                site_name: "Offline DC".into(),
                client: make_mock_client(&url_bad),
            },
            SiteSyncTarget {
                site_id: "side-cloud".into(),
                site_name: "Side Cloud".into(),
                client: make_mock_client(&url_b),
            },
            SiteSyncTarget {
                site_id: "partner-cloud".into(),
                site_name: "Partner Cloud".into(),
                client: make_mock_client(&url_c),
            },
        ];

        let storage = Storage::open_in_memory().unwrap();
        let mut breakers = CircuitBreakerMap::new();

        // Cycle 1: max_concurrency = 2 across 4 sites; 1 failing site does not block 3 healthy sites.
        let outcome1 = run_multi_site_sync(&targets, &storage, &mut breakers, 2, None).await;
        assert_eq!(outcome1.total_sites, 4);
        assert_eq!(outcome1.succeeded_sites, 3);
        assert_eq!(outcome1.per_site.len(), 4);
        assert!(outcome1.summary.contains("3/4 sites"));
        assert!(
            peak_in_flight.load(Ordering::SeqCst) <= 2,
            "bounded semaphore must cap concurrency at 2, observed {}",
            peak_in_flight.load(Ordering::SeqCst)
        );

        let bad_res = outcome1
            .per_site
            .iter()
            .find(|r| r.site_id == "offline-dc")
            .unwrap();
        assert!(!bad_res.succeeded);
        assert!(bad_res.circuit_open);
        assert!(bad_res.error.is_some());

        for healthy_id in ["acme-cloud", "side-cloud", "partner-cloud"] {
            let res = outcome1
                .per_site
                .iter()
                .find(|r| r.site_id == healthy_id)
                .unwrap();
            assert!(res.succeeded, "site {healthy_id} should succeed");
            assert!(!res.circuit_open);
            assert!(res.error.is_none());
        }

        // Cycle 2: offline-dc endpoints are now in cooldown and skipped, while healthy sites keep syncing.
        let bad_hits_after_cycle1 = hits_bad.load(Ordering::SeqCst);
        let outcome2 = run_multi_site_sync(&targets, &storage, &mut breakers, 2, None).await;
        assert_eq!(outcome2.succeeded_sites, 3);
        assert_eq!(hits_bad.load(Ordering::SeqCst), bad_hits_after_cycle1);
        assert_eq!(hits_a.load(Ordering::SeqCst), 2);
        assert_eq!(hits_b.load(Ordering::SeqCst), 2);
        assert_eq!(hits_c.load(Ordering::SeqCst), 2);

        let bad_res2 = outcome2
            .per_site
            .iter()
            .find(|r| r.site_id == "offline-dc")
            .unwrap();
        assert_eq!(bad_res2.attempted_steps, 0);
        assert_eq!(bad_res2.skipped_steps, 4);

        srv_a.abort();
        srv_b.abort();
        srv_c.abort();
        srv_bad.abort();
    }

    #[tokio::test]
    async fn test_spawn_multi_site_sync_engine_refresh_site_command() {
        let hits_a = Arc::new(AtomicUsize::new(0));
        let hits_b = Arc::new(AtomicUsize::new(0));
        let (url_a, srv_a) =
            spawn_healthy_mock_site("ACME", "DOCS", 0, None, None, hits_a.clone()).await;
        let (url_b, srv_b) =
            spawn_healthy_mock_site("CORP", "WIKI", 0, None, None, hits_b.clone()).await;

        let targets = vec![
            SiteSyncTarget {
                site_id: "acme-cloud".into(),
                site_name: "Acme Cloud".into(),
                client: make_mock_client(&url_a),
            },
            SiteSyncTarget {
                site_id: "corp-dc".into(),
                site_name: "Corp DC".into(),
                client: make_mock_client(&url_b),
            },
        ];

        let cfg = AppConfig {
            sync_interval_secs: 300,
            ..Default::default()
        };
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, mut core_rx) = mpsc::unbounded_channel();
        let (site_tx, mut site_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_multi_site_sync_engine_with_status(
            cfg,
            targets,
            storage,
            DEFAULT_MULTI_SITE_CONCURRENCY,
            core_tx,
            Some(site_tx),
        );

        // Request refresh for only corp-dc.
        assert!(ctrl.request(SyncCommand::RefreshSite {
            site_id: "corp-dc".into(),
        }));

        let results = tokio::time::timeout(Duration::from_secs(3), site_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].site_id, "corp-dc");
        assert!(results[0].succeeded);
        assert_eq!(hits_a.load(Ordering::SeqCst), 0);
        assert_eq!(hits_b.load(Ordering::SeqCst), 1);

        // Drain until SyncFinished arrives.
        let mut finished = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(200), core_rx.recv()).await
            {
                if matches!(ev, CoreEvent::SyncFinished { .. }) {
                    finished = true;
                    break;
                }
            }
        }
        assert!(finished, "expected SyncFinished after RefreshSite");

        assert!(ctrl.request(SyncCommand::Shutdown));
        srv_a.abort();
        srv_b.abort();
    }
}
