use crate::types::Mode;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{info, warn};

#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle>;
}

pub struct AgentOpts {
    pub slug: String,
    pub worktree_path: PathBuf,
    pub prompt: String, // rendered system prompt for the mode
    pub mode: Mode,
    pub log_file: PathBuf,            // stream-json output destination
    pub env: HashMap<String, String>, // GITKB_WORKSPACE, GITKB_ROOT, etc.
    pub session_name: String,         // tmux session name (derived from slug)
    pub dispatch_id: String,          // stable registry ID (used for post-complete --id)
    pub sandbox: bool, // false = pass --settings with sandbox.enabled=false to claude
    pub inline: bool,  // true = CI mode, no tmux, run synchronously
    pub max_turns: u32,
    pub max_budget_usd: f64,
    /// Pre-built stdin content from the pipeline (for non-task dispatches).
    /// When set, the executor pipes this directly to claude stdin instead of
    /// calling `git kb show`. When None, falls back to fetching the task
    /// document from git-kb (legacy task dispatch path).
    pub stdin_content: Option<String>,
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
    async fn write_sandbox_settings(path: &std::path::Path) -> Result<()> {
        let settings = r#"{"sandbox":{"enabled":false}}"#;
        tokio::fs::write(path, settings).await?;
        Ok(())
    }

    /// CI mode: run claude synchronously, capture exit code.
    #[tracing::instrument(skip(self, opts), fields(slug = %opts.slug, session = %opts.session_name))]
    async fn spawn_inline(&self, opts: &AgentOpts) -> Result<AgentHandle> {
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

            let task_doc = Command::new("git-kb")
                .args(["show", &opts.slug])
                .env("GITKB_ROOT", kb_root)
                .output()
                .await?;

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
        if let Some(parent) = opts.log_file.parent() {
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
            stdin.write_all(&stdin_bytes).await?;
            drop(stdin);
        }

        // 8. Stream stdout and stderr to log file with interleaving preserved
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let log_file_handle = tokio::fs::File::create(&opts.log_file).await?;
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
        let status = child.wait().await?;

        // 9. Extract exit code
        let exit_code = status.code().unwrap_or(-1);
        info!(slug = %opts.slug, exit_code, "inline spawn completed");

        // Temp files cleaned up on drop

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(exit_code),
        })
    }

    /// Local mode: create a named tmux session, return immediately.
    #[tracing::instrument(skip(self, opts), fields(slug = %opts.slug, session = %opts.session_name))]
    async fn spawn_tmux(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        use tokio::process::Command;

        // 1. Write system prompt to a stable path (must outlive this process)
        let log_dir = opts
            .log_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        tokio::fs::create_dir_all(log_dir).await?;

        let prompt_path = log_dir.join(format!("{}.prompt.md", opts.session_name));
        tokio::fs::write(&prompt_path, &opts.prompt).await?;

        // 2. Optionally write sandbox settings
        let sandbox_path = if !opts.sandbox {
            let p = log_dir.join(format!("{}.sandbox.json", opts.session_name));
            Self::write_sandbox_settings(&p).await?;
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

        // Ensure pipe failures propagate (so EXIT_CODE captures claude's exit, not tee's)
        bash_parts.push("set -o pipefail".to_string());

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

        // Write stdin content to a temp file to avoid shell expansion.
        // Writing to a file (rather than a shell variable + echo) prevents
        // command injection via $(), backticks, or other expansion in the content.
        let task_doc_path = log_dir.join(format!("{}.taskdoc", opts.session_name));
        let task_doc_path_str = task_doc_path.to_string_lossy();

        if let Some(ref content) = opts.stdin_content {
            // Pre-built stdin content: write directly to the temp file
            tokio::fs::write(&task_doc_path, content).await?;
        } else {
            // Legacy path: fetch task document from git-kb (task dispatches only)
            let kb_root = opts
                .env
                .get("GITKB_ROOT")
                .ok_or_else(|| anyhow::anyhow!("GITKB_ROOT not set in agent env"))?;
            bash_parts.push(format!(
                "GITKB_ROOT='{}' git-kb show '{}' > '{}' || {{ echo 'error: git-kb show failed' >&2 ; exit 1 ; }}",
                shell_escape(kb_root)?,
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

        if let Some(ref sp) = sandbox_path {
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

        // Cleanup temp files
        bash_parts.push(format!("rm -f '{}'", shell_escape(&prompt_path_str)?));
        bash_parts.push(format!("rm -f '{}'", shell_escape(&task_doc_path_str)?));
        if let Some(ref sp) = sandbox_path {
            bash_parts.push(format!("rm -f '{}'", shell_escape(&sp.to_string_lossy())?));
        }
        bash_parts.push("exit $EXIT_CODE".to_string());

        let bash_body = bash_parts.join(" ; ");

        // 4. Create tmux session
        info!(session = %opts.session_name, "creating tmux session");
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
            dispatch_id: "test".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 10_000,
            max_budget_usd: 25.0,
            stdin_content: None,
        };
        let prompt = ClaudeExecutor::build_user_prompt(&opts);
        assert!(prompt.contains("Directive: implement"));
        assert!(prompt.contains("Task: tasks/gitkb-42"));
        assert!(prompt.contains("Working directory: /tmp/worktrees/gitkb/core"));
        assert!(prompt.contains("The task document follows on stdin"));
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
            mode: Mode::Implement,
            log_file: PathBuf::from("/tmp/test.jsonl"),
            env,
            session_name: "test-session".to_string(),
            dispatch_id: "test-dispatch".to_string(),
            sandbox: false,
            inline: true,
            max_turns: 100,
            max_budget_usd: 5.0,
            stdin_content,
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
        let opts = make_test_opts(
            Some("Hello from stdin content".to_string()),
            HashMap::new(), // No GITKB_ROOT
        );

        // This should not fail with "GITKB_ROOT not set" error
        let result = executor.spawn_inline(&opts).await;
        // It may fail because 'echo' doesn't behave exactly like claude,
        // but it should NOT fail with a GITKB_ROOT error
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("GITKB_ROOT"),
                "should not require GITKB_ROOT when stdin_content is set, got: {}",
                msg
            );
        }
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

    #[tokio::test]
    async fn test_spawn_tmux_with_stdin_content_skips_gitkb() {
        // When stdin_content is set, tmux spawn should NOT require GITKB_ROOT
        // for the git-kb show step (it writes content directly to file)
        let executor = ClaudeExecutor::default();
        let tmp = tempfile::tempdir().unwrap();
        let log_file = tmp.path().join("test.jsonl");
        let opts = AgentOpts {
            stdin_content: Some("Hello from stdin content".to_string()),
            env: HashMap::new(), // No GITKB_ROOT
            log_file,
            worktree_path: tmp.path().to_path_buf(),
            ..make_test_opts(None, HashMap::new())
        };

        // spawn_tmux will likely fail because tmux isn't available in test,
        // but it should NOT fail with "GITKB_ROOT not set"
        let result = executor.spawn_tmux(&opts).await;
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("GITKB_ROOT"),
                "should not require GITKB_ROOT when stdin_content is set, got: {}",
                msg
            );
        }
    }

    #[tokio::test]
    async fn test_spawn_tmux_without_stdin_content_requires_gitkb_root() {
        // When stdin_content is None, tmux spawn should require GITKB_ROOT
        let executor = ClaudeExecutor::default();
        let tmp = tempfile::tempdir().unwrap();
        let log_file = tmp.path().join("test.jsonl");
        let opts = AgentOpts {
            stdin_content: None,
            env: HashMap::new(), // No GITKB_ROOT
            log_file,
            worktree_path: tmp.path().to_path_buf(),
            ..make_test_opts(None, HashMap::new())
        };

        let result = executor.spawn_tmux(&opts).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("GITKB_ROOT not set"),
            "should require GITKB_ROOT when stdin_content is None"
        );
    }
}
