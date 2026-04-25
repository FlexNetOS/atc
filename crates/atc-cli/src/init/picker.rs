//! Interactive multi-select picker for `atc init` (TTY mode).
//!
//! The picker is a thin wrapper over [`super::agents::run_init_agent`] — it
//! resolves *which* agents to wire by asking the user, then delegates to the
//! same install core the flag-driven path uses. No business logic lives here.
//!
//! [`build_options`] and [`apply_selection`] are kept pure (no TTY) so the
//! list-rendering and the dispatch-to-`run_init_agent` step are unit-testable
//! without scripting a real terminal.

use anyhow::Result;

use super::agents::{
    agent_status, run_init_agent, AgentEntry, AgentOpts, AgentStatus, AGENT_REGISTRY,
};
use std::path::Path;

/// One row of the picker, including pre-rendered label and default-selection state.
#[derive(Debug, Clone)]
pub struct AgentOption {
    pub entry: &'static AgentEntry,
    pub status: AgentStatus,
    pub label: String,
    /// Whether the row is selectable. `ParentMissing` is filtered out.
    pub selectable: bool,
    /// Whether the row is pre-checked (`Available` only).
    pub default_selected: bool,
}

/// Build options for every entry in the registry. Pure: only filesystem state.
pub fn build_options(base: &Path) -> Vec<AgentOption> {
    AGENT_REGISTRY
        .iter()
        .map(|entry| {
            let status = agent_status(base, entry);
            let glyph = match &status {
                AgentStatus::Wired => "✓ wired",
                AgentStatus::WrongTarget(_) => "↯ wrong target",
                AgentStatus::UserDir => "⚠ user dir",
                AgentStatus::Copied => "● copied",
                AgentStatus::ParentMissing => "·  parent missing",
                AgentStatus::Available => "+  available",
            };
            let label = format!(
                "{name:<8}  {target:<24}  {glyph}",
                name = entry.name,
                target = entry.target_dir,
            );
            let selectable = !matches!(status, AgentStatus::ParentMissing);
            let default_selected = matches!(status, AgentStatus::Available);
            AgentOption {
                entry,
                status,
                label,
                selectable,
                default_selected,
            }
        })
        .collect()
}

/// Render the picker rows as a single string, for golden-test snapshots.
pub fn render_options(opts: &[AgentOption]) -> String {
    let mut out = String::new();
    for o in opts {
        let mark = if !o.selectable {
            "    "
        } else if o.default_selected {
            "[x] "
        } else {
            "[ ] "
        };
        out.push_str(mark);
        out.push_str(&o.label);
        out.push('\n');
    }
    out
}

/// Apply a selection by calling [`run_init_agent`] for each chosen agent.
/// `force_for_wrong_target` lets the picker auto-`--force` rows the user
/// selected that already have a wrong-target symlink, without requiring the
/// user to retype the flag.
pub fn apply_selection(
    base: &Path,
    selected: &[&AgentOption],
    copy: bool,
    force: bool,
) -> Result<()> {
    let mut failures: Vec<(String, String)> = Vec::new();
    for opt in selected {
        let force_this = force || matches!(opt.status, AgentStatus::WrongTarget(_));
        let agent_opts = AgentOpts {
            force: force_this,
            copy,
        };
        if let Err(e) = run_init_agent(base, opt.entry.name, agent_opts) {
            failures.push((opt.entry.name.to_string(), e.to_string()));
        }
    }
    if !failures.is_empty() {
        for (name, msg) in &failures {
            eprintln!("  {name}: {msg}");
        }
        anyhow::bail!("{} agent(s) failed to wire", failures.len());
    }
    Ok(())
}

/// Run the interactive picker. No-op (returns Ok) if no selectable rows exist.
pub fn run_picker(base: &Path) -> Result<()> {
    let options = build_options(base);
    let selectable: Vec<&AgentOption> = options.iter().filter(|o| o.selectable).collect();

    if selectable.is_empty() {
        eprintln!(
            "atc init: no agent skill directories detected. \
            Create one (e.g. mkdir -p .claude/skills) and re-run `atc init --interactive`."
        );
        return Ok(());
    }

    let labels: Vec<String> = selectable.iter().map(|o| o.label.clone()).collect();
    let defaults: Vec<usize> = selectable
        .iter()
        .enumerate()
        .filter(|(_, o)| o.default_selected)
        .map(|(i, _)| i)
        .collect();

    let prompt = inquire::MultiSelect::new("Wire ATC skills into:", labels)
        .with_default(&defaults)
        .with_help_message(
            "space toggle · a select all · n select none · enter confirm · esc cancel",
        );

    let chosen_labels = match prompt.prompt() {
        Ok(v) => v,
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => {
            eprintln!("atc init: cancelled — no agents wired.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let chosen: Vec<&AgentOption> = selectable
        .iter()
        .copied()
        .filter(|o| chosen_labels.contains(&o.label))
        .collect();

    apply_selection(base, &chosen, false, false)?;

    // Print the resulting status table for confirmation.
    super::agents::list_agents(base)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::scaffold::DEFAULT_SKILLS;

    fn fake_base(make_claude_parent: bool, make_agents_parent: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Always create .atc/skills so run_init_agent's pre-flight passes.
        let s = dir.path().join(".atc/skills");
        std::fs::create_dir_all(&s).unwrap();
        for (name, content) in DEFAULT_SKILLS {
            std::fs::write(s.join(name), content).unwrap();
        }
        if make_claude_parent {
            std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();
        }
        if make_agents_parent {
            std::fs::create_dir_all(dir.path().join(".agents/skills")).unwrap();
        }
        dir
    }

    #[test]
    fn build_options_classifies_parent_missing() {
        let dir = fake_base(false, false);
        let opts = build_options(dir.path());
        assert_eq!(opts.len(), AGENT_REGISTRY.len());
        for o in &opts {
            assert_eq!(o.status, AgentStatus::ParentMissing);
            assert!(!o.selectable);
            assert!(!o.default_selected);
        }
    }

    #[test]
    fn build_options_marks_available_default_selected() {
        let dir = fake_base(true, false);
        let opts = build_options(dir.path());

        let claude = opts.iter().find(|o| o.entry.name == "claude").unwrap();
        assert_eq!(claude.status, AgentStatus::Available);
        assert!(claude.selectable);
        assert!(claude.default_selected);

        let agents = opts.iter().find(|o| o.entry.name == "agents").unwrap();
        assert_eq!(agents.status, AgentStatus::ParentMissing);
        assert!(!agents.selectable);
    }

    #[test]
    fn render_options_golden_no_parents() {
        let dir = fake_base(false, false);
        let opts = build_options(dir.path());
        let rendered = render_options(&opts);
        // Both rows should be parent-missing and unselectable.
        assert!(rendered.contains("parent missing"));
        assert!(!rendered.contains("[x]"));
        assert!(!rendered.contains("[ ]"));
    }

    #[test]
    fn render_options_golden_one_available() {
        let dir = fake_base(true, false);
        let opts = build_options(dir.path());
        let rendered = render_options(&opts);
        assert!(rendered.contains("[x]"));
        assert!(rendered.contains("available"));
        assert!(rendered.contains("parent missing"));
    }

    #[test]
    fn apply_selection_calls_run_init_agent() {
        // This is the picker→install smoke test: handing a selection to
        // apply_selection must produce the same on-disk result as calling
        // run_init_agent through the flag path.
        let dir = fake_base(true, true);
        let opts = build_options(dir.path());
        let selected: Vec<&AgentOption> = opts.iter().filter(|o| o.selectable).collect();
        apply_selection(dir.path(), &selected, false, false).unwrap();

        // Every selectable agent should now be wired.
        for o in &selected {
            let target = dir.path().join(o.entry.target_dir);
            let meta = std::fs::symlink_metadata(&target).unwrap();
            assert!(
                meta.file_type().is_symlink(),
                "expected symlink at {}",
                target.display()
            );
        }

        // Same call as `atc init claude` from the flag path:
        let entry = AGENT_REGISTRY.iter().find(|e| e.name == "claude").unwrap();
        let target = dir.path().join(entry.target_dir);
        let link = std::fs::read_link(&target).unwrap();
        assert_eq!(link, std::path::PathBuf::from("../../.atc/skills"));
    }

    #[test]
    fn apply_selection_auto_force_for_wrong_target() {
        let dir = fake_base(true, false);
        let entry = AGENT_REGISTRY.iter().find(|e| e.name == "claude").unwrap();

        // Plant a wrong-target symlink.
        let target = dir.path().join(entry.target_dir);
        let bogus = dir.path().join("somewhere");
        std::fs::create_dir(&bogus).unwrap();
        std::os::unix::fs::symlink("../../somewhere", &target).unwrap();

        // Re-render — now status should be WrongTarget.
        let opts = build_options(dir.path());
        let claude = opts.iter().find(|o| o.entry.name == "claude").unwrap();
        assert!(matches!(claude.status, AgentStatus::WrongTarget(_)));

        // Selecting a wrong-target row applies --force automatically.
        apply_selection(dir.path(), &[claude], false, false).unwrap();
        let link = std::fs::read_link(&target).unwrap();
        assert_eq!(link, std::path::PathBuf::from("../../.atc/skills"));
    }
}
