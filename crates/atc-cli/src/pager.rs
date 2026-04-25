//! Pager support for long human-facing CLI output.
//!
//! Ported from `gitkb-cli/src/pager.rs` — the `dup2`-based stdout redirection
//! pattern is small and self-contained. A shared `cli-utils` extraction is
//! deferred until a third CLI in the org needs it.
//!
//! Commands that produce long output (`status`, `history`, `info`, `health`,
//! `logs` non-follow) can pipe through a configurable pager program.
//!
//! # Precedence
//!
//! 1. `ATC_PAGER` env var (set to `cat` or empty to disable)
//! 2. `pager.plain` in `.atc/config.toml`
//! 3. `PAGER` env var
//! 4. Built-in default: `less -R +G` (`-R` keeps ANSI colors, `+G` opens at end)
//!
//! # When the pager is bypassed
//!
//! - stdout is not a TTY (piped, redirected)
//! - `--no-pager` flag (sets `ATC_NO_PAGER=1`)
//! - `--json` mode (set explicitly by callers)
//! - `ATC_CI=true` (CI environment)
//! - `ATC_PAGER=cat` (escape hatch)

use std::io::IsTerminal;
use std::process::{Child, Command, Stdio};

use atc_core::config::PagerConfig;

/// Built-in default pager — keeps ANSI colors with `-R` and opens at the end
/// with `+G` so the newest-at-bottom render order is visible immediately.
pub const DEFAULT_PAGER: &str = "less -R +G";

/// Guard that owns the pager child process.
///
/// Restores the original stdout fd and waits for the pager to exit when dropped.
#[must_use = "PagerGuard restores stdout on drop; dropping it immediately undoes the redirection"]
pub struct PagerGuard {
    child: Child,
    #[cfg(unix)]
    saved_stdout: std::os::unix::io::RawFd,
}

impl Drop for PagerGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::io::Write;
            let _ = std::io::stdout().flush();

            use std::os::unix::io::RawFd;
            let stdout_fd: RawFd = 1;
            unsafe {
                libc::dup2(self.saved_stdout, stdout_fd);
                libc::close(self.saved_stdout);
            }
        }

        // Drop child stdin to close the pipe write end so the pager sees EOF.
        drop(self.child.stdin.take());

        // Wait for pager to exit — don't leave zombies.
        let _ = self.child.wait();
    }
}

/// Resolve the effective pager command.
///
/// Returns `None` if the resolved value is empty or `cat` (explicit disable).
pub fn resolve_pager(config: Option<&PagerConfig>) -> Option<String> {
    let pager = std::env::var("ATC_PAGER")
        .ok()
        .or_else(|| config.and_then(|c| c.plain.clone()))
        .or_else(|| std::env::var("PAGER").ok())
        .unwrap_or_else(|| DEFAULT_PAGER.to_string());

    let trimmed = pager.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("cat") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Returns true if the CI/no-pager environment forbids paging.
pub fn pager_blocked_by_env() -> bool {
    if std::env::var("ATC_NO_PAGER").is_ok() {
        return true;
    }
    if std::env::var("ATC_CI")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        return true;
    }
    false
}

/// Set up pager redirection for the current process.
///
/// Returns `None` if paging is skipped (no TTY, no pager configured, env block, etc.).
#[must_use = "PagerGuard restores stdout on drop; binding to `_` immediately undoes the redirection"]
pub fn setup_pager(config: Option<&PagerConfig>) -> Option<PagerGuard> {
    if !std::io::stdout().is_terminal() {
        return None;
    }

    if pager_blocked_by_env() {
        return None;
    }

    let pager_cmd = resolve_pager(config)?;

    #[cfg(not(unix))]
    {
        let _ = pager_cmd;
        None
    }

    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, RawFd};

        let mut child = Command::new("sh")
            .args(["-c", &pager_cmd])
            .stdin(Stdio::piped())
            .spawn()
            .ok()?;

        let child_stdin = match child.stdin.as_ref() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        let pipe_fd = child_stdin.as_raw_fd();
        let stdout_fd: RawFd = 1;

        let saved_stdout = unsafe { libc::dup(stdout_fd) };
        if saved_stdout < 0 {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let dup2_rc = unsafe { libc::dup2(pipe_fd, stdout_fd) };
        if dup2_rc < 0 {
            unsafe { libc::close(saved_stdout) };
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }

        Some(PagerGuard {
            child,
            saved_stdout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const ENV_KEYS: [&str; 4] = ["ATC_PAGER", "ATC_NO_PAGER", "ATC_CI", "PAGER"];

    struct EnvRestore(Vec<(&'static str, Option<String>)>);

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, original) in &self.0 {
                match original {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn with_clean_env<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot: Vec<_> = ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        let _restore = EnvRestore(snapshot);
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        f()
    }

    #[test]
    fn resolve_default_when_nothing_set() {
        with_clean_env(|| {
            assert_eq!(resolve_pager(None), Some(DEFAULT_PAGER.to_string()));
        });
    }

    #[test]
    fn resolve_atc_pager_overrides() {
        with_clean_env(|| {
            std::env::set_var("ATC_PAGER", "bat");
            assert_eq!(resolve_pager(None), Some("bat".to_string()));
        });
    }

    #[test]
    fn resolve_config_used_when_env_unset() {
        with_clean_env(|| {
            let config = PagerConfig {
                plain: Some("less -FRX".to_string()),
            };
            assert_eq!(resolve_pager(Some(&config)), Some("less -FRX".to_string()));
        });
    }

    #[test]
    fn resolve_pager_env_fallback() {
        with_clean_env(|| {
            std::env::set_var("PAGER", "more");
            assert_eq!(resolve_pager(None), Some("more".to_string()));
        });
    }

    #[test]
    fn resolve_cat_disables() {
        with_clean_env(|| {
            std::env::set_var("ATC_PAGER", "cat");
            assert!(resolve_pager(None).is_none());
        });
    }

    #[test]
    fn resolve_empty_disables() {
        with_clean_env(|| {
            std::env::set_var("ATC_PAGER", "");
            assert!(resolve_pager(None).is_none());
        });
    }

    #[test]
    fn pager_blocked_by_atc_no_pager() {
        with_clean_env(|| {
            std::env::set_var("ATC_NO_PAGER", "1");
            assert!(pager_blocked_by_env());
        });
    }

    #[test]
    fn pager_blocked_by_atc_ci() {
        with_clean_env(|| {
            std::env::set_var("ATC_CI", "true");
            assert!(pager_blocked_by_env());
        });
    }

    #[test]
    fn pager_not_blocked_when_atc_ci_false() {
        with_clean_env(|| {
            std::env::set_var("ATC_CI", "false");
            assert!(!pager_blocked_by_env());
        });
    }

    #[test]
    fn setup_pager_returns_none_when_stdout_not_a_tty() {
        // In `cargo test` stdout is not a TTY (it's captured), so setup_pager
        // must short-circuit before any fork/dup. Locks the bypass invariant
        // for piped/redirected runs and CI.
        with_clean_env(|| {
            // Even with a real pager configured, no-tty short-circuits first.
            std::env::set_var("ATC_PAGER", "less -R");
            assert!(setup_pager(None).is_none());
        });
    }
}
