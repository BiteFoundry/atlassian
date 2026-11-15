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
}

impl TaskStatus {
    pub fn display(&self) -> String {
        match &self.state {
            TaskState::Running => format!("{}…", self.label),
            TaskState::Done => format!("{} 완료", self.label),
            TaskState::Failed(err) => format!("{} 실패: {err}", self.label),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state, TaskState::Running)
    }
}

#[derive(Debug, Default)]
pub struct TaskBoard {
    tasks: Vec<TaskStatus>,
}

impl TaskBoard {
    pub fn begin(&mut self, label: impl Into<String>) {
        let label = label.into();
        self.tasks.retain(|t| t.label != label);
        self.tasks.push(TaskStatus {
            label,
            state: TaskState::Running,
        });
    }

    pub fn finish(&mut self, label: &str, error: Option<String>) {
        self.tasks.retain(|t| t.label != label);
        self.tasks.push(TaskStatus {
            label: label.to_string(),
            state: match error {
                Some(err) => TaskState::Failed(err),
                None => TaskState::Done,
            },
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

    pub fn first_running(&self) -> Option<&TaskStatus> {
        self.tasks.iter().find(|t| t.running())
    }

    pub fn last(&self) -> Option<&TaskStatus> {
        self.tasks.last()
    }
}
