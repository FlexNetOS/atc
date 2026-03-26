//! Dispatch queue: the universal buffer between selection strategies and the daemon drain loop.
//!
//! Every write path (CLI `atc enqueue`, daemon sources, future ACP endpoint) goes through
//! `DispatchQueue::enqueue()`, which provides dedup and priority ordering.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Input type for a queue item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueInputType {
    Task,
    Template,
    Prompt,
}

impl QueueInputType {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueInputType::Task => "task",
            QueueInputType::Template => "template",
            QueueInputType::Prompt => "prompt",
        }
    }
}

impl std::str::FromStr for QueueInputType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task" => Ok(QueueInputType::Task),
            "template" => Ok(QueueInputType::Template),
            "prompt" => Ok(QueueInputType::Prompt),
            other => Err(anyhow::anyhow!("unknown input type: {}", other)),
        }
    }
}

impl std::fmt::Display for QueueInputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status of a queue item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueItemStatus {
    Pending,
    Dispatching,
    Dispatched,
    Failed,
    Cancelled,
}

impl QueueItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueItemStatus::Pending => "pending",
            QueueItemStatus::Dispatching => "dispatching",
            QueueItemStatus::Dispatched => "dispatched",
            QueueItemStatus::Failed => "failed",
            QueueItemStatus::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for QueueItemStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(QueueItemStatus::Pending),
            "dispatching" => Ok(QueueItemStatus::Dispatching),
            "dispatched" => Ok(QueueItemStatus::Dispatched),
            "failed" => Ok(QueueItemStatus::Failed),
            "cancelled" => Ok(QueueItemStatus::Cancelled),
            other => Err(anyhow::anyhow!("unknown queue item status: {}", other)),
        }
    }
}

impl std::fmt::Display for QueueItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Priority levels for dispatch ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Critical = 0,
    High = 25,
    #[default]
    Medium = 50,
    Low = 75,
}

impl Priority {
    pub fn as_i32(&self) -> i32 {
        *self as i32
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Critical => "critical",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        }
    }
}

impl std::str::FromStr for Priority {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "critical" | "0" => Ok(Priority::Critical),
            "high" | "25" => Ok(Priority::High),
            "medium" | "50" => Ok(Priority::Medium),
            "low" | "75" => Ok(Priority::Low),
            other => Err(anyhow::anyhow!(
                "unknown priority '{}'; valid: critical, high, medium, low",
                other
            )),
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row in the dispatch_queue table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueRow {
    pub id: String,
    pub queue_name: String,
    pub input_type: QueueInputType,
    pub input_value: String,
    pub mode: Option<String>,
    pub priority: i32,
    pub params: Option<String>,
    pub status: QueueItemStatus,
    pub dispatch_id: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub enqueued_by: Option<String>,
    pub dispatched_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// Input for enqueuing a new item.
#[derive(Debug, Clone)]
pub struct EnqueueItem {
    pub queue_name: String,
    pub input_type: QueueInputType,
    pub input_value: String,
    pub mode: Option<String>,
    pub priority: Priority,
    pub params: Option<String>,
    pub enqueued_by: Option<String>,
}

impl Default for EnqueueItem {
    fn default() -> Self {
        Self {
            queue_name: "default".to_string(),
            input_type: QueueInputType::Prompt,
            input_value: String::new(),
            mode: None,
            priority: Priority::default(),
            params: None,
            enqueued_by: None,
        }
    }
}

/// Result of an enqueue operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueResult {
    Enqueued { id: String },
    Skipped(String),
}

impl EnqueueResult {
    pub fn is_enqueued(&self) -> bool {
        matches!(self, EnqueueResult::Enqueued { .. })
    }
}

/// The dispatch queue interface.
#[async_trait]
pub trait DispatchQueue: Send + Sync {
    /// Enqueue an item with dedup checks.
    async fn enqueue(&self, item: EnqueueItem) -> Result<EnqueueResult>;

    /// List pending items in a queue, ordered by priority then enqueued_at.
    async fn queue_list(&self, queue_name: &str) -> Result<Vec<QueueRow>>;

    /// Peek at the next N pending items ready for dispatch.
    async fn queue_peek(&self, queue_name: &str, limit: u32) -> Result<Vec<QueueRow>>;

    /// Atomically claim a pending item for dispatch (set status = 'dispatching').
    /// Returns true if the claim succeeded (row was still pending).
    async fn queue_claim(&self, id: &str) -> Result<bool>;

    /// Mark a queue item as dispatched with the registry dispatch_id.
    async fn queue_mark_dispatched(&self, id: &str, dispatch_id: &str) -> Result<()>;

    /// Mark a queue item as failed with an error message.
    async fn queue_mark_failed(&self, id: &str, error: &str) -> Result<()>;

    /// Clear all pending items from a queue.
    async fn queue_clear(&self, queue_name: &str) -> Result<u64>;

    /// Count pending items in a queue.
    async fn queue_pending_count(&self, queue_name: &str) -> Result<u64>;

    /// Check if an input_value is already pending or dispatching in a queue.
    async fn queue_has_pending(&self, queue_name: &str, input_value: &str) -> Result<bool>;

    /// Recover 'dispatching' rows on daemon restart.
    /// Returns (recovered_to_pending, marked_dispatched) counts.
    async fn queue_recover(&self) -> Result<(u64, u64)>;
}
