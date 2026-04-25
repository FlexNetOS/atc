//! Agent registry and wire-up logic for `atc init <agent>`.
//!
//! The registry is a small static array describing each supported coding agent's
//! skills convention (e.g. Claude Code reads `.claude/skills/`, the generic
//! `.agents/skills/` convention, …). Wiring up an agent means creating a
//! relative directory-level symlink (or copy fallback) from
//! `<agent>/skills/atc` -> `.atc/skills`, so any skill file written by `atc init`
//! is automatically visible to the agent without re-running anything.
//!
//! The core function is [`run_init_agent`]. The CLI flag path and the interactive
//! [`super::picker`] both call it — there is no duplicated install logic.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::scaffold::DEFAULT_SKILLS;

/// Marker file written into copy-mode targets so we can later confirm a directory
/// is ATC-managed without depending on filename heuristics. The file body is a
/// short identifier so a casual `cat` reveals what produced the directory.
const COPY_MARKER_FILENAME: &str = ".atc-skills-managed";
const COPY_MARKER_BODY: &str = "atc init copy-mode marker; do not edit\n";

/// Static description of one supported coding agent.
#[derive(Debug, Clone, Copy)]
pub struct AgentEntry {
    /// Identifier used as the positional arg to `atc init <agent>`.
    pub name: &'static str,
    /// Target path relative to project root (e.g. `.claude/skills/atc`).
    pub target_dir: &'static str,
    /// Parent dir that must exist before wiring (e.g. `.claude/skills`).
    /// `--all-agents` skips entries whose parent dir is missing.
    pub parent_dir: &'static str,
    /// Human-friendly description.
    pub description: &'static str,
}

/// All agents wired by `atc init <agent>` and `atc init --all-agents`.
///
/// v1 entries: `claude` and `agents`. Other agents (codex, cursor, aider, …) are
/// deferred until each agent's skills convention is confirmed.
pub const AGENT_REGISTRY: &[AgentEntry] = &[
    AgentEntry {
        name: "claude",
        target_dir: ".claude/skills/atc",
        parent_dir: ".claude/skills",
        description: "Claude Code skills directory",
    },
    AgentEntry {
        name: "agents",
        target_dir: ".agents/skills/atc",
        parent_dir: ".agents/skills",
        description: "Generic .agents/ skills convention",
    },
];

/// Runtime options for `run_init_agent`.
#[derive(Debug, Clone, Default)]
pub struct AgentOpts {
    /// Replace a wrong-target symlink (does NOT replace a real directory).
    pub force: bool,
    /// Copy files instead of symlinking; subsequent runs mirror `.atc/skills/`.
    pub copy: bool,
}

/// Snapshot of an agent's wire-up state at a point in time. Drives the picker
/// status column and makes idempotent decisions explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Symlink at target_dir already points at `.atc/skills` (relative form).
    Wired,
    /// Symlink at target_dir points elsewhere; need `--force` to replace.
    WrongTarget(PathBuf),
    /// Target is a real directory or file (user content). Refuse to delete.
    UserDir,
    /// Target is a real directory holding a copy mirror of `.atc/skills/`.
    /// Treated as ATC-managed in copy mode.
    Copied,
    /// Agent's parent directory is missing — pre-deselected in the picker.
    ParentMissing,
    /// Parent dir exists, target is absent. Ready to wire.
    Available,
}

/// Result of a successful `run_init_agent` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOutcome {
    /// New symlink or copy created.
    Created,
    /// Already correct — nothing changed.
    AlreadyWired,
    /// Wrong-target symlink replaced under `--force`.
    Replaced,
    /// Symlink syscall failed; fell back to copy and printed a warning.
    SymlinkFallbackToCopy,
    /// Copy mirror brought into sync (added/updated/removed ATC files only).
    Mirrored,
}

/// Look up a registry entry by name.
pub fn find_agent(name: &str) -> Option<&'static AgentEntry> {
    AGENT_REGISTRY.iter().find(|a| a.name == name)
}

/// Inspect the current state of an agent's target on disk.
pub fn agent_status(base: &Path, entry: &AgentEntry) -> AgentStatus {
    let target = base.join(entry.target_dir);
    let parent = base.join(entry.parent_dir);

    // Use symlink_metadata so we don't follow the symlink itself.
    let meta = match std::fs::symlink_metadata(&target) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => None,
    };

    match meta {
        None => {
            if parent.exists() {
                AgentStatus::Available
            } else {
                AgentStatus::ParentMissing
            }
        }
        Some(m) if m.file_type().is_symlink() => {
            let link_target = std::fs::read_link(&target).unwrap_or_else(|_| PathBuf::new());
            let expected = expected_symlink_target(entry);
            if link_target == expected {
                AgentStatus::Wired
            } else {
                AgentStatus::WrongTarget(link_target)
            }
        }
        Some(m) if m.is_dir() => {
            if is_atc_skills_copy(&target) {
                AgentStatus::Copied
            } else {
                AgentStatus::UserDir
            }
        }
        Some(_) => AgentStatus::UserDir,
    }
}

/// Wire `.atc/skills/` into a single agent's skills directory.
///
/// See [`AgentStatus`] for the idempotency rules:
/// - absent → create
/// - correct symlink → no-op (`AlreadyWired`)
/// - wrong-target symlink → replace if `force`, else error
/// - real user directory → refuse, regardless of `force`
pub fn run_init_agent(base: &Path, agent_name: &str, opts: AgentOpts) -> Result<WireOutcome> {
    let entry = find_agent(agent_name).ok_or_else(|| {
        anyhow!("unknown agent '{agent_name}'. Run 'atc init --list-agents' for supported agents.")
    })?;

    let skills_src = base.join(".atc").join("skills");
    if !skills_src.exists() {
        bail!(
            "{} does not exist. Run 'atc init' first to scaffold .atc/.",
            skills_src.display()
        );
    }

    // Ensure the agent's parent dir exists (e.g. .claude/skills/).
    let parent = base.join(entry.parent_dir);
    std::fs::create_dir_all(&parent).with_context(|| {
        format!(
            "failed to create parent dir {} for agent '{agent_name}'",
            parent.display()
        )
    })?;

    let target = base.join(entry.target_dir);
    let status = agent_status(base, entry);

    match (&status, opts.copy) {
        (AgentStatus::Wired, false) => {
            println!("  {agent_name}: already wired (-> .atc/skills)");
            Ok(WireOutcome::AlreadyWired)
        }
        (AgentStatus::Wired, true) => {
            // Symlink exists but user asked for copy. Replace under --force; otherwise error.
            if !opts.force {
                bail!(
                    "{} is a symlink. Use --force to replace it with a copy, or omit --copy to keep the symlink.",
                    target.display()
                );
            }
            std::fs::remove_file(&target).with_context(|| {
                format!("failed to remove existing symlink {}", target.display())
            })?;
            copy_skills(&skills_src, &target)?;
            println!(
                "  {agent_name}: replaced symlink with copy at {}",
                target.display()
            );
            Ok(WireOutcome::Created)
        }
        (AgentStatus::WrongTarget(existing), _) => {
            if !opts.force {
                bail!(
                    "{} is a symlink to {} (expected -> .atc/skills). Re-run with --force to replace.",
                    target.display(),
                    existing.display()
                );
            }
            std::fs::remove_file(&target).with_context(|| {
                format!("failed to remove existing symlink {}", target.display())
            })?;
            wire_fresh(&skills_src, &target, entry, opts.copy)?;
            println!("  {agent_name}: replaced wrong-target symlink");
            Ok(WireOutcome::Replaced)
        }
        (AgentStatus::UserDir, _) => {
            bail!(
                "{} is an existing directory or file with content ATC didn't create. \
                Refusing to delete user content. Back it up and remove it before re-running.",
                target.display()
            );
        }
        (AgentStatus::Copied, true) => {
            // Mirror ATC's skill set into the existing copy.
            mirror_skills(&skills_src, &target)?;
            println!(
                "  {agent_name}: mirrored .atc/skills into {}",
                target.display()
            );
            Ok(WireOutcome::Mirrored)
        }
        (AgentStatus::Copied, false) => {
            // Copy exists but user wants a symlink — only swap with --force.
            if !opts.force {
                bail!(
                    "{} is a copy directory. Use --force to replace it with a symlink (note: any \
                    extra files in the directory are preserved by `--copy --force`, not by symlink mode).",
                    target.display()
                );
            }
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("failed to remove copy directory {}", target.display()))?;
            wire_fresh(&skills_src, &target, entry, false)?;
            println!("  {agent_name}: replaced copy with symlink");
            Ok(WireOutcome::Replaced)
        }
        (AgentStatus::ParentMissing, _) | (AgentStatus::Available, _) => {
            wire_fresh(&skills_src, &target, entry, opts.copy)
        }
    }
}

/// Wire every entry in the registry whose parent dir exists.
pub fn run_init_all_agents(base: &Path, opts: AgentOpts) -> Result<()> {
    let mut wired = 0usize;
    let mut skipped: Vec<&'static str> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    for entry in AGENT_REGISTRY {
        let parent = base.join(entry.parent_dir);
        if !parent.exists() {
            skipped.push(entry.name);
            continue;
        }
        match run_init_agent(base, entry.name, opts.clone()) {
            Ok(_) => wired += 1,
            Err(e) => failed.push((entry.name.to_string(), e.to_string())),
        }
    }

    println!("\n--- atc init --all-agents summary ---");
    println!("  wired:   {wired}");
    if !skipped.is_empty() {
        println!("  skipped: {} (parent dir missing)", skipped.join(", "));
    }
    if !failed.is_empty() {
        println!("  failed:");
        for (name, msg) in &failed {
            println!("    {name}: {msg}");
        }
        bail!("{} agent(s) failed to wire", failed.len());
    }
    Ok(())
}

/// Print the registry as a table (status column reflects current filesystem state).
pub fn list_agents(base: &Path) -> Result<()> {
    use comfy_table::{Cell, Table};

    let mut table = Table::new();
    table.set_header(vec!["Agent", "Target", "Status", "Description"]);
    for entry in AGENT_REGISTRY {
        let status = agent_status(base, entry);
        table.add_row(vec![
            Cell::new(entry.name),
            Cell::new(entry.target_dir),
            Cell::new(format_status(&status)),
            Cell::new(entry.description),
        ]);
    }
    println!("{table}");
    Ok(())
}

/// JSON variant of [`list_agents`] for scripting.
pub fn list_agents_json(base: &Path) -> Result<()> {
    let arr: Vec<_> = AGENT_REGISTRY
        .iter()
        .map(|entry| {
            let status = agent_status(base, entry);
            serde_json::json!({
                "name": entry.name,
                "target_dir": entry.target_dir,
                "parent_dir": entry.parent_dir,
                "description": entry.description,
                "status": format_status(&status),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr)?);
    Ok(())
}

fn format_status(s: &AgentStatus) -> &'static str {
    match s {
        AgentStatus::Wired => "wired",
        AgentStatus::WrongTarget(_) => "wrong-target",
        AgentStatus::UserDir => "user-dir",
        AgentStatus::Copied => "copied",
        AgentStatus::ParentMissing => "parent-missing",
        AgentStatus::Available => "available",
    }
}

// --- internals ---

/// Compute the relative symlink target for an agent. The agent's `target_dir` is
/// e.g. `.claude/skills/atc`; the symlink lives at `BASE/.claude/skills/atc` and
/// must point to `BASE/.atc/skills`. From the symlink's parent (`.claude/skills`)
/// that's `../../.atc/skills`.
fn expected_symlink_target(entry: &AgentEntry) -> PathBuf {
    let depth = Path::new(entry.target_dir)
        .parent()
        .map(|p| p.components().count())
        .unwrap_or(0);
    let mut buf = PathBuf::new();
    for _ in 0..depth {
        buf.push("..");
    }
    buf.push(".atc");
    buf.push("skills");
    buf
}

/// Create a fresh wire (no pre-existing target). On symlink failure, fall back
/// to copy with a stderr warning unless `force_copy` is already set.
fn wire_fresh(
    skills_src: &Path,
    target: &Path,
    entry: &AgentEntry,
    force_copy: bool,
) -> Result<WireOutcome> {
    if force_copy {
        copy_skills(skills_src, target)?;
        println!("  created {} (copy)", target.display());
        return Ok(WireOutcome::Created);
    }

    let link_target = expected_symlink_target(entry);
    match make_symlink(&link_target, target) {
        Ok(()) => {
            println!(
                "  created {} -> {}",
                target.display(),
                link_target.display()
            );
            Ok(WireOutcome::Created)
        }
        Err(e) => {
            eprintln!(
                "warning: symlink failed ({e}); falling back to copy. \
                Re-run with `atc init {} --copy` to make this permanent.",
                entry.name
            );
            copy_skills(skills_src, target)?;
            Ok(WireOutcome::SymlinkFallbackToCopy)
        }
    }
}

#[cfg(unix)]
fn make_symlink(link_target: &Path, link_path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link_target, link_path)
}

#[cfg(windows)]
fn make_symlink(link_target: &Path, link_path: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(link_target, link_path)
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_link_target: &Path, _link_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ))
}

/// Copy every skill file from `skills_src` into `target` (creating it if absent).
///
/// We mirror the on-disk source directory rather than only `DEFAULT_SKILLS`, so
/// user-authored files in `.atc/skills/` are picked up on the first `--copy`
/// run and on the symlink fallback path. Falls back to the embedded content for
/// known skill names that are missing on disk.
fn copy_skills(skills_src: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("failed to create target directory {}", target.display()))?;

    let mut copied: HashSet<String> = HashSet::new();

    let rd = std::fs::read_dir(skills_src)
        .with_context(|| format!("failed to read {}", skills_src.display()))?;
    for entry in rd {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let src = entry.path();
        let dst = target.join(entry.file_name());
        std::fs::copy(&src, &dst)
            .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;
        if let Ok(name) = entry.file_name().into_string() {
            copied.insert(name);
        }
    }

    // Backstop: any embedded skill missing from disk gets restored from the bundle.
    for (name, content) in DEFAULT_SKILLS {
        if copied.contains(*name) {
            continue;
        }
        std::fs::write(target.join(name), content.as_bytes())
            .with_context(|| format!("failed to write {}", target.join(name).display()))?;
    }

    write_marker(target)?;

    Ok(())
}

/// Mirror `skills_src` into `target`: write/overwrite ATC-named files, remove
/// orphaned ATC-named files (files matching the embedded skill set that no
/// longer exist in source). User-added files unrelated to ATC's skill set are
/// never touched.
fn mirror_skills(skills_src: &Path, target: &Path) -> Result<()> {
    let known_names: HashSet<&str> = DEFAULT_SKILLS.iter().map(|(n, _)| *n).collect();
    let src_names: HashSet<String> = std::fs::read_dir(skills_src)
        .with_context(|| format!("failed to read {}", skills_src.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    // Write every source file into the target.
    for name in &src_names {
        let src = skills_src.join(name);
        let dst = target.join(name);
        std::fs::copy(&src, &dst)
            .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;
    }

    // Remove ATC-named files in target that no longer exist in source.
    if let Ok(rd) = std::fs::read_dir(target) {
        for entry in rd.flatten() {
            let name_os = entry.file_name();
            let Some(name) = name_os.to_str() else {
                continue;
            };
            if !known_names.contains(name) {
                continue; // user file — leave alone
            }
            if !src_names.contains(name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    write_marker(target)?;

    Ok(())
}

/// Write the copy-mode marker file. Idempotent — overwriting is fine because
/// the contents are fixed.
fn write_marker(target: &Path) -> Result<()> {
    let marker = target.join(COPY_MARKER_FILENAME);
    std::fs::write(&marker, COPY_MARKER_BODY)
        .with_context(|| format!("failed to write {}", marker.display()))
}

/// A directory is an ATC-skills copy iff it contains the marker file written
/// by [`copy_skills`] / [`mirror_skills`]. Filename-based heuristics produced
/// false positives for any user dir that happened to hold a doc with the same
/// name as an embedded skill (e.g. `dispatch.md`).
fn is_atc_skills_copy(dir: &Path) -> bool {
    dir.join(COPY_MARKER_FILENAME).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skills_dir(root: &Path) {
        let s = root.join(".atc/skills");
        std::fs::create_dir_all(&s).unwrap();
        for (name, content) in DEFAULT_SKILLS {
            std::fs::write(s.join(name), content).unwrap();
        }
    }

    fn make_parent(root: &Path, entry: &AgentEntry) {
        std::fs::create_dir_all(root.join(entry.parent_dir)).unwrap();
    }

    #[test]
    fn registry_has_v1_entries() {
        assert!(find_agent("claude").is_some());
        assert!(find_agent("agents").is_some());
        assert!(find_agent("nonexistent").is_none());
    }

    #[test]
    fn expected_symlink_target_for_claude() {
        let entry = find_agent("claude").unwrap();
        assert_eq!(
            expected_symlink_target(entry),
            PathBuf::from("../../.atc/skills")
        );
    }

    #[test]
    fn expected_symlink_target_for_agents() {
        let entry = find_agent("agents").unwrap();
        assert_eq!(
            expected_symlink_target(entry),
            PathBuf::from("../../.atc/skills")
        );
    }

    #[cfg(unix)]
    #[test]
    fn fresh_wire_creates_relative_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();
        make_parent(base, entry);

        let outcome = run_init_agent(base, "claude", AgentOpts::default()).unwrap();
        assert_eq!(outcome, WireOutcome::Created);

        let target = base.join(entry.target_dir);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_symlink());
        let link = std::fs::read_link(&target).unwrap();
        assert_eq!(link, PathBuf::from("../../.atc/skills"));

        // Through the symlink, we can read the embedded skill files.
        let through = target.join("atc-reference.md");
        let s = std::fs::read_to_string(&through).unwrap();
        assert!(s.contains("ATC Quick Reference"));
    }

    #[cfg(unix)]
    #[test]
    fn rerun_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();
        make_parent(base, entry);

        let _ = run_init_agent(base, "claude", AgentOpts::default()).unwrap();
        let outcome = run_init_agent(base, "claude", AgentOpts::default()).unwrap();
        assert_eq!(outcome, WireOutcome::AlreadyWired);
    }

    #[cfg(unix)]
    #[test]
    fn wrong_target_errors_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();
        make_parent(base, entry);

        // Create a wrong-target symlink.
        let bogus = base.join("somewhere-else");
        std::fs::create_dir(&bogus).unwrap();
        let target = base.join(entry.target_dir);
        std::os::unix::fs::symlink("../../somewhere-else", &target).unwrap();

        let err = run_init_agent(base, "claude", AgentOpts::default()).unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "should mention --force, got: {err}"
        );

        // With --force, it replaces.
        let outcome = run_init_agent(
            base,
            "claude",
            AgentOpts {
                force: true,
                copy: false,
            },
        )
        .unwrap();
        assert_eq!(outcome, WireOutcome::Replaced);
        let link = std::fs::read_link(&target).unwrap();
        assert_eq!(link, PathBuf::from("../../.atc/skills"));
    }

    #[test]
    fn real_user_dir_is_refused_even_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();
        make_parent(base, entry);

        // Create a real user directory at the target with non-ATC content.
        let target = base.join(entry.target_dir);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("user-notes.md"), "my notes").unwrap();

        let err = run_init_agent(
            base,
            "claude",
            AgentOpts {
                force: true,
                copy: false,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Refusing"),
            "should refuse to delete user content, got: {err}"
        );

        // User content untouched.
        assert!(target.join("user-notes.md").exists());
    }

    #[test]
    fn copy_mode_writes_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("agents").unwrap();
        make_parent(base, entry);

        let outcome = run_init_agent(
            base,
            "agents",
            AgentOpts {
                force: false,
                copy: true,
            },
        )
        .unwrap();
        assert_eq!(outcome, WireOutcome::Created);

        let target = base.join(entry.target_dir);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.is_dir(), "copy mode should produce a real directory");
        for (name, _) in DEFAULT_SKILLS {
            let f = target.join(name);
            let m = std::fs::symlink_metadata(&f).unwrap();
            assert!(m.is_file(), "{name} should be a regular file");
        }
    }

    #[test]
    fn copy_mode_includes_user_authored_skill_files() {
        // copy_skills should mirror the on-disk .atc/skills/ directory rather than
        // only the embedded set, so user-authored files like .atc/skills/custom.md
        // appear in the target on the first --copy run.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        std::fs::write(base.join(".atc/skills/custom.md"), "user skill").unwrap();
        let entry = find_agent("agents").unwrap();
        make_parent(base, entry);

        run_init_agent(
            base,
            "agents",
            AgentOpts {
                force: false,
                copy: true,
            },
        )
        .unwrap();

        let target = base.join(entry.target_dir);
        let custom = target.join("custom.md");
        assert!(custom.exists(), "user-added skill file should be copied");
        assert_eq!(std::fs::read_to_string(&custom).unwrap(), "user skill");
    }

    #[test]
    fn copy_mode_mirror_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("agents").unwrap();
        make_parent(base, entry);

        // First copy.
        run_init_agent(
            base,
            "agents",
            AgentOpts {
                force: false,
                copy: true,
            },
        )
        .unwrap();
        let target = base.join(entry.target_dir);

        // Drop a user-added file into the target.
        std::fs::write(target.join("custom.md"), "user content").unwrap();

        // Modify a source skill file and remove one to simulate ATC version drift.
        std::fs::write(base.join(".atc/skills/dispatch.md"), "# updated dispatch\n").unwrap();
        // Simulate "removed in source" by deleting one file from .atc/skills/.
        std::fs::remove_file(base.join(".atc/skills/monitor.md")).unwrap();

        // Re-run mirrors source into target.
        let outcome = run_init_agent(
            base,
            "agents",
            AgentOpts {
                force: false,
                copy: true,
            },
        )
        .unwrap();
        assert_eq!(outcome, WireOutcome::Mirrored);

        // Updated file reflects new content.
        let updated = std::fs::read_to_string(target.join("dispatch.md")).unwrap();
        assert_eq!(updated, "# updated dispatch\n");

        // Removed file (matching ATC name) is also removed from target.
        assert!(!target.join("monitor.md").exists());

        // User-added file is preserved.
        assert!(target.join("custom.md").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("custom.md")).unwrap(),
            "user content"
        );
    }

    #[test]
    fn unknown_agent_errors() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let err = run_init_agent(base, "vscode", AgentOpts::default()).unwrap_err();
        assert!(err.to_string().contains("unknown agent"));
    }

    #[test]
    fn missing_atc_skills_errors() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // Note: we did NOT call make_skills_dir.
        let err = run_init_agent(base, "claude", AgentOpts::default()).unwrap_err();
        assert!(err.to_string().contains("Run 'atc init' first"));
    }

    #[test]
    fn parent_dir_created_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        // Note: do NOT call make_parent — parent should be created on the fly.

        let outcome = run_init_agent(base, "claude", AgentOpts::default()).unwrap();
        assert_eq!(outcome, WireOutcome::Created);
        let entry = find_agent("claude").unwrap();
        assert!(base.join(entry.parent_dir).is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn agent_status_reports_states() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();

        // 1. Parent missing.
        assert_eq!(agent_status(base, entry), AgentStatus::ParentMissing);

        // 2. Available.
        make_parent(base, entry);
        assert_eq!(agent_status(base, entry), AgentStatus::Available);

        // 3. Wired.
        run_init_agent(base, "claude", AgentOpts::default()).unwrap();
        assert_eq!(agent_status(base, entry), AgentStatus::Wired);

        // 4. WrongTarget.
        let target = base.join(entry.target_dir);
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("../../somewhere-else", &target).unwrap();
        assert!(matches!(
            agent_status(base, entry),
            AgentStatus::WrongTarget(_)
        ));

        // 5. UserDir.
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("notes.md"), "user").unwrap();
        assert_eq!(agent_status(base, entry), AgentStatus::UserDir);
    }

    #[test]
    fn user_dir_with_default_skill_filename_is_not_copied() {
        // Regression: is_atc_skills_copy used to return true for any directory
        // containing a file named after an embedded skill (e.g. `dispatch.md`),
        // misclassifying user-owned dirs as ATC-managed and putting them at
        // risk of being mirrored over. The marker file gates that decision.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("claude").unwrap();
        make_parent(base, entry);

        let target = base.join(entry.target_dir);
        std::fs::create_dir_all(&target).unwrap();
        // User happens to have a file named like an embedded skill.
        std::fs::write(target.join("dispatch.md"), "user notes").unwrap();

        assert_eq!(agent_status(base, entry), AgentStatus::UserDir);
    }

    #[test]
    fn copy_mode_writes_marker_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        make_skills_dir(base);
        let entry = find_agent("agents").unwrap();
        make_parent(base, entry);

        run_init_agent(
            base,
            "agents",
            AgentOpts {
                force: false,
                copy: true,
            },
        )
        .unwrap();

        let marker = base.join(entry.target_dir).join(COPY_MARKER_FILENAME);
        assert!(marker.is_file(), "copy mode should write the marker file");
        assert_eq!(agent_status(base, entry), AgentStatus::Copied);
    }

    #[test]
    fn copy_skills_propagates_read_failure() {
        // If the source directory cannot be read (e.g. it does not exist),
        // copy_skills must fail rather than silently fall back to the embedded
        // bundle and drop user-authored files.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let target = base.join("target-skills");
        let nonexistent = base.join("nope");
        let err = copy_skills(&nonexistent, &target).unwrap_err();
        assert!(
            err.to_string().contains("failed to read"),
            "expected 'failed to read' context, got: {err}"
        );
    }

    #[test]
    fn list_agents_runs() {
        let dir = tempfile::tempdir().unwrap();
        list_agents(dir.path()).unwrap();
        list_agents_json(dir.path()).unwrap();
    }
}
