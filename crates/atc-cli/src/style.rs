//! Color and style helpers for human-facing CLI output.
//!
//! Honors `NO_COLOR` (any value disables), `CLICOLOR_FORCE=1` (force-on),
//! and the `--color {auto,always,never}` flag set by `main`.
//!
//! # Usage
//!
//! ```ignore
//! use crate::style::{status_style, dim};
//!
//! let s = status_style(Status::Running).render("running");
//! let header = dim().render("dispatched_at");
//! ```

use atc_core::types::{Status, WorkUnitStatus};
use owo_colors::{OwoColorize, Style};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

/// Color mode controlling how `apply_color` decides to emit ANSI escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Detect from TTY + `NO_COLOR`.
    Auto,
    /// Always emit ANSI codes.
    Always,
    /// Never emit ANSI codes.
    Never,
}

impl std::str::FromStr for ColorMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            other => Err(anyhow::anyhow!(
                "invalid --color value '{other}' (expected auto, always, or never)"
            )),
        }
    }
}

// 0 = auto (default), 1 = always, 2 = never
static COLOR_MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_color_mode(mode: ColorMode) {
    let v = match mode {
        ColorMode::Auto => 0,
        ColorMode::Always => 1,
        ColorMode::Never => 2,
    };
    COLOR_MODE.store(v, Ordering::Relaxed);
}

fn current_mode() -> ColorMode {
    match COLOR_MODE.load(Ordering::Relaxed) {
        1 => ColorMode::Always,
        2 => ColorMode::Never,
        _ => ColorMode::Auto,
    }
}

/// Returns true if styled output should be emitted right now.
pub fn colors_enabled() -> bool {
    match current_mode() {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if std::env::var("CLICOLOR_FORCE")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                return true;
            }
            std::io::stdout().is_terminal()
        }
    }
}

/// Apply the style to a value, returning a styled string. When colors are
/// disabled, returns the bare display form.
pub fn apply<T: std::fmt::Display>(value: T, style: Style) -> String {
    if colors_enabled() {
        value.style(style).to_string()
    } else {
        value.to_string()
    }
}

/// Style for a status cell.
pub fn status_style(status: Status) -> Style {
    let s = Style::new();
    match status {
        Status::Running => s.cyan(),
        Status::Done => s.green(),
        Status::Failed => s.red().bold(),
        Status::NeedsHuman => s.yellow(),
        Status::NeedsReview => s.magenta(),
        Status::Stopped => s.dimmed(),
        Status::Retrying => s.blue(),
    }
}

/// Style for a work-unit status cell. Mirrors `status_style` for dispatches.
pub fn work_unit_status_style(status: WorkUnitStatus) -> Style {
    let s = Style::new();
    match status {
        WorkUnitStatus::Active => s.cyan(),
        WorkUnitStatus::Merged => s.green(),
        WorkUnitStatus::Closed => s.dimmed(),
        WorkUnitStatus::Abandoned => s.red(),
    }
}

/// Style for a USD cost cell. Threshold-based emphasis.
pub fn cost_style(cost: Option<f64>) -> Style {
    let s = Style::new();
    match cost {
        Some(c) if c >= 50.0 => s.red(),
        Some(c) if c >= 20.0 => s.yellow(),
        _ => s,
    }
}

/// Dim style — for chrome (headers, separators, "-" placeholders).
pub fn dim() -> Style {
    Style::new().dimmed()
}

/// Bold style — for content emphasis (task slug, primary anchors).
pub fn strong() -> Style {
    Style::new().bold()
}

/// Convenience: render a status as colored text.
pub fn render_status(status: Status) -> String {
    apply(status.as_str(), status_style(status))
}

/// Convenience: render a work-unit status as colored text.
pub fn render_work_unit_status(status: WorkUnitStatus) -> String {
    apply(status.as_str(), work_unit_status_style(status))
}

/// Convenience: render a cost as colored text. Returns "-" when None.
pub fn render_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => apply(format!("${:.2}", c), cost_style(Some(c))),
        None => apply("-", dim()),
    }
}

/// Convenience: render a placeholder dash.
pub fn render_dash() -> String {
    apply("-", dim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static MODE_LOCK: Mutex<()> = Mutex::new(());

    fn with_mode<F: FnOnce()>(mode: ColorMode, f: F) {
        let _g = MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = current_mode();
        set_color_mode(mode);
        f();
        set_color_mode(prev);
    }

    #[test]
    fn always_emits_ansi() {
        with_mode(ColorMode::Always, || {
            let out = apply("hello", Style::new().red());
            assert!(out.contains("\x1b["), "expected ANSI escape, got {out:?}");
        });
    }

    #[test]
    fn never_emits_no_ansi() {
        with_mode(ColorMode::Never, || {
            let out = apply("hello", Style::new().red());
            assert_eq!(out, "hello");
        });
    }

    #[test]
    fn render_status_never_is_plain() {
        with_mode(ColorMode::Never, || {
            assert_eq!(render_status(Status::Running), "running");
            assert_eq!(render_status(Status::Done), "done");
        });
    }

    #[test]
    fn render_work_unit_status_never_is_plain() {
        with_mode(ColorMode::Never, || {
            assert_eq!(render_work_unit_status(WorkUnitStatus::Active), "active");
            assert_eq!(render_work_unit_status(WorkUnitStatus::Merged), "merged");
            assert_eq!(render_work_unit_status(WorkUnitStatus::Closed), "closed");
            assert_eq!(
                render_work_unit_status(WorkUnitStatus::Abandoned),
                "abandoned"
            );
        });
    }

    #[test]
    fn render_work_unit_status_always_emits_ansi() {
        with_mode(ColorMode::Always, || {
            // Each variant should produce an ANSI escape with its mapped color.
            for s in [
                WorkUnitStatus::Active,
                WorkUnitStatus::Merged,
                WorkUnitStatus::Closed,
                WorkUnitStatus::Abandoned,
            ] {
                let out = render_work_unit_status(s);
                assert!(
                    out.contains("\x1b["),
                    "expected ANSI escape for {s:?}, got {out:?}"
                );
                assert!(out.contains(s.as_str()));
            }
        });
    }

    #[test]
    fn render_cost_threshold_styles() {
        with_mode(ColorMode::Always, || {
            assert!(render_cost(Some(60.0)).contains("\x1b["));
            assert!(render_cost(Some(25.0)).contains("\x1b["));
            // Low cost still emits a (default) style; we only verify the value
            assert!(render_cost(Some(1.0)).contains("$1.00"));
            assert!(render_cost(None).contains('-'));
        });
    }

    #[test]
    fn color_mode_parses() {
        assert_eq!("auto".parse::<ColorMode>().unwrap(), ColorMode::Auto);
        assert_eq!("ALWAYS".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("never".parse::<ColorMode>().unwrap(), ColorMode::Never);
        assert!("nope".parse::<ColorMode>().is_err());
    }
}
