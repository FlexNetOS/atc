//! Source trait: a pluggable selection strategy that runs periodically and feeds queues.
//!
//! Each source is a loop body that calls `enqueue()` — the same dedup path as the CLI.
//! Sources are activated via `atc daemon --source <name>` and configured in `atc.toml [sources.*]`.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A source produces queue items on a schedule.
#[async_trait]
pub trait Source: Send + Sync {
    /// Human-readable name (e.g. "ready", "board", "events", "script").
    fn name(&self) -> &str;

    /// Run one iteration of the source (called on each poll interval).
    /// Returns the number of items enqueued (after dedup).
    async fn poll(&self) -> Result<u32>;

    /// Graceful shutdown (e.g., close event stream subscriptions).
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Source configuration from `atc.toml [sources.*]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SourceConfig {
    Ready(ReadySourceConfig),
    Board(BoardSourceConfig),
    Events(EventsSourceConfig),
    Script(ScriptSourceConfig),
}

impl SourceConfig {
    pub fn poll_interval_secs(&self) -> u64 {
        match self {
            SourceConfig::Ready(c) => c.poll_interval_secs,
            SourceConfig::Board(c) => c.poll_interval_secs,
            SourceConfig::Events(c) => c.poll_interval_secs,
            SourceConfig::Script(c) => c.poll_interval_secs,
        }
    }

    pub fn queue(&self) -> &str {
        match self {
            SourceConfig::Ready(c) => &c.queue,
            SourceConfig::Board(c) => &c.queue,
            SourceConfig::Events(c) => &c.queue,
            SourceConfig::Script(c) => &c.queue,
        }
    }
}

/// `[sources.ready]` — poll `kb_ready` for top-scored tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReadySourceConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_queue")]
    pub queue: String,
}

/// `[sources.board]` — poll `git kb list` / `kb_view` for matching tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BoardSourceConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_queue")]
    pub queue: String,
    #[serde(default)]
    pub filter_status: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    #[serde(default)]
    pub require_unassigned: bool,
    #[serde(default)]
    pub require_unblocked: bool,
    /// Execute a saved view instead of ad-hoc filters.
    pub view: Option<String>,
    pub filter_type: Option<Vec<String>>,
    pub filter_priority: Option<Vec<String>>,
    pub filter_container: Option<String>,
}

/// `[sources.events]` — subscribe to `git kb events` stream.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventsSourceConfig {
    #[serde(default = "default_events_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_queue")]
    pub queue: String,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub trigger_on_status: Vec<String>,
}

/// `[sources.script]` — run a user command and enqueue output lines.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptSourceConfig {
    pub command: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_queue")]
    pub queue: String,
}

fn default_poll_interval() -> u64 {
    10
}

fn default_events_poll_interval() -> u64 {
    5
}

fn default_limit() -> u32 {
    1
}

fn default_queue() -> String {
    "default".to_string()
}
