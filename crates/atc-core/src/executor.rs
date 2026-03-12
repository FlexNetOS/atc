use crate::types::Mode;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;

#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle>;
}

pub struct AgentOpts {
    pub slug: String,
    pub worktree_path: PathBuf,
    pub prompt: String,
    pub mode: Mode,
    pub log_file: PathBuf,
    pub env: HashMap<String, String>,
    pub session_name: String,
    pub sandbox: bool,
    pub inline: bool,
}

pub struct AgentHandle {
    pub session: String,
    pub inline_exit_code: Option<i32>,
}

/// Stub ClaudeExecutor — compiles but returns Err("not implemented").
/// Full implementation in tasks/gitkb-264.
pub struct ClaudeExecutor {
    pub claude_bin: PathBuf,
}

impl Default for ClaudeExecutor {
    fn default() -> Self {
        Self {
            claude_bin: PathBuf::from("claude"),
        }
    }
}

#[async_trait]
impl AgentExecutor for ClaudeExecutor {
    async fn spawn(&self, _opts: &AgentOpts) -> Result<AgentHandle> {
        anyhow::bail!("ClaudeExecutor::spawn not yet implemented — see tasks/gitkb-264")
    }
}
