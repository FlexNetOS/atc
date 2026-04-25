//! `atc init` and `atc init <agent>` — scaffold `.atc/` and wire skills into agent dirs.
//!
//! Two top-level entry points:
//!
//! - [`run_init`] — scaffold the `.atc/` project directory with embedded defaults
//!   (config, directives, templates, components, partials, skills). Optionally drops
//!   into the [`picker`] after scaffolding to wire the user's coding agent(s).
//! - [`run_init_agent`] — wire `.atc/skills/` into a specific coding agent's skills
//!   directory via relative symlink (or copy with `--copy`). Idempotent and re-runnable.

pub mod agents;
pub mod picker;
pub mod scaffold;

pub use agents::{
    list_agents, list_agents_json, run_init_agent, AgentEntry, AgentOpts, AgentStatus, WireOutcome,
    AGENT_REGISTRY,
};
pub use picker::run_picker;
pub use scaffold::run_init;

/// Options accepted by `atc init` from the CLI layer.
#[derive(Debug, Clone, Default)]
pub struct InitOpts {
    /// Optional agent name (positional). When set, dispatches to `run_init_agent`.
    pub agent: Option<String>,
    /// Force overwrite (scaffold mode) or replace wrong-target symlink (agent mode).
    pub force: bool,
    /// Copy files instead of symlinking (agent mode only).
    pub copy: bool,
    /// Print the agent registry and exit.
    pub list_agents: bool,
    /// JSON output for `--list-agents`.
    pub list_agents_json: bool,
    /// Wire every entry in the registry whose parent dir exists.
    pub all_agents: bool,
    /// Skip the interactive picker (CI / scripts).
    pub no_interactive: bool,
    /// Open the picker without re-scaffolding `.atc/`.
    pub interactive: bool,
}

/// CLI dispatch entry. Routes to scaffold, agent-wire, list-agents, or picker
/// based on the parsed options. Returns an error for invalid combinations.
pub async fn run(config: &atc_core::config::AtcConfig, opts: InitOpts) -> anyhow::Result<()> {
    if opts.list_agents {
        let base = scaffold::base_dir(config);
        if opts.list_agents_json {
            list_agents_json(base)?;
        } else {
            list_agents(base)?;
        }
        return Ok(());
    }

    if opts.all_agents {
        let base = scaffold::base_dir(config).to_path_buf();
        return agents::run_init_all_agents(
            &base,
            AgentOpts {
                force: opts.force,
                copy: opts.copy,
            },
        );
    }

    if let Some(agent) = opts.agent.as_deref() {
        let base = scaffold::base_dir(config).to_path_buf();
        return run_init_agent(
            &base,
            agent,
            AgentOpts {
                force: opts.force,
                copy: opts.copy,
            },
        )
        .map(|_| ());
    }

    if opts.interactive {
        let base = scaffold::base_dir(config).to_path_buf();
        return run_picker(&base, opts.copy, opts.force);
    }

    // Default: scaffold .atc/, then optionally pick agents.
    run_init(config, opts.force).await?;

    if !opts.no_interactive
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        let base = scaffold::base_dir(config).to_path_buf();
        run_picker(&base, opts.copy, opts.force)?;
    }

    Ok(())
}
