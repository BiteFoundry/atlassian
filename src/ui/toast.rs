//! Transient toast notifications and background task status tracker.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub created: Instant,
    pub ttl: Duration,
}

impl Toast {
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Info)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Success)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Error)
    }

    fn new(message: impl Into<String>, kind: ToastKind) -> Self {
        Self {
            message: message.into(),
            kind,
            created: Instant::now(),
            ttl: Duration::from_secs(4),
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created) >= self.ttl
    }
}

#[derive(Debug, Default)]
pub struct ToastQueue {
    toasts: VecDeque<Toast>,
    max: usize,
}

impl ToastQueue {
    pub fn new() -> Self {
        Self {
            toasts: VecDeque::new(),
            max: 4,
        }
    }

    pub fn push(&mut self, toast: Toast) {
        if self.max == 0 {
            self.max = 4;
        }
        if let Some(existing) = self
            .toasts
            .iter_mut()
            .find(|t| t.kind == toast.kind && t.message == toast.message)
        {
            existing.created = Instant::now();
            return;
        }
        self.toasts.push_back(toast);
        while self.toasts.len() > self.max {
            self.toasts.pop_front();
        }
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Toast::info(message));
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.push(Toast::success(message));
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.push(Toast::error(message));
    }

    pub fn current(&self) -> Option<&Toast> {
        self.toasts.back()
    }

    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub fn tick(&mut self, now: Instant) {
        while self.toasts.front().is_some_and(|t| t.expired(now)) {
            self.toasts.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.toasts.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TaskStatus {
    pub label: String,
    pub state: TaskState,
    pub started: Instant,
}

/// A running task older than this is treated as leaked: no request the TUI
/// starts has a timeout above 30 s, so a task still "running" after two
/// minutes lost its finishing message and must not keep the spinner alive.
pub const STALE_TASK_AFTER: Duration = Duration::from_secs(120);

impl TaskStatus {
    pub fn display(&self) -> String {
        match &self.state {
            TaskState::Running => format!("{}…", self.label),
            TaskState::Done => format!("{} done", self.label),
            TaskState::Failed(err) => format!("{} failed: {err}", self.label),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state, TaskState::Running)
    }

    fn is_stale_running(&self, now: Instant) -> bool {
        self.running() && now.saturating_duration_since(self.started) >= STALE_TASK_AFTER
    }
}

#[derive(Debug, Default)]
pub struct TaskBoard {
    tasks: Vec<TaskStatus>,
}

impl TaskBoard {
    pub fn begin(&mut self, label: impl Into<String>) {
        let label = label.into();
        let now = Instant::now();
        self.purge_stale(now);
        self.tasks.retain(|t| t.label != label);
        self.tasks.push(TaskStatus {
            label,
            state: TaskState::Running,
            started: now,
        });
    }

    pub fn finish(&mut self, label: &str, error: Option<String>) {
        let now = Instant::now();
        self.purge_stale(now);
        let started = self
            .tasks
            .iter()
            .find(|t| t.label == label)
            .map(|t| t.started)
            .unwrap_or(now);
        self.tasks.retain(|t| t.label != label);
        self.tasks.push(TaskStatus {
            label: label.to_string(),
            state: match error {
                Some(err) => TaskState::Failed(err),
                None => TaskState::Done,
            },
            started,
        });
        let mut done = 0usize;
        let mut keep = Vec::with_capacity(self.tasks.len());
        for t in self.tasks.drain(..).rev() {
            if t.running() {
                keep.push(t);
            } else if done < 3 {
                done += 1;
                keep.push(t);
            }
        }
        keep.reverse();
        self.tasks = keep;
    }

    pub fn running(&self) -> impl Iterator<Item = &TaskStatus> {
        let now = Instant::now();
        self.tasks
            .iter()
            .filter(move |t| t.running() && !t.is_stale_running(now))
    }

    fn purge_stale(&mut self, now: Instant) {
        self.tasks.retain(|t| !t.is_stale_running(now));
    }

    pub fn first_running(&self) -> Option<&TaskStatus> {
        self.running().next()
    }

    pub fn last(&self) -> Option<&TaskStatus> {
        self.tasks.last()
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_deduplicate_and_expire_cleanly() {
        let mut queue = ToastQueue::new();
        let older = Instant::now() - Duration::from_secs(3);
        queue.push(Toast {
            message: "Sync failed".to_string(),
            kind: ToastKind::Error,
            created: older,
            ttl: Duration::from_secs(4),
        });
        assert_eq!(queue.len(), 1);
        queue.error("Sync failed");
        assert_eq!(queue.len(), 1);
        assert!(queue.current().unwrap().created > older);

        queue.info("Sync failed");
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn task_board_formats_english_status() {
        let mut board = TaskBoard::default();
        board.begin("Sync");
        assert_eq!(board.first_running().unwrap().display(), "Sync…");
        board.finish("Sync", None);
        assert_eq!(board.last().unwrap().display(), "Sync done");
        board.finish("Sync", Some("timeout".into()));
        assert_eq!(board.last().unwrap().display(), "Sync failed: timeout");
    }

    #[test]
    fn task_board_expires_stale_running_tasks() {
        let mut board = TaskBoard::default();
        let old = Instant::now()
            .checked_sub(STALE_TASK_AFTER + Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        board.tasks.push(TaskStatus {
            label: "Child pages: Leaked".into(),
            state: TaskState::Running,
            started: old,
        });
        // A leaked task no longer counts as running work.
        assert!(board.first_running().is_none());

        // Fresh work is still reported, and the next begin purges the leak.
        board.begin("Sync");
        assert_eq!(board.first_running().unwrap().label, "Sync");
        assert_eq!(board.tasks.len(), 1);

        // finish also purges, and a stale Done entry is kept (only running expires).
        board.tasks.push(TaskStatus {
            label: "Old leak".into(),
            state: TaskState::Running,
            started: old,
        });
        board.finish("Sync", None);
        assert!(board.tasks.iter().all(|t| t.label != "Old leak"));
        assert_eq!(board.last().unwrap().display(), "Sync done");
    }
}
