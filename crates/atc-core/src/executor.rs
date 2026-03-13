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
    pub prompt: String, // rendered system prompt for the mode
    pub mode: Mode,
    pub log_file: PathBuf, // stream-json output destination
    pub env: HashMap<String, String>, // GITKB_WORKSPACE, GITKB_ROOT, etc.
    pub session_name: String, // tmux session name (derived from slug)
    pub sandbox: bool, // false = pass --settings with sandbox.enabled=false to claude
    pub inline: bool,  // true = CI mode, no tmux, run synchronously
    pub max_turns: u32,
    pub max_budget_usd: f64,
}

pub struct AgentHandle {
    pub session: String,             // tmux session name or pid string
    pub inline_exit_code: Option<i32>, // set immediately when inline = true
}

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

impl ClaudeExecutor {
    /// Build the user prompt preamble (the `-p` argument to claude).
    fn build_user_prompt(opts: &AgentOpts) -> String {
        format!(
            "Directive: {}\nTask: {}\nWorking directory: {}\n\n\
             The task document follows on stdin \u{2014} it IS your plan. \
             Follow the system prompt instructions exactly.",
            opts.mode.as_str(),
            opts.slug,
            opts.worktree_path.display(),
        )
    }

    /// Write sandbox-disable settings JSON to a file, returning the path.
    /// When sandbox=false, we pass this to claude --settings to disable OS sandbox.
    fn write_sandbox_settings(path: &std::path::Path) -> Result<()> {
        let settings = r#"{"sandbox":{"enabled":false}}"#;
        std::fs::write(path, settings)?;
        Ok(())
    }

    /// CI mode: run claude synchronously, capture exit code.
    async fn spawn_inline(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        use tokio::process::Command;

        // 1. Read task document via subprocess
        let kb_root = opts
            .env
            .get("GITKB_ROOT")
            .ok_or_else(|| anyhow::anyhow!("GITKB_ROOT not set in agent env"))?;

        let task_doc = Command::new("git")
            .args(["kb", "show", &opts.slug])
            .env("GITKB_ROOT", kb_root)
            .output()
            .await?;

        if !task_doc.status.success() && !task_doc.stdout.is_empty() {
            // Warn but proceed if stdout is empty (unusual but not fatal per spec)
        }
        if task_doc.stdout.is_empty() {
            eprintln!(
                "warning: git kb show {} returned empty output",
                opts.slug
            );
        }

        // 2. Create log file parent dirs
        if let Some(parent) = opts.log_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 3. Write system prompt to temp file
        let prompt_file = tempfile::NamedTempFile::new()?;
        std::fs::write(prompt_file.path(), &opts.prompt)?;

        // 4. Optionally write sandbox settings
        let sandbox_file = if !opts.sandbox {
            let f = tempfile::NamedTempFile::new()?;
            Self::write_sandbox_settings(f.path())?;
            Some(f)
        } else {
            None
        };

        // 5. Build claude command
        let user_prompt = Self::build_user_prompt(opts);
        let mut cmd = Command::new(&self.claude_bin);
        cmd.arg("-p")
            .arg(&user_prompt)
            .arg("--append-system-prompt-file")
            .arg(prompt_file.path())
            .arg("--dangerously-skip-permissions")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--max-turns")
            .arg(opts.max_turns.to_string())
            .arg("--max-budget-usd")
            .arg(opts.max_budget_usd.to_string());

        if let Some(ref sf) = sandbox_file {
            cmd.arg("--settings").arg(sf.path());
        }

        // 6. Set cwd and env
        cmd.current_dir(&opts.worktree_path);
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        // 7. Pipe task doc to stdin
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        // Write task doc to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(&task_doc.stdout).await?;
            drop(stdin);
        }

        // 8. Tee stdout+stderr to log file
        let output = child.wait_with_output().await?;

        // Write combined output to log file
        let mut log_content = output.stdout.clone();
        if !output.stderr.is_empty() {
            log_content.extend_from_slice(&output.stderr);
        }
        tokio::fs::write(&opts.log_file, &log_content).await?;

        // 9. Extract exit code
        let exit_code = output.status.code().unwrap_or(-1);

        // Temp files cleaned up on drop

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(exit_code),
        })
    }

    /// Local mode: create a named tmux session, return immediately.
    async fn spawn_tmux(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        use tokio::process::Command;

        let kb_root = opts
            .env
            .get("GITKB_ROOT")
            .ok_or_else(|| anyhow::anyhow!("GITKB_ROOT not set in agent env"))?;

        // 1. Write system prompt to a stable path (must outlive this process)
        let log_dir = opts
            .log_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        std::fs::create_dir_all(log_dir)?;

        let prompt_path = log_dir.join(format!("{}.prompt.md", opts.session_name));
        std::fs::write(&prompt_path, &opts.prompt)?;

        // 2. Optionally write sandbox settings
        let sandbox_path = if !opts.sandbox {
            let p = log_dir.join(format!("{}.sandbox.json", opts.session_name));
            Self::write_sandbox_settings(&p)?;
            Some(p)
        } else {
            None
        };

        // 3. Build the bash -c command string
        let user_prompt = Self::build_user_prompt(opts);
        let prompt_path_str = prompt_path.to_string_lossy();
        let log_file_str = opts.log_file.to_string_lossy();
        let worktree_str = opts.worktree_path.to_string_lossy();
        let claude_bin_str = self.claude_bin.to_string_lossy();

        let mut bash_parts = Vec::new();

        // Export env vars
        for (k, v) in &opts.env {
            bash_parts.push(format!("export {}='{}'", k, shell_escape(v)));
        }

        // cd to worktree
        bash_parts.push(format!("cd '{}'", shell_escape(&worktree_str)));

        // Build the claude pipeline
        let mut claude_cmd = format!(
            "GITKB_ROOT='{}' git kb show '{}' | '{}' -p '{}' \
             --append-system-prompt-file '{}' \
             --dangerously-skip-permissions \
             --output-format stream-json --verbose \
             --max-turns {} --max-budget-usd {}",
            shell_escape(kb_root),
            shell_escape(&opts.slug),
            shell_escape(&claude_bin_str),
            shell_escape(&user_prompt),
            shell_escape(&prompt_path_str),
            opts.max_turns,
            opts.max_budget_usd,
        );

        if let Some(ref sp) = sandbox_path {
            claude_cmd.push_str(&format!(" --settings '{}'", shell_escape(&sp.to_string_lossy())));
        }

        claude_cmd.push_str(&format!(" 2>&1 | tee '{}'", shell_escape(&log_file_str)));

        bash_parts.push(claude_cmd);

        // Cleanup temp files
        bash_parts.push("EXIT_CODE=$?".to_string());
        bash_parts.push(format!("rm -f '{}'", shell_escape(&prompt_path_str)));
        if let Some(ref sp) = sandbox_path {
            bash_parts.push(format!("rm -f '{}'", shell_escape(&sp.to_string_lossy())));
        }
        bash_parts.push("exit $EXIT_CODE".to_string());

        let bash_body = bash_parts.join(" ; ");

        // 4. Create tmux session
        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &opts.session_name,
                "bash",
                "-c",
                &bash_body,
            ])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("duplicate session") || stderr.contains("already exists") {
                anyhow::bail!(
                    "tmux session '{}' already exists; use `atc redirect` to reattach or kill the session",
                    opts.session_name
                );
            }
            anyhow::bail!(
                "tmux new-session failed (exit {:?}): {}",
                output.status.code(),
                stderr
            );
        }

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: None,
        })
    }
}

/// Simple shell escaping: escape single quotes within single-quoted strings.
fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[async_trait]
impl AgentExecutor for ClaudeExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        if opts.inline {
            self.spawn_inline(opts).await
        } else {
            self.spawn_tmux(opts).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;

    #[test]
    fn test_build_user_prompt() {
        let opts = AgentOpts {
            slug: "tasks/gitkb-42".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/gitkb/core"),
            prompt: String::new(),
            mode: Mode::Implement,
            log_file: PathBuf::from("/tmp/log.jsonl"),
            env: HashMap::new(),
            session_name: "test".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 10_000,
            max_budget_usd: 25.0,
        };
        let prompt = ClaudeExecutor::build_user_prompt(&opts);
        assert!(prompt.contains("Directive: implement"));
        assert!(prompt.contains("Task: tasks/gitkb-42"));
        assert!(prompt.contains("Working directory: /tmp/worktrees/gitkb/core"));
        assert!(prompt.contains("The task document follows on stdin"));
    }

    #[test]
    fn test_shell_escape() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("it's"), "it'\\''s");
        assert_eq!(shell_escape("a'b'c"), "a'\\''b'\\''c");
    }
}
