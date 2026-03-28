use crate::types::Directive;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle>;
}

pub struct AgentOpts {
    pub slug: String,
    pub worktree_path: PathBuf,
    pub prompt: String, // rendered system prompt for the directive
    pub directive: Directive,
    pub log_file: Option<PathBuf>, // stream-json output destination (None for ephemeral)
    pub env: HashMap<String, String>, // GITKB_WORKSPACE, GITKB_ROOT, etc.
    pub session_name: String,      // tmux session name (derived from slug)
    pub dispatch_id: String,       // stable registry ID (used for post-complete --id)
    pub sandbox: bool,             // false = pass --settings with sandbox.enabled=false to claude
    pub inline: bool,              // true = CI mode, no tmux, run synchronously
    pub max_turns: u32,
    pub max_budget_usd: f64,
    /// Pre-built stdin content from the pipeline (for non-task dispatches).
    /// When set, the executor pipes this directly to claude stdin instead of
    /// calling `git kb show`. When None, falls back to fetching the task
    /// document from git-kb (legacy task dispatch path).
    pub stdin_content: Option<String>,
    /// Ephemeral mode: skip log file, use text output, pass template body as -p directly.
    pub ephemeral: bool,
    /// Timeout in seconds for inline execution (kill after N seconds, exit 124).
    pub timeout: Option<u32>,
}

#[derive(Debug)]
pub struct AgentHandle {
    pub session: String,               // tmux session name or pid string
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
        if opts.stdin_content.is_some() {
            // Non-task dispatch (prompt/template): the system prompt already
            // contains the full instructions. Stdin carries a context
            // separator — not a duplicate of the system prompt.
            format!(
                "Directive: {}\nTask: {}\nWorking directory: {}\n\n\
                 Follow the system prompt instructions exactly.",
                opts.directive.as_str(),
                opts.slug,
                opts.worktree_path.display(),
            )
        } else {
            // Task dispatch: stdin carries the task document from git-kb.
            format!(
                "Directive: {}\nTask: {}\nWorking directory: {}\n\n\
                 The task document follows on stdin \u{2014} it IS your plan. \
                 Follow the system prompt instructions exactly.",
                opts.directive.as_str(),
                opts.slug,
                opts.worktree_path.display(),
            )
        }
    }

    /// Write sandbox-disable settings JSON to a file, returning the path.
    /// When sandbox=false, we pass this to claude --settings to disable OS sandbox.
    async fn write_sandbox_settings(path: &std::path::Path) -> Result<()> {
        let settings = r#"{"sandbox":{"enabled":false}}"#;
        tokio::fs::write(path, settings).await?;
        Ok(())
    }

    /// Wait for a child process with an optional timeout.
    /// On timeout, kills the child and returns exit code 124 (matching `timeout(1)`).
    async fn wait_with_timeout(
        child: &mut tokio::process::Child,
        timeout: Option<u32>,
        slug: &str,
    ) -> Result<std::process::ExitStatus> {
        if let Some(secs) = timeout {
            match tokio::time::timeout(Duration::from_secs(secs as u64), child.wait()).await {
                Ok(result) => result.map_err(Into::into),
                Err(_) => {
                    warn!(slug = %slug, timeout_secs = secs, "inline spawn timed out, killing child");
                    let _ = child.kill().await;
                    // Return a synthetic "timed out" exit status — caller checks inline_exit_code
                    Err(anyhow::anyhow!("__timeout__"))
                }
            }
        } else {
            child.wait().await.map_err(Into::into)
        }
    }

    /// CI mode: run claude synchronously, capture exit code.
    #[tracing::instrument(skip(self, opts), fields(slug = %opts.slug, session = %opts.session_name))]
    async fn spawn_inline(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        // Ephemeral fast path
        if opts.ephemeral {
            return self.spawn_inline_ephemeral(opts).await;
        }

        use tokio::process::Command;

        // 1. Get stdin content: use pre-built content or fetch from git-kb
        let stdin_bytes = if let Some(ref content) = opts.stdin_content {
            content.as_bytes().to_vec()
        } else {
            // Legacy path: fetch task document from git-kb (task dispatches only)
            let kb_root = opts
                .env
                .get("GITKB_ROOT")
                .ok_or_else(|| anyhow::anyhow!("GITKB_ROOT not set in agent env"))?;

            let task_doc = tokio::time::timeout(
                Duration::from_secs(30),
                Command::new("git-kb")
                    .args(["show", &opts.slug])
                    .env("GITKB_ROOT", kb_root)
                    .env(
                        "GITKB_WORKSPACE",
                        opts.env
                            .get("GITKB_WORKSPACE")
                            .map(|s| s.as_str())
                            .unwrap_or("main"),
                    )
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("git-kb show {} timed out", opts.slug))??;

            if !task_doc.status.success() {
                anyhow::bail!(
                    "git kb show {} failed (exit {:?}): {}",
                    opts.slug,
                    task_doc.status.code(),
                    String::from_utf8_lossy(&task_doc.stderr)
                );
            }
            if task_doc.stdout.is_empty() {
                warn!(slug = %opts.slug, "git kb show returned empty output");
            }
            task_doc.stdout
        };

        // 2. Create log file parent dirs
        let log_file = opts.log_file.as_ref().ok_or_else(|| {
            anyhow::anyhow!("log_file required for non-ephemeral inline dispatch")
        })?;
        if let Some(parent) = log_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 3. Write system prompt to temp file
        let prompt_file = tempfile::NamedTempFile::new()?;
        tokio::fs::write(prompt_file.path(), &opts.prompt).await?;

        // 4. Optionally write sandbox settings
        let sandbox_file = if !opts.sandbox {
            let f = tempfile::NamedTempFile::new()?;
            Self::write_sandbox_settings(f.path()).await?;
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

        // 6. Set cwd and env (empty value = remove from inherited env)
        cmd.current_dir(&opts.worktree_path);
        for (k, v) in &opts.env {
            if v.is_empty() {
                cmd.env_remove(k);
            } else {
                cmd.env(k, v);
            }
        }

        // 7. Pipe task doc to stdin, merge stderr into stdout (2>&1 equivalent)
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        // Both stdout and stderr are piped separately, then interleaved into
        // the log file line-by-line to preserve temporal ordering, matching
        // the `2>&1 | tee` behavior of spawn_tmux.
        cmd.stderr(std::process::Stdio::piped());

        info!(slug = %opts.slug, "spawning claude (inline)");
        let mut child = cmd.spawn()?;

        // Write stdin content (task doc or pre-built content) to claude stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = stdin.write_all(&stdin_bytes).await {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(e.into());
                }
                warn!(slug = %opts.slug, error = %e, "claude closed stdin early");
            }
            drop(stdin);
        }

        // 8. Stream stdout and stderr to log file with interleaving preserved
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let log_file_handle = tokio::fs::File::create(log_file).await?;
        let log_writer = std::sync::Arc::new(tokio::sync::Mutex::new(tokio::io::BufWriter::new(
            log_file_handle,
        )));

        let mut tasks = Vec::new();

        if let Some(stdout) = stdout {
            tasks.push(spawn_stream_to_log(stdout, log_writer.clone()));
        }

        if let Some(stderr) = stderr {
            tasks.push(spawn_stream_to_log(stderr, log_writer.clone()));
        }

        // Wait for all stream tasks and the child process
        for t in tasks {
            if let Err(e) = t.await {
                tracing::warn!(slug = %opts.slug, error = %e, "log stream task failed");
            }
        }
        let exit_code = match Self::wait_with_timeout(&mut child, opts.timeout, &opts.slug).await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) if e.to_string() == "__timeout__" => 124,
            Err(e) => return Err(e),
        };
        info!(slug = %opts.slug, exit_code, "inline spawn completed");

        // Temp files cleaned up on drop

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(exit_code),
        })
    }

    /// Ephemeral inline mode: stdout/stderr inherited, text output, no log file, no system prompt.
    #[tracing::instrument(skip(self, opts), fields(slug = %opts.slug, session = %opts.session_name))]
    async fn spawn_inline_ephemeral(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        use tokio::process::Command;

        // The -p argument is the rendered template body directly (no build_user_prompt wrapper)
        let user_prompt = opts
            .stdin_content
            .as_deref()
            .unwrap_or("No prompt provided.");

        // Optionally write sandbox settings (same as non-ephemeral path)
        let sandbox_file = if !opts.sandbox {
            let f = tempfile::NamedTempFile::new()?;
            Self::write_sandbox_settings(f.path()).await?;
            Some(f)
        } else {
            None
        };

        let mut cmd = Command::new(&self.claude_bin);
        cmd.arg("-p")
            .arg(user_prompt)
            .arg("--dangerously-skip-permissions")
            .arg("--output-format")
            .arg("text")
            .arg("--max-turns")
            .arg(opts.max_turns.to_string())
            .arg("--max-budget-usd")
            .arg(opts.max_budget_usd.to_string());

        if let Some(ref sf) = sandbox_file {
            cmd.arg("--settings").arg(sf.path());
        }

        // No --append-system-prompt-file, no --verbose

        // Set cwd and env
        cmd.current_dir(&opts.worktree_path);
        for (k, v) in &opts.env {
            if v.is_empty() {
                cmd.env_remove(k);
            } else {
                cmd.env(k, v);
            }
        }

        // Inherit stdout/stderr — output goes directly to terminal
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());

        info!(slug = %opts.slug, "spawning claude (ephemeral inline)");
        let mut child = cmd.spawn()?;

        // Wait with optional timeout (shared helper)
        let exit_code = match Self::wait_with_timeout(&mut child, opts.timeout, &opts.slug).await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) if e.to_string() == "__timeout__" => 124,
            Err(e) => return Err(e),
        };
        info!(slug = %opts.slug, exit_code, "ephemeral inline spawn completed");

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(exit_code),
        })
    }

    /// Clean up pre-written temp files when tmux session creation fails.
    async fn cleanup_tmux_files(
        prompt_path: &std::path::Path,
        task_doc_path: &std::path::Path,
        sandbox_path: Option<&std::path::Path>,
    ) {
        let _ = tokio::fs::remove_file(prompt_path).await;
        let _ = tokio::fs::remove_file(task_doc_path).await;
        if let Some(sp) = sandbox_path {
            let _ = tokio::fs::remove_file(sp).await;
        }
    }

    /// Build the bash -c body that will run inside the tmux session.
    ///
    /// This is extracted from `spawn_tmux` so the generated script can be
    /// unit-tested without actually launching tmux.
    fn build_tmux_bash_body(
        &self,
        opts: &AgentOpts,
        prompt_path: &std::path::Path,
        task_doc_path: &std::path::Path,
        sandbox_path: Option<&std::path::Path>,
    ) -> Result<String> {
        let user_prompt = Self::build_user_prompt(opts);
        let prompt_path_str = prompt_path.to_string_lossy();
        let log_file = opts
            .log_file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("log_file required for tmux dispatch"))?;
        let log_file_str = log_file.to_string_lossy();
        let worktree_str = opts.worktree_path.to_string_lossy();
        let claude_bin_str = self.claude_bin.to_string_lossy();
        let task_doc_path_str = task_doc_path.to_string_lossy();

        let mut bash_parts = Vec::new();

        // Ensure pipe failures propagate (so EXIT_CODE captures claude's exit, not tee's)
        bash_parts.push("set -o pipefail".to_string());

        // Trap EXIT to clean up temp files on any exit path (including early
        // failures like git-kb show timeout or cd failure).
        // Assign paths to variables, then reference them in a single-quoted
        // trap body. The trap body contains only $VAR references (no single
        // quotes), so the outer single quotes are safe. Variables are expanded
        // at trap-fire time, and the inner double quotes protect against
        // word-splitting on paths with spaces.
        bash_parts.push(format!(
            "ATC_PROMPT_FILE='{}'",
            shell_escape(&prompt_path.to_string_lossy())?
        ));
        bash_parts.push(format!(
            "ATC_TASKDOC_FILE='{}'",
            shell_escape(&task_doc_path.to_string_lossy())?
        ));
        if let Some(sp) = sandbox_path {
            bash_parts.push(format!(
                "ATC_SANDBOX_FILE='{}'",
                shell_escape(&sp.to_string_lossy())?
            ));
        }
        let trap_rm = if sandbox_path.is_some() {
            r#"rm -f "$ATC_PROMPT_FILE" "$ATC_TASKDOC_FILE" "$ATC_SANDBOX_FILE""#
        } else {
            r#"rm -f "$ATC_PROMPT_FILE" "$ATC_TASKDOC_FILE""#
        };
        bash_parts.push(format!("trap '{}' EXIT", trap_rm));

        // Export env vars (keys are validated to prevent shell injection)
        // Empty value = unset the variable from the inherited environment.
        for (k, v) in &opts.env {
            validate_env_key(k)?;
            if v.is_empty() {
                bash_parts.push(format!("unset {}", k));
            } else {
                bash_parts.push(format!("export {}='{}'", k, shell_escape(v)?));
            }
        }

        // cd to worktree
        bash_parts.push(format!("cd '{}'", shell_escape(&worktree_str)?));

        if opts.stdin_content.is_none() {
            // Legacy path: fetch task document from git-kb (task dispatches only)
            let kb_root = opts
                .env
                .get("GITKB_ROOT")
                .ok_or_else(|| anyhow::anyhow!("GITKB_ROOT not set in agent env"))?;
            let gitkb_workspace = opts
                .env
                .get("GITKB_WORKSPACE")
                .map(|s| s.as_str())
                .unwrap_or("main");
            bash_parts.push(format!(
                "GITKB_ROOT='{}' GITKB_WORKSPACE='{}' timeout 30 git-kb show '{}' > '{}' || {{ echo 'error: git-kb show failed or timed out' >&2 ; exit 1 ; }}",
                shell_escape(kb_root)?,
                shell_escape(gitkb_workspace)?,
                shell_escape(&opts.slug)?,
                shell_escape(&task_doc_path_str)?,
            ));
        }

        // Build the claude pipeline — pipe task doc file to claude
        let mut claude_cmd = format!(
            "cat '{}' | '{}' -p '{}' \
             --append-system-prompt-file '{}' \
             --dangerously-skip-permissions \
             --output-format stream-json --verbose \
             --max-turns {} --max-budget-usd {}",
            shell_escape(&task_doc_path_str)?,
            shell_escape(&claude_bin_str)?,
            shell_escape(&user_prompt)?,
            shell_escape(&prompt_path_str)?,
            opts.max_turns,
            opts.max_budget_usd,
        );

        if let Some(sp) = sandbox_path {
            claude_cmd.push_str(&format!(
                " --settings '{}'",
                shell_escape(&sp.to_string_lossy())?
            ));
        }

        claude_cmd.push_str(&format!(" 2>&1 | tee '{}'", shell_escape(&log_file_str)?));

        bash_parts.push(claude_cmd);

        // Capture Claude's exit code from: cat | claude | tee
        bash_parts.push("EXIT_CODE=${PIPESTATUS[1]}".to_string());

        // Run post-completion pipeline (fire and forget — errors logged but don't fail the session)
        bash_parts.push(format!(
            "atc post-complete --id '{}' --exit-code $EXIT_CODE --log '{}' 2>/dev/null || true",
            shell_escape(&opts.dispatch_id)?,
            shell_escape(&log_file_str)?,
        ));

        // Exit with Claude's exit code (trap handles temp file cleanup)
        bash_parts.push("exit $EXIT_CODE".to_string());

        Ok(bash_parts.join(" ; "))
    }

    /// Local mode: create a named tmux session, return immediately.
    #[tracing::instrument(skip(self, opts), fields(slug = %opts.slug, session = %opts.session_name))]
    async fn spawn_tmux(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        use tokio::process::Command;

        // 1. Write system prompt to a stable path (must outlive this process)
        let log_file = opts
            .log_file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("log_file required for tmux dispatch"))?;
        let log_dir = log_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        tokio::fs::create_dir_all(log_dir).await?;

        let prompt_path = log_dir.join(format!("{}.prompt.md", opts.session_name));
        tokio::fs::write(&prompt_path, &opts.prompt).await?;

        // 2. Optionally write sandbox settings
        let sandbox_path = if !opts.sandbox {
            let p = log_dir.join(format!("{}.sandbox.json", opts.session_name));
            if let Err(e) = Self::write_sandbox_settings(&p).await {
                let _ = tokio::fs::remove_file(&prompt_path).await;
                let _ = tokio::fs::remove_file(&p).await;
                return Err(e);
            }
            Some(p)
        } else {
            None
        };

        // 3. Write stdin content to a temp file to avoid shell expansion.
        // Writing to a file (rather than a shell variable + echo) prevents
        // command injection via $(), backticks, or other expansion in the content.
        let task_doc_path = log_dir.join(format!("{}.taskdoc", opts.session_name));

        if let Some(ref content) = opts.stdin_content {
            // Pre-built stdin content: write directly to the temp file
            if let Err(e) = tokio::fs::write(&task_doc_path, content).await {
                Self::cleanup_tmux_files(&prompt_path, &task_doc_path, sandbox_path.as_deref())
                    .await;
                return Err(e.into());
            }
        }

        // 4. Build the bash -c command string
        let bash_body = match self.build_tmux_bash_body(
            opts,
            &prompt_path,
            &task_doc_path,
            sandbox_path.as_deref(),
        ) {
            Ok(b) => b,
            Err(e) => {
                Self::cleanup_tmux_files(&prompt_path, &task_doc_path, sandbox_path.as_deref())
                    .await;
                return Err(e);
            }
        };

        // 5. Create tmux session
        info!(session = %opts.session_name, "creating tmux session");
        let output = match Command::new("tmux")
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
            .await
        {
            Ok(o) => o,
            Err(e) => {
                Self::cleanup_tmux_files(&prompt_path, &task_doc_path, sandbox_path.as_deref())
                    .await;
                return Err(e.into());
            }
        };

        if !output.status.success() {
            Self::cleanup_tmux_files(&prompt_path, &task_doc_path, sandbox_path.as_deref()).await;

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

        info!(session = %opts.session_name, "tmux session created");
        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: None,
        })
    }
}

/// Spawn a tokio task that reads lines from `reader` and writes them to the
/// shared `writer`, flushing after each line for real-time observability.
fn spawn_stream_to_log<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    writer: std::sync::Arc<tokio::sync::Mutex<tokio::io::BufWriter<tokio::fs::File>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut w = writer.lock().await;
            if let Err(e) = w.write_all(&line).await {
                warn!(error = %e, "failed to write to log");
            }
            if let Err(e) = w.flush().await {
                warn!(error = %e, "failed to flush log");
            }
        }
    })
}

/// Validate that an environment variable key is safe for shell `export`.
/// Accepts only `[A-Za-z_][A-Za-z0-9_]*` — the POSIX portable name set.
/// Rejects keys that could enable shell injection (e.g., `x; rm -rf /`).
pub fn validate_env_key(key: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !key.is_empty(),
        "environment variable key must not be empty"
    );
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    anyhow::ensure!(
        first.is_ascii_alphabetic() || first == '_',
        "environment variable key must start with [A-Za-z_], got: {key:?}"
    );
    anyhow::ensure!(
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "environment variable key must contain only [A-Za-z0-9_], got: {key:?}"
    );
    Ok(())
}

/// Simple shell escaping: escape single quotes within single-quoted strings.
/// Rejects NUL bytes which would silently truncate bash strings.
fn shell_escape(s: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !s.contains('\0'),
        "NUL byte in shell argument is not allowed"
    );
    Ok(s.replace('\'', "'\\''"))
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
    use crate::types::Directive;

    #[test]
    fn test_build_user_prompt() {
        let opts = AgentOpts {
            slug: "tasks/gitkb-42".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/gitkb/core"),
            prompt: String::new(),
            directive: Directive::Implement,
            log_file: Some(PathBuf::from("/tmp/log.jsonl")),
            env: HashMap::new(),
            session_name: "test".to_string(),
            dispatch_id: "test".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 10_000,
            max_budget_usd: 25.0,
            stdin_content: None,
            ephemeral: false,
            timeout: None,
        };
        let prompt = ClaudeExecutor::build_user_prompt(&opts);
        assert!(prompt.contains("Directive: implement"));
        assert!(prompt.contains("Task: tasks/gitkb-42"));
        assert!(prompt.contains("Working directory: /tmp/worktrees/gitkb/core"));
        assert!(prompt.contains("The task document follows on stdin"));
    }

    #[test]
    fn test_build_user_prompt_with_stdin_content() {
        let opts = AgentOpts {
            slug: "my-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/test"),
            prompt: String::new(),
            directive: Directive::Implement,
            log_file: Some(PathBuf::from("/tmp/log.jsonl")),
            env: HashMap::new(),
            session_name: "test".to_string(),
            dispatch_id: "test".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 10_000,
            max_budget_usd: 25.0,
            stdin_content: Some("some content".to_string()),
            ephemeral: false,
            timeout: None,
        };
        let prompt = ClaudeExecutor::build_user_prompt(&opts);
        assert!(prompt.contains("Directive: implement"));
        assert!(prompt.contains("Task: my-branch"));
        assert!(
            !prompt.contains("task document follows on stdin"),
            "non-task dispatch should not reference task document on stdin"
        );
        assert!(prompt.contains("Follow the system prompt instructions exactly"));
    }

    #[test]
    fn test_shell_escape() {
        assert_eq!(shell_escape("hello").unwrap(), "hello");
        assert_eq!(shell_escape("it's").unwrap(), "it'\\''s");
        assert_eq!(shell_escape("a'b'c").unwrap(), "a'\\''b'\\''c");
        assert_eq!(shell_escape("").unwrap(), "");
    }

    #[test]
    fn test_shell_escape_special_chars_safe_in_single_quotes() {
        // These characters are all safe inside single-quoted bash strings
        // (single quotes prevent all expansion). Verify they pass through unchanged.
        assert_eq!(shell_escape("$(rm -rf /)").unwrap(), "$(rm -rf /)");
        assert_eq!(shell_escape("`whoami`").unwrap(), "`whoami`");
        assert_eq!(shell_escape("$HOME").unwrap(), "$HOME");
        assert_eq!(shell_escape("back\\slash").unwrap(), "back\\slash");
        assert_eq!(shell_escape("new\nline").unwrap(), "new\nline");
        assert_eq!(shell_escape("tab\there").unwrap(), "tab\there");
        assert_eq!(shell_escape("semi;colon").unwrap(), "semi;colon");
        assert_eq!(shell_escape("pipe|cmd").unwrap(), "pipe|cmd");
        assert_eq!(shell_escape("amp&bg").unwrap(), "amp&bg");
    }

    #[test]
    fn test_shell_escape_rejects_nul() {
        let result = shell_escape("hello\0world");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("NUL byte"),
            "error message should mention NUL byte"
        );
    }

    #[test]
    fn test_validate_env_key_valid() {
        assert!(validate_env_key("HOME").is_ok());
        assert!(validate_env_key("GITKB_ROOT").is_ok());
        assert!(validate_env_key("_private").is_ok());
        assert!(validate_env_key("A").is_ok());
        assert!(validate_env_key("_").is_ok());
        assert!(validate_env_key("VAR_123").is_ok());
    }

    #[test]
    fn test_validate_env_key_rejects_injection() {
        assert!(validate_env_key("").is_err());
        assert!(validate_env_key("x; rm -rf /").is_err());
        assert!(validate_env_key("FOO=bar").is_err());
        assert!(validate_env_key("1STARTS_WITH_DIGIT").is_err());
        assert!(validate_env_key("has space").is_err());
        assert!(validate_env_key("has-dash").is_err());
        assert!(validate_env_key("$(cmd)").is_err());
    }

    /// Helper to create AgentOpts for tests.
    fn make_test_opts(stdin_content: Option<String>, env: HashMap<String, String>) -> AgentOpts {
        AgentOpts {
            slug: "test-slug".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            prompt: "test system prompt".to_string(),
            directive: Directive::Implement,
            log_file: Some(PathBuf::from("/tmp/test.jsonl")),
            env,
            session_name: "test-session".to_string(),
            dispatch_id: "test-dispatch".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 100,
            max_budget_usd: 5.0,
            stdin_content,
            ephemeral: false,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn test_spawn_inline_with_stdin_content_skips_gitkb() {
        // When stdin_content is set, executor should NOT require GITKB_ROOT
        // and should NOT call git-kb show. We can verify by not setting
        // GITKB_ROOT and confirming no error about it.
        let executor = ClaudeExecutor {
            claude_bin: PathBuf::from("echo"), // use echo as a harmless command
        };
        let tmp = tempfile::tempdir().unwrap();
        let opts = AgentOpts {
            log_file: Some(tmp.path().join("test.jsonl")),
            worktree_path: tmp.path().to_path_buf(),
            ..make_test_opts(
                Some("Hello from stdin content".to_string()),
                HashMap::new(), // No GITKB_ROOT
            )
        };

        let result = executor.spawn_inline(&opts).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_spawn_inline_without_stdin_content_requires_gitkb_root() {
        // When stdin_content is None, executor should require GITKB_ROOT
        let executor = ClaudeExecutor::default();
        let opts = make_test_opts(None, HashMap::new()); // No GITKB_ROOT

        let result = executor.spawn_inline(&opts).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("GITKB_ROOT not set"),
            "should require GITKB_ROOT when stdin_content is None"
        );
    }

    #[test]
    fn test_build_tmux_bash_body_with_stdin_content_skips_gitkb() {
        // When stdin_content is set, the generated bash body should NOT contain
        // a git-kb show command (content is pre-written to the taskdoc file).
        let executor = ClaudeExecutor::default();
        let opts = AgentOpts {
            stdin_content: Some("Hello from stdin content".to_string()),
            env: HashMap::new(), // No GITKB_ROOT
            ..make_test_opts(None, HashMap::new())
        };

        let prompt_path = PathBuf::from("/tmp/test.prompt.md");
        let task_doc_path = PathBuf::from("/tmp/test.taskdoc");

        let result = executor.build_tmux_bash_body(&opts, &prompt_path, &task_doc_path, None);
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());

        let body = result.unwrap();
        assert!(
            !body.contains("git-kb"),
            "bash body should not contain git-kb when stdin_content is set, got: {}",
            body
        );
        assert!(
            body.contains("cat '/tmp/test.taskdoc'"),
            "bash body should pipe the taskdoc file to claude"
        );
        assert!(
            body.contains("trap 'rm -f"),
            "bash body should contain a trap for cleanup, got: {}",
            body
        );
    }

    #[test]
    fn test_build_tmux_bash_body_task_dispatch_has_timeout_and_trap() {
        let executor = ClaudeExecutor::default();
        let mut env = HashMap::new();
        env.insert("GITKB_ROOT".to_string(), "/tmp/kb".to_string());
        let opts = make_test_opts(None, env);

        let prompt_path = PathBuf::from("/tmp/test.prompt.md");
        let task_doc_path = PathBuf::from("/tmp/test.taskdoc");

        let body = executor
            .build_tmux_bash_body(&opts, &prompt_path, &task_doc_path, None)
            .unwrap();
        assert!(
            body.contains("timeout 30"),
            "git-kb show should have a 30s timeout, got: {}",
            body
        );
        assert!(
            body.contains("trap 'rm -f"),
            "bash body should contain a trap for cleanup, got: {}",
            body
        );
    }

    #[test]
    fn test_build_tmux_bash_body_without_stdin_content_requires_gitkb_root() {
        // When stdin_content is None, building the bash body should require GITKB_ROOT
        let executor = ClaudeExecutor::default();
        let opts = AgentOpts {
            stdin_content: None,
            env: HashMap::new(), // No GITKB_ROOT
            ..make_test_opts(None, HashMap::new())
        };

        let prompt_path = PathBuf::from("/tmp/test.prompt.md");
        let task_doc_path = PathBuf::from("/tmp/test.taskdoc");

        let result = executor.build_tmux_bash_body(&opts, &prompt_path, &task_doc_path, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("GITKB_ROOT not set"),
            "should require GITKB_ROOT when stdin_content is None"
        );
    }

    #[test]
    fn test_build_tmux_bash_body_trap_with_sandbox() {
        let executor = ClaudeExecutor::default();
        let opts = AgentOpts {
            stdin_content: Some("content".to_string()),
            ..make_test_opts(None, HashMap::new())
        };

        let prompt_path = PathBuf::from("/tmp/test.prompt.md");
        let task_doc_path = PathBuf::from("/tmp/test.taskdoc");
        let sandbox_path = PathBuf::from("/tmp/test.sandbox.json");

        let body = executor
            .build_tmux_bash_body(&opts, &prompt_path, &task_doc_path, Some(&sandbox_path))
            .unwrap();

        // Verify variable assignments are present
        assert!(
            body.contains("ATC_PROMPT_FILE="),
            "should assign ATC_PROMPT_FILE, got: {}",
            body
        );
        assert!(
            body.contains("ATC_TASKDOC_FILE="),
            "should assign ATC_TASKDOC_FILE, got: {}",
            body
        );
        assert!(
            body.contains("ATC_SANDBOX_FILE="),
            "should assign ATC_SANDBOX_FILE when sandbox path provided, got: {}",
            body
        );
        // Verify trap references all three variables
        assert!(
            body.contains(
                r#"trap 'rm -f "$ATC_PROMPT_FILE" "$ATC_TASKDOC_FILE" "$ATC_SANDBOX_FILE"' EXIT"#
            ),
            "trap should reference all three variables, got: {}",
            body
        );
    }

    #[test]
    fn test_build_tmux_bash_body_trap_without_sandbox() {
        let executor = ClaudeExecutor::default();
        let opts = AgentOpts {
            stdin_content: Some("content".to_string()),
            ..make_test_opts(None, HashMap::new())
        };

        let prompt_path = PathBuf::from("/tmp/test.prompt.md");
        let task_doc_path = PathBuf::from("/tmp/test.taskdoc");

        let body = executor
            .build_tmux_bash_body(&opts, &prompt_path, &task_doc_path, None)
            .unwrap();

        assert!(
            !body.contains("ATC_SANDBOX_FILE"),
            "should not reference ATC_SANDBOX_FILE when no sandbox path, got: {}",
            body
        );
        assert!(
            body.contains(r#"trap 'rm -f "$ATC_PROMPT_FILE" "$ATC_TASKDOC_FILE"' EXIT"#),
            "trap should reference only two variables, got: {}",
            body
        );
    }

    #[test]
    fn test_ephemeral_opts_defaults() {
        let opts = make_test_opts(Some("template body".to_string()), HashMap::new());
        assert!(!opts.ephemeral);
        assert_eq!(opts.timeout, None);
    }

    #[test]
    fn test_ephemeral_opts_set() {
        let mut opts = make_test_opts(Some("template body".to_string()), HashMap::new());
        opts.ephemeral = true;
        opts.timeout = Some(15);
        assert!(opts.ephemeral);
        assert_eq!(opts.timeout, Some(15));
    }

    #[test]
    fn test_ephemeral_skips_build_user_prompt() {
        // In ephemeral mode, the executor passes stdin_content directly as -p,
        // NOT through build_user_prompt. Verify build_user_prompt output differs
        // from the raw stdin_content.
        let opts = AgentOpts {
            slug: "test".to_string(),
            worktree_path: PathBuf::from("/tmp"),
            prompt: String::new(),
            directive: Directive::Implement,
            log_file: None,
            env: HashMap::new(),
            session_name: "test".to_string(),
            dispatch_id: "test".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 1,
            max_budget_usd: 0.50,
            stdin_content: Some("Generate a commit message".to_string()),
            ephemeral: true,
            timeout: Some(15),
        };
        // build_user_prompt wraps in "Directive: ...\nTask: ..." — ephemeral mode
        // should NOT use this wrapper.
        let wrapped = ClaudeExecutor::build_user_prompt(&opts);
        assert!(wrapped.contains("Directive: implement"));
        // The raw content "Generate a commit message" should NOT equal the wrapped prompt
        assert_ne!(
            opts.stdin_content.as_deref().unwrap(),
            wrapped,
            "ephemeral mode should pass stdin_content directly, not through build_user_prompt"
        );
    }
}
