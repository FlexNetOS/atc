use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::terminal_text::{display_text, terminal_safe_json_pretty};
use atc_core::types::RunOpts;
use std::collections::HashMap;
use std::sync::Arc;

pub use args::{Args, Commands};

pub mod cleanup;
pub mod close;
pub mod daemon;
pub mod dispatch;
pub mod enqueue;
pub mod health;
pub mod history;
pub mod info;
pub mod init;
pub mod kb;
pub mod logs;
pub mod open_session;
pub mod output_schema;
pub mod pager;
pub mod pipeline;
pub mod post_complete;
pub mod queue_cmd;
pub mod redirect;
pub mod resolve;
pub mod resolvers;
pub mod retry;
pub mod sessions;
pub mod status;
pub mod stop;
pub mod style;
pub mod subprocess;
pub mod tmux;
pub mod watch;

pub(crate) mod shell_text;

#[cfg(test)]
pub(crate) mod test_support;

mod args {
    use crate::sessions::SessionGroupBy;
    use atc_core::types::Directive;
    use clap::{Parser, Subcommand};
    use std::path::PathBuf;

    #[derive(Parser)]
    #[command(
        name = "atc",
        about = "Air Traffic Control — agent orchestrator",
        version,
        after_help = "EXAMPLES:\n  atc status                  # Active dispatches (running, retrying, needs-*)\n  atc status --all            # Include done/failed/stopped\n  atc sessions                # Keyboard switchboard for sessions\n  atc tui                     # Alias for atc sessions\n  atc run task tasks/foo      # Dispatch a task\n  atc open-session <id>       # Attach to an ATC tmux session\n  atc info <id>               # Detailed view of one dispatch\n  atc health --auto           # Auto-fix NeedsReview dispatches\n\nGLOBAL FLAGS:\n  --no-pager       Bypass the pager even in TTY mode\n  --color <mode>   auto|always|never (default: auto)\n\nENV:\n  ATC_PAGER        Pager command (set to 'cat' to disable)\n  ATC_NO_PAGER     Bypass pager when set\n  NO_COLOR         Disable color when set (any value)\n  ATC_CI           Disable pager + force inline when set to 1/true/yes\n"
    )]
    pub struct Args {
        #[arg(long, global = true)]
        pub config: Option<std::path::PathBuf>,

        /// Bypass the pager even when stdout is a TTY.
        #[arg(long = "no-pager", global = true)]
        pub no_pager: bool,

        /// Color mode: auto (default), always, never.
        #[arg(long = "color", global = true, default_value = "auto")]
        pub color: String,

        #[command(subcommand)]
        pub command: Commands,
    }

    #[derive(Subcommand)]
    pub enum Commands {
        /// Run an agent (direct dispatch, no queue)
        #[command(
            after_help = "EXAMPLES:\n  atc run task tasks/gitkb-42                  # Implement a task\n  atc run review-fix --param pr=<pr-url>       # Address PR review\n  atc run pr-comments --param pr=<pr-url>      # Resolve PR comments\n  atc run task tasks/foo --dry-run             # Preview without launching\n  atc run my-template --inline --no-worktree   # Run a template in cwd\n  atc run --resume tasks/foo 'follow up'       # Continue the latest task session\n  atc run task tasks/foo --json | jq           # Stable v1 envelope on stdout\n\nJSON OUTPUT (--json):\n  Emits a stable v1 envelope on stdout instead of the human-readable\n  confirmation. Errors also emit a structured envelope on stdout and exit\n  non-zero. Schema:\n\n    {\n      \"schema_version\": 1,\n      \"kind\": \"dispatch\" | \"error\" | \"templates\",\n      \"data\": {\n        // dispatch (success):\n        \"dispatch_id\": \"<id>\",\n        \"task_slug\": \"tasks/...\" | null,\n        \"branch\": \"...\",\n        \"session\": \"...\",\n        \"directive\": \"implement\" | ...,\n        \"worktree_path\": \"/path/...\",\n        \"status\": \"running\" | \"done\" | \"failed\" | \"preview\",\n        \"resolver\": \"task\" | \"template\" | \"prompt\",\n        \"pr_urls\": [...],\n        \"log_file\": \"/path/...\" | null,\n        \"agent_provider\": \"claude\",\n        \"agent_session_id\": \"<uuid>\" | null,\n        \"agent_transcript_cwd\": \"/path/...\" | null,\n        \"resume_of_dispatch_id\": \"<id>\" | null,\n        \"agent_capabilities\": { ... } | null,\n        \"is_dry_run\": false,\n        \"inline_exit_code\": null | <i32>,\n        \"dispatched_at\": \"<rfc3339>\"\n        // error:\n        // \"code\": \"<category>\", \"message\": \"<msg>\", \"task_slug\": \"...\"?\n        // templates (--list):\n        // \"templates\": [\"name\", ...]\n      }\n    }\n\n  Future fields are additive; consumers should ignore unknown keys.\n\nRESUME / RETRY / REDIRECT:\n  `atc run --resume <dispatch-id|task-slug> ...` creates a new ATC dispatch that\n  continues the provider-native Claude conversation from the source record.\n  `atc retry` starts a fresh provider conversation for a failed dispatch.\n  `atc redirect` sends text to a currently running tmux-backed ATC session.\n\nNOTE: pass PR URLs via --param pr=<url> or --pr-url <url>; never as a\npositional argument (it falls through to the prompt resolver).\n"
        )]
        Run {
            /// Input: "task <slug>", template name, or raw prompt string
            input: Vec<String>,
            /// Directive (implement, research, kb-update, review-fix, pr-comments, refine, create-task, close)
            #[arg(long, value_parser = clap::value_parser!(Directive))]
            directive: Option<Directive>,
            /// Key=value pairs for template rendering
            #[arg(long = "param")]
            param: Vec<String>,
            /// PR URL (required for review-fix and pr-comments modes)
            #[arg(long)]
            pr_url: Option<String>,
            /// Target repo path(s) within meta workspace (e.g., open-source/atc). Repeatable.
            #[arg(long = "repo", action = clap::ArgAction::Append)]
            repos: Vec<String>,
            /// Run inline (synchronous, no tmux). Auto-enabled when ATC_CI=true.
            #[arg(long)]
            inline: bool,
            /// Force dispatch even if worktree is in use or session exists
            #[arg(long)]
            force: bool,
            /// Preview full dispatch config without launching
            #[arg(long)]
            dry_run: bool,
            /// List available templates
            #[arg(long)]
            list: bool,
            /// Comma-separated directive override
            #[arg(long)]
            directives: Option<String>,
            /// Skip worktree creation (run in current directory)
            #[arg(long)]
            no_worktree: bool,
            /// Override max budget (USD) for this dispatch
            #[arg(long)]
            max_budget_usd: Option<f64>,
            /// Override max turns for this dispatch
            #[arg(long)]
            max_turns: Option<u32>,
            /// Resume provider conversation from dispatch ID or latest task slug
            #[arg(long)]
            resume: Option<String>,
            /// Ephemeral mode: skip registry, logs, system prompt, providers (requires --inline)
            #[arg(long)]
            ephemeral: bool,
            /// Timeout in seconds for inline execution (kill after N seconds)
            #[arg(long)]
            timeout: Option<u32>,
            /// Emit a stable v1 JSON envelope on stdout (success and error). Suppresses
            /// human-readable confirmation. Errors also emit on stdout, exit non-zero.
            #[arg(long)]
            json: bool,
        },
        /// Check health of all active dispatches
        #[command(
            after_help = "EXAMPLES:\n  atc health                  # Active records\n  atc health --all            # Include done/failed\n  atc health --auto           # Auto-fix NeedsReview dispatches\n  atc health --json | jq      # Stable v1 schema for agents\n"
        )]
        Health {
            /// Output as JSON array
            #[arg(long)]
            json: bool,
            /// Include done and failed records
            #[arg(long)]
            all: bool,
            /// Auto-dispatch review-fix for NeedsReview records with PR URLs
            #[arg(long)]
            auto: bool,
        },
        /// Render and print the system prompt for a directive (useful for debugging)
        #[command(
            after_help = "EXAMPLES:\n  atc prompt implement                                # Default slug\n  atc prompt review-fix --slug tasks/gitkb-42         # Real slug\n  atc prompt implement --directive-text 'extra...'    # Append directive text\n  atc prompt implement --worktree-path /path/to/wt    # Use a project's partials\n"
        )]
        Prompt {
            /// Directive to render
            #[arg(value_parser = clap::value_parser!(Directive))]
            directive: Directive,
            /// Task slug for {{slug}} interpolation (default: "tasks/example")
            #[arg(long, default_value = "tasks/example")]
            slug: String,
            /// Additional directive text passed into prompt rendering
            #[arg(long = "directive-text")]
            directive_text: Option<String>,
            /// Worktree path for resolving project-level partials
            #[arg(long)]
            worktree_path: Option<PathBuf>,
        },
        /// Mark a task as complete, remove worktree, update git-kb
        #[command(
            after_help = "EXAMPLES:\n  atc close tasks/gitkb-42                                # Close without PR\n  atc close tasks/gitkb-42 --pr https://github.com/o/r/pull/1   # Record PR\n"
        )]
        Close {
            /// Task slug (e.g. tasks/gitkb-42)
            slug: String,
            /// PR URL to record
            #[arg(long)]
            pr: Option<String>,
        },
        /// Send a message to a running agent's tmux session
        #[command(
            after_help = "EXAMPLES:\n  atc redirect tasks/gitkb-42 'please rerun the tests'\n  atc redirect <dispatch-id> 'context: focus on auth'\n  atc redirect tasks/foo 'stop and write a recap'\n"
        )]
        Redirect {
            /// Dispatch ID or task slug
            id: String,
            /// Message to send to the agent
            message: String,
        },
        /// Re-dispatch a failed task with the same directive and config
        #[command(
            after_help = "EXAMPLES:\n  atc retry tasks/gitkb-42                # Retry by task slug\n  atc retry <dispatch-id>                 # Retry by ID\n  # See `atc info <id>` for the original config that will be reused.\n"
        )]
        Retry {
            /// Dispatch ID or task slug
            id: String,
        },
        /// Show table view of all dispatch records
        #[command(
            name = "status",
            after_help = "EXAMPLES:\n  atc status                  # Active work (running, retrying, needs-*)\n  atc status --all            # Include done/failed/stopped\n  atc status --since 24h      # Add anything updated in the last 24h\n  atc status --status failed  # Only failed records\n  atc status --reverse        # Newest at top (git log style)\n  atc status --json | jq      # Stable v1 schema for agents\n"
        )]
        StatusCmd {
            /// Filter by status (running, done, failed, needs-review, needs-human, stopped, retrying)
            #[arg(long = "status")]
            status_filter: Option<String>,
            /// Output as JSON array
            #[arg(long)]
            json: bool,
            /// Show flat per-dispatch table instead of work-unit-grouped view
            #[arg(long)]
            flat: bool,
            /// Include all statuses (overrides default "interesting" filter)
            #[arg(long)]
            all: bool,
            /// Include done/failed in addition to the default interesting set
            #[arg(long = "include-done")]
            include_done: bool,
            /// Also include records updated within the given duration (e.g. 24h, 2d, 1w)
            #[arg(long)]
            since: Option<String>,
            /// Render newest first (default: newest at the bottom of the buffer)
            #[arg(long)]
            reverse: bool,
        },
        /// Browse and switch between ATC agent sessions
        #[command(
            name = "sessions",
            visible_alias = "tui",
            after_help = "EXAMPLES:\n  atc sessions                         # Interactive session switchboard\n  atc tui                              # Alias for atc sessions\n  atc sessions --task tasks/foo        # Filter by task\n  atc sessions --provider claude       # Filter by provider\n  atc sessions --status running        # Filter by status\n  atc sessions --once                  # Render once and exit\n  atc sessions --json | jq             # Stable v1 schema for agents\n  atc tui --json                       # Alias emits the same schema\n"
        )]
        Sessions {
            /// Filter by task slug
            #[arg(long)]
            task: Option<String>,
            /// Filter by work unit id
            #[arg(long = "work-unit")]
            work_unit: Option<String>,
            /// Filter by branch
            #[arg(long)]
            branch: Option<String>,
            /// Filter by agent provider
            #[arg(long)]
            provider: Option<String>,
            /// Filter by status (running, done, failed, needs-review, needs-human, stopped, retrying)
            #[arg(long = "status")]
            status_filter: Option<String>,
            /// Text search across visible session fields
            #[arg(long)]
            search: Option<String>,
            /// Group rows by task, work-unit, branch, provider, status, or none
            #[arg(long, value_enum, default_value = "none")]
            group: SessionGroupBy,
            /// Include all statuses beyond the default active + recent terminal set
            #[arg(long)]
            all: bool,
            /// Interactive refresh interval, e.g. 1s, 2s, 500ms (minimum 250ms)
            #[arg(long = "poll-interval")]
            poll_interval: Option<String>,
            /// Render once and exit
            #[arg(long)]
            once: bool,
            /// Emit stable v1 JSON and exit
            #[arg(long)]
            json: bool,
        },
        /// Attach to an ATC terminal session by URI, dispatch ID, or unambiguous task slug
        #[command(
            name = "open-session",
            after_help = "EXAMPLES:\n  atc open-session <dispatch-id>\n  atc open-session atc://session/<dispatch-id>\n  atc open-session tasks/foo --json\n\nJSON OUTPUT (--json):\n  Resolves and previews the open-session action without attaching.\n"
        )]
        OpenSession {
            /// ATC session URI, dispatch ID, or task slug with exactly one active dispatch
            target: String,
            /// Emit a stable v1 JSON preview and do not attach
            #[arg(long)]
            json: bool,
        },
        /// Show dispatch history for a work unit (by task, PR, or branch)
        #[command(
            after_help = "EXAMPLES:\n  atc history tasks/harmony-370                              # By task slug\n  atc history --pr https://github.com/o/r/pull/123           # By PR URL\n  atc history --branch tasks--harmony-370                    # By branch\n  atc history tasks/harmony-370 --json | jq                  # Stable v1 schema\n"
        )]
        History {
            /// Task slug (e.g. tasks/harmony-370)
            slug: Option<String>,
            /// PR URL to resolve to a work unit
            #[arg(long)]
            pr: Option<String>,
            /// Branch name to resolve to a work unit
            #[arg(long)]
            branch: Option<String>,
            /// Output as JSON
            #[arg(long)]
            json: bool,
        },
        /// Show detailed info for a single dispatch record
        #[command(
            after_help = "EXAMPLES:\n  atc info <dispatch-id>      # By full ID\n  atc info tasks/foo          # By task slug\n  atc info <id> --json        # Stable v1 schema for agents\n"
        )]
        Info {
            /// Dispatch ID or task slug
            id: String,
            /// Output as JSON envelope
            #[arg(long)]
            json: bool,
        },
        /// Tail the stream-json log for a dispatch
        #[command(
            after_help = "EXAMPLES:\n  atc logs tasks/gitkb-42            # By task slug\n  atc logs <dispatch-id>             # By ID\n  atc logs <id> -f                   # Follow (tail -f)\n  atc logs <session-name>            # By tmux session name\n"
        )]
        Logs {
            /// Dispatch ID, task slug, or session name
            arg: String,
            /// Follow log file (like tail -f)
            #[arg(short = 'f', long)]
            follow: bool,
        },
        /// Stop a running dispatch (kill session, mark stopped)
        #[command(
            after_help = "EXAMPLES:\n  atc stop tasks/gitkb-42        # Stop by task slug\n  atc stop <dispatch-id>         # Stop by ID\n  # Use `atc cleanup <id>` afterward to remove the worktree.\n"
        )]
        Stop {
            /// Dispatch ID or task slug
            id: String,
        },
        /// Clean up a dispatch (remove worktree, kill session)
        #[command(
            after_help = "EXAMPLES:\n  atc cleanup tasks/gitkb-42       # By task slug\n  atc cleanup <dispatch-id>        # By ID\n  atc cleanup --done               # Bulk-clean all Done dispatches\n"
        )]
        Cleanup {
            /// Dispatch ID or task slug (omit for --done mode)
            id: Option<String>,
            /// Clean up all Done dispatches
            #[arg(long)]
            done: bool,
        },
        /// Run post-completion pipeline (extract artifacts, update registry, notify)
        #[command(
            after_help = "EXAMPLES:\n  atc post-complete                                       # Most recent Running\n  atc post-complete --id <dispatch-id>                    # Specific record\n  atc post-complete --id <id> --exit-code 0               # With explicit exit code\n  atc post-complete --id <id> --log /path/to/stream.jsonl # Override log path\n"
        )]
        PostComplete {
            /// Dispatch ID (default: most recent Running)
            #[arg(long)]
            id: Option<String>,
            /// Exit code from the agent process (inferred from log if not provided)
            #[arg(long)]
            exit_code: Option<i32>,
            /// Path to stream-json log file (resolved from registry if not provided)
            #[arg(long)]
            log: Option<std::path::PathBuf>,
        },
        /// Initialize .atc/ directory, then optionally wire skills into a coding agent
        ///
        /// Examples:
        ///   atc init                  # scaffold .atc/, then prompt to wire agents (TTY)
        ///   atc init --no-interactive # scaffold only, skip picker (CI / scripts)
        ///   atc init --force          # overwrite .atc/, then prompt
        ///   atc init claude           # wire .atc/skills into .claude/skills/atc
        ///   atc init claude --copy    # copy files instead of symlinking
        ///   atc init --all-agents     # wire every detected agent
        ///   atc init --list-agents    # show registry + current wire-up status
        ///   atc init --interactive    # picker only (skip .atc/ scaffold)
        Init {
            /// Agent name (e.g. "claude", "agents"). When set, wires .atc/skills
            /// into that agent's skills dir without re-scaffolding .atc/.
            agent: Option<String>,
            /// Force overwrite existing .atc/ files (scaffold mode), or replace a
            /// wrong-target symlink (agent mode). Never deletes a real user dir.
            #[arg(long)]
            force: bool,
            /// Copy files instead of symlinking (agent mode). Re-runs mirror
            /// .atc/skills/ but never delete user-added files in the target dir.
            #[arg(long)]
            copy: bool,
            /// Print the agent registry with current wire-up status, then exit.
            #[arg(long)]
            list_agents: bool,
            /// JSON output for --list-agents.
            #[arg(long)]
            json: bool,
            /// Wire every registry entry whose parent dir exists in the project.
            #[arg(long)]
            all_agents: bool,
            /// Skip the interactive picker even on a TTY.
            #[arg(long)]
            no_interactive: bool,
            /// Open the picker without re-scaffolding .atc/ (post-init re-wire).
            #[arg(long)]
            interactive: bool,
        },
        /// Watch running agent sessions and emit structured events
        #[command(
            after_help = "EXAMPLES:\n  atc watch                          # Most recent Running dispatch\n  atc watch --id <dispatch-id>       # Specific dispatch\n  atc watch --all-running            # All running dispatches\n  atc watch --pretty                 # Human-formatted output\n  atc watch --format json            # JSON event stream (default)\n  atc watch --socket \"$XDG_RUNTIME_DIR/atc/watch.sock\" --id <dispatch-id>\n\nSOCKETS:\n  --socket requires a non-existing path inside a directory owned by the current\n  user with no group/other permissions (mode 0700/0600-style parent). Shared\n  directories such as /tmp or normal 0755 project directories are refused.\n"
        )]
        Watch {
            /// Dispatch ID to watch (default: most recent Running)
            #[arg(long)]
            id: Option<String>,
            /// Watch all running dispatches
            #[arg(long)]
            all_running: bool,
            /// Output format: json, pretty, human, or auto (default: auto → json)
            #[arg(long, default_value = "auto")]
            format: String,
            /// Shorthand for --format pretty
            #[arg(long)]
            pretty: bool,
            /// Unix socket path for multi-consumer mode; parent must be private to the current user
            #[arg(long)]
            socket: Option<std::path::PathBuf>,
        },
        /// Add work to the dispatch queue
        #[command(
            after_help = "EXAMPLES:\n  atc enqueue task tasks/foo                       # Single task\n  atc enqueue --ready --limit 3                    # Top-3 ready tasks\n  atc enqueue --board --status active --unblocked  # Filter via board\n  atc enqueue --view 'my-saved-view'               # From a saved view\n  echo 'tasks/a\\ntasks/b' | atc enqueue --stdin   # From stdin\n"
        )]
        Enqueue {
            /// Input: "task <slug>", template name, or raw prompt
            input: Vec<String>,
            /// Target named queue
            #[arg(long, default_value = "default")]
            queue: String,
            /// Dispatch priority (critical, high, medium, low)
            #[arg(long, default_value = "medium")]
            priority: String,
            /// Override directive/mode for dispatched items
            #[arg(long)]
            mode: Option<String>,
            /// Key=value pairs for template rendering
            #[arg(long = "param")]
            param: Vec<String>,
            /// Delegate selection to kb_ready scoring
            #[arg(long)]
            ready: bool,
            /// Limit for --ready (how many top-scored tasks to enqueue)
            #[arg(long, default_value = "1")]
            limit: u32,
            /// Delegate selection to board query
            #[arg(long)]
            board: bool,
            /// Board filter: status
            #[arg(long = "status")]
            status_filter: Option<String>,
            /// Board filter: only unblocked tasks
            #[arg(long)]
            unblocked: bool,
            /// Board filter: only unassigned tasks
            #[arg(long)]
            unassigned: bool,
            /// Delegate selection to a saved view
            #[arg(long)]
            view: Option<String>,
            /// Read slugs from stdin (one per line)
            #[arg(long)]
            stdin: bool,
        },
        /// View and manage dispatch queues
        #[command(
            after_help = "EXAMPLES:\n  atc queue                          # List pending items in default queue\n  atc queue --name my-queue          # List a named queue\n  atc queue drain                    # Dispatch all pending items\n  atc queue clear                    # Remove all pending items\n"
        )]
        Queue {
            #[command(subcommand)]
            action: Option<QueueAction>,
            /// Queue name to inspect
            #[arg(long, default_value = "default")]
            name: String,
        },
        /// Lightweight AI dispatch — prompt in, text out. No worktree, registry, or system prompt.
        /// Equivalent to: atc run <template> --inline --no-worktree --ephemeral --timeout <N>
        #[command(
            after_help = "EXAMPLES:\n  atc quick commit-message --param diff='...'      # Run a template\n  atc quick --list                                 # List templates\n  atc quick foo --param k=v --timeout 30           # Override timeout\n  atc quick foo --max-budget-usd 0.10              # Tighten budget\n  atc quick foo --dry-run                          # Preview\n"
        )]
        Quick {
            /// Template name (e.g., "commit-message")
            template: Option<String>,
            /// Template parameters
            #[arg(long = "param", action = clap::ArgAction::Append)]
            param: Vec<String>,
            /// Timeout in seconds (default 15)
            #[arg(long, default_value = "15")]
            timeout: u32,
            /// Max budget in USD (default 0.50)
            #[arg(long, default_value = "0.50")]
            max_budget_usd: f64,
            /// List available templates
            #[arg(long)]
            list: bool,
            /// Preview without dispatching
            #[arg(long)]
            dry_run: bool,
        },
        /// Run the continuous dispatch daemon
        #[command(
            after_help = "EXAMPLES:\n  atc daemon                                # Drain default queue\n  atc daemon --queue main --queue retry     # Drain multiple queues\n  atc daemon --max-concurrent 2             # Limit concurrency\n  atc daemon --source github-issues         # Activate a source\n  atc daemon status                         # Daemon health check\n  atc daemon stop                           # Graceful shutdown\n"
        )]
        Daemon {
            #[command(subcommand)]
            action: Option<DaemonAction>,
            /// Queue(s) to drain
            #[arg(long = "queue")]
            queues: Vec<String>,
            /// Max concurrent dispatches
            #[arg(long)]
            max_concurrent: Option<usize>,
            /// Activate source(s) alongside the drain loop
            #[arg(long = "source")]
            sources: Vec<String>,
        },
    }

    #[derive(Subcommand)]
    pub enum QueueAction {
        /// Dispatch all pending items in one shot, then exit
        Drain,
        /// Remove all pending items from the queue
        Clear,
    }

    #[derive(Subcommand)]
    pub enum DaemonAction {
        /// Gracefully stop the running daemon
        Stop,
        /// Show daemon status (uptime, queue depth, active dispatches)
        Status,
    }
}

/// Parse `--param key=value` pairs into a HashMap.
fn parse_params(param_args: &[String]) -> Result<HashMap<String, String>> {
    let mut params = HashMap::new();
    for p in param_args {
        let (k, v) = p.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --param format: '{}' (expected key=value)",
                display_text(p)
            )
        })?;
        params.insert(k.to_string(), v.to_string());
    }
    Ok(params)
}

/// Handle `atc run` against an abstract registry/executor. Shared by the local
/// entry point ([`run`]) and the cloud entry point ([`run_cloud`]) so the
/// dispatch logic stays identical across backends.
async fn handle_run(
    command: &Commands,
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
) -> Result<()> {
    let Commands::Run {
        input,
        directive,
        param,
        pr_url,
        repos,
        inline,
        force,
        dry_run,
        list,
        directives,
        no_worktree,
        max_budget_usd,
        max_turns,
        resume,
        ephemeral,
        timeout,
        json,
    } = command
    else {
        unreachable!("handle_run called with a non-Run command");
    };
    let json_mode = *json;
    // In --json mode, every failure path (including pre-pipeline argument
    // validation) must surface as a structured envelope on stdout instead
    // of the default anyhow stderr trace. Wrap the whole handler in a
    // closure so we can intercept errors uniformly.
    let result: Result<()> = (async {
        if *list {
            let templates = resolvers::template::TemplateResolver::list_templates(config);
            if json_mode {
                let payload = serde_json::json!({
                    "schema_version": output_schema::SCHEMA_VERSION,
                    "kind": "templates",
                    "data": { "templates": templates },
                });
                println!("{}", terminal_safe_json_pretty(&payload)?);
            } else if templates.is_empty() {
                println!("No templates found.");
            } else {
                println!("Available templates:");
                for name in &templates {
                    println!("  {}", display_text(name));
                }
            }
            return Ok(());
        }

        if input.is_empty() || input.iter().all(|s| s.trim().is_empty()) {
            anyhow::bail!(
                "input is required: provide a task slug, template name, or prompt string\n\
                 hint: try `atc run --list` to see templates, or `atc run task <slug>` for a task."
            );
        }

        let (raw_input, force_task) = if input.first().map(|s| s.as_str()) == Some("task") {
            let slug = input[1..].join(" ");
            if slug.is_empty() {
                anyhow::bail!(
                    "'atc run task' requires a task slug, e.g. 'atc run task tasks/gitkb-42'\n\
                     hint: list slugs with `git kb list --type task` or check `atc status`."
                );
            }
            (slug, true)
        } else {
            (input.join(" "), false)
        };

        let is_inline = *inline
            || std::env::var("ATC_CI")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);

        let params = parse_params(param)?;

        let opts = RunOpts {
            input: raw_input.clone(),
            directive: directive.clone(),
            params,
            pr_url: pr_url.clone(),
            repos: repos.clone(),
            inline: is_inline,
            force: *force,
            dry_run: *dry_run,
            directives: directives.clone(),
            no_worktree: *no_worktree,
            max_budget_usd: *max_budget_usd,
            max_turns: *max_turns,
            resume: resume.clone(),
            retries: 0,
            list: false,
            ephemeral: *ephemeral,
            timeout: *timeout,
            json: json_mode,
        };

        let all_resolvers = resolvers::build_resolvers(config);
        let resolvers_to_use = if force_task {
            all_resolvers
                .into_iter()
                .filter(|r| r.name() == "task")
                .collect()
        } else {
            all_resolvers
        };

        let pipeline = pipeline::DispatchPipeline {
            resolvers: resolvers_to_use,
            config,
            registry,
            executor,
        };

        let outcome = pipeline.execute(&raw_input, &opts).await?;
        if let Some(code) = outcome.inline_exit_code {
            if code != 0 {
                if json_mode {
                    std::process::exit(1);
                }
                anyhow::bail!("inline dispatch failed with exit code {code}");
            }
        }
        Ok(())
    })
    .await;

    if json_mode {
        if let Err(e) = result {
            pipeline::emit_run_error_envelope(&e);
            std::process::exit(1);
        }
        Ok(())
    } else {
        result
    }
}

/// Library entry point for command execution.
pub async fn run(
    args: &Args,
    config: &AtcConfig,
    registry: Arc<atc_core::registry::SqliteRegistry>,
    executor: Arc<dyn AgentExecutor>,
) -> Result<()> {
    match &args.command {
        Commands::Run { .. } => {
            handle_run(&args.command, config, registry.as_ref(), executor.as_ref()).await
        }
        Commands::Health { json, all, auto } => {
            health::run_health(
                config,
                registry.clone() as Arc<dyn Registry>,
                executor,
                *json,
                *all,
                *auto,
            )
            .await
        }
        Commands::Prompt {
            directive,
            slug,
            directive_text,
            worktree_path,
        } => {
            let prompt = atc_core::prompt_engine::render_prompt(
                directive,
                slug,
                config,
                directive_text.as_deref().unwrap_or(""),
                worktree_path.as_deref(),
            )
            .await?;
            println!("{prompt}");
            Ok(())
        }
        Commands::Close { slug, pr } => {
            close::run_close(config, registry.as_ref(), slug, pr.as_deref()).await
        }
        Commands::Redirect { id, message } => {
            redirect::run_redirect(registry.as_ref(), id, message).await
        }
        Commands::Retry { id } => {
            retry::run_retry(config, registry.as_ref(), executor.as_ref(), id).await
        }
        Commands::History {
            slug,
            pr,
            branch,
            json,
        } => {
            let pager_cfg = if args.no_pager || *json {
                None
            } else {
                Some(&config.pager)
            };
            history::run_history(
                registry.clone() as Arc<dyn Registry>,
                pager_cfg,
                slug.as_deref(),
                pr.as_deref(),
                branch.as_deref(),
                *json,
            )
            .await
        }
        Commands::StatusCmd {
            status_filter,
            json,
            flat,
            all,
            include_done,
            since,
            reverse,
        } => {
            let opts = status::StatusOpts {
                status_filter: status_filter.clone(),
                json: *json,
                flat: *flat,
                all: *all,
                include_done: *include_done,
                since: since.clone(),
                reverse: *reverse,
                no_pager: args.no_pager || *json,
            };
            status::run_status(
                registry.clone() as Arc<dyn Registry>,
                Some(&config.pager),
                opts,
            )
            .await
        }
        Commands::Sessions {
            task,
            work_unit,
            branch,
            provider,
            status_filter,
            search,
            group,
            all,
            poll_interval,
            once,
            json,
        } => {
            let opts = sessions::SessionsOpts {
                task: task.clone(),
                work_unit: work_unit.clone(),
                branch: branch.clone(),
                provider: provider.clone(),
                status: status_filter.clone(),
                search: search.clone(),
                group: *group,
                all: *all,
                poll_interval: poll_interval.clone(),
                once: *once,
                json: *json,
            };
            sessions::run_sessions(
                config,
                registry.clone() as Arc<dyn Registry>,
                executor,
                opts,
            )
            .await
        }
        Commands::OpenSession { target, json } => {
            open_session::run_open_session(registry.as_ref(), target, *json).await
        }
        Commands::Info { id, json } => {
            info::run_info(registry.clone() as Arc<dyn Registry>, id, *json).await
        }
        Commands::Logs { arg, follow } => {
            logs::run_logs(registry.clone() as Arc<dyn Registry>, config, arg, *follow).await
        }
        Commands::Stop { id } => stop::run_stop(config, registry.as_ref(), id).await,
        Commands::Cleanup { id, done } => {
            cleanup::run_cleanup(config, registry.as_ref(), id.as_deref(), *done).await
        }
        Commands::PostComplete { id, exit_code, log } => {
            post_complete::run_post_complete(
                config,
                registry.as_ref(),
                id.as_deref(),
                *exit_code,
                log.clone(),
            )
            .await
        }
        Commands::Init {
            agent,
            force,
            copy,
            list_agents,
            json,
            all_agents,
            no_interactive,
            interactive,
        } => {
            let opts = init::InitOpts {
                agent: agent.clone(),
                force: *force,
                copy: *copy,
                list_agents: *list_agents,
                list_agents_json: *json,
                all_agents: *all_agents,
                no_interactive: *no_interactive,
                interactive: *interactive,
            };
            init::run(config, opts).await
        }
        Commands::Watch {
            id,
            all_running,
            format,
            pretty,
            socket,
        } => {
            let effective_format = if *pretty { "pretty" } else { format.as_str() };
            watch::run_watch(
                config,
                registry.clone() as Arc<dyn Registry>,
                id.as_deref(),
                *all_running,
                effective_format,
                socket.clone(),
            )
            .await
        }
        Commands::Enqueue {
            input,
            queue,
            priority,
            mode,
            param,
            ready,
            limit,
            board,
            status_filter,
            unblocked,
            unassigned,
            view,
            stdin,
        } => {
            let priority: atc_core::queue::Priority = priority.parse()?;
            let params = parse_params(param)?;
            let opts = enqueue::EnqueueOpts {
                input: input.clone(),
                queue: queue.clone(),
                priority,
                mode: mode.clone(),
                params,
                ready: *ready,
                limit: *limit,
                board: *board,
                status_filter: status_filter.clone(),
                unblocked: *unblocked,
                unassigned: *unassigned,
                view: view.clone(),
                stdin: *stdin,
                enqueued_by: "user".to_string(),
                workspace_root: config.config_dir.clone(),
            };
            enqueue::run_enqueue(registry.as_ref(), &opts).await
        }
        Commands::Queue { action, name } => match action {
            Some(args::QueueAction::Drain) => {
                queue_cmd::run_queue_drain(
                    registry.as_ref(),
                    registry.as_ref(),
                    executor.as_ref(),
                    config,
                    name,
                )
                .await
            }
            Some(args::QueueAction::Clear) => {
                queue_cmd::run_queue_clear(registry.as_ref(), name).await
            }
            None => queue_cmd::run_queue_list(registry.as_ref(), name).await,
        },
        Commands::Quick {
            template,
            param,
            timeout,
            max_budget_usd,
            list,
            dry_run,
        } => {
            // Handle --list
            if *list {
                let templates = resolvers::template::TemplateResolver::list_templates(config);
                if templates.is_empty() {
                    println!("No templates found.");
                } else {
                    println!("Available templates:");
                    for name in &templates {
                        println!("  {}", display_text(name));
                    }
                }
                return Ok(());
            }

            let template = template.as_deref().ok_or_else(|| {
                anyhow::anyhow!("template name required (use --list to see available templates)")
            })?;

            let params = parse_params(param)?;
            let opts = RunOpts {
                input: template.to_string(),
                directive: None,
                params,
                pr_url: None,
                repos: vec![],
                inline: true,
                force: false,
                dry_run: *dry_run,
                directives: None,
                no_worktree: true,
                max_budget_usd: Some(*max_budget_usd),
                max_turns: None,
                resume: None,
                retries: 0,
                list: false,
                ephemeral: true,
                timeout: Some(*timeout),
                json: false,
            };

            // Quick is template-only — don't allow fallthrough to prompt/task resolvers.
            let template_resolver: Vec<Box<dyn atc_core::resolver::InputResolver>> =
                vec![Box::new(resolvers::template::TemplateResolver)];
            let pipeline = pipeline::DispatchPipeline {
                resolvers: template_resolver,
                config,
                registry: registry.as_ref(),
                executor: executor.as_ref(),
            };

            let outcome = pipeline.execute(template, &opts).await?;
            if let Some(code) = outcome.inline_exit_code {
                if code != 0 {
                    anyhow::bail!("quick dispatch failed with exit code {code}");
                }
            }
            Ok(())
        }
        Commands::Daemon {
            action,
            queues,
            max_concurrent,
            sources,
        } => match action {
            Some(args::DaemonAction::Stop) => daemon::stop_daemon(config).await,
            Some(args::DaemonAction::Status) => {
                daemon::daemon_status(config, registry.as_ref(), registry.as_ref(), queues).await
            }
            None => {
                let max_concurrent = max_concurrent.unwrap_or(config.daemon.max_concurrent);
                let opts = daemon::DaemonOpts {
                    queues: queues.clone(),
                    max_concurrent,
                    sources: sources.clone(),
                };
                daemon::run_daemon(registry, executor, config, &opts).await
            }
        },
    }
}

/// Cloud entry point for the Cloud ATC vertical slice ([[tasks/harmony-844]]).
///
/// Selected at `main` when `[cloud] enabled = true`, with a `PgRegistry` and a
/// `RemoteExecutor` injected as trait objects. It supports the spike's hand-run
/// command surface — dispatch (`run`), finalize (`post-complete`), and observe
/// (`status`/`logs`/`info`/`watch`/`health`) — all of which need only
/// `&dyn Registry`. Queue/daemon and other commands require the SQLite backend
/// and are intentionally unsupported here (follow-on work).
/// Error message shown when a command is invoked with `[cloud]` enabled but is
/// not wired to the Postgres/remote backend. Shared by [`cloud_command_supported`]
/// callers (`main`) and [`run_cloud`]'s fallback arm so the two stay in sync.
pub const CLOUD_UNSUPPORTED_COMMAND_MSG: &str =
    "this command is not supported with [cloud] enabled; the Cloud ATC slice \
     supports run, post-complete, status, logs, info, watch, health, stop, and \
     cleanup. Unset cloud.enabled to use the local SQLite backend.";

/// Whether `command` is supported when the `[cloud]` backend is enabled.
///
/// The Cloud ATC slice only wires a subset of commands to the Postgres/remote
/// backend (see [`run_cloud`]). `main` consults this *before* resolving the
/// Postgres URL or connecting, so an unsupported command fails with a clear
/// message instead of an opaque DB/credential error. Keep this list in sync
/// with the explicit arms of [`run_cloud`].
pub fn cloud_command_supported(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Run { .. }
            | Commands::PostComplete { .. }
            | Commands::StatusCmd { .. }
            | Commands::Logs { .. }
            | Commands::Info { .. }
            | Commands::Watch { .. }
            | Commands::Health { .. }
            | Commands::Stop { .. }
            | Commands::Cleanup { .. }
    )
}

pub async fn run_cloud(
    args: &Args,
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
) -> Result<()> {
    match &args.command {
        Commands::Run { .. } => {
            handle_run(&args.command, config, registry.as_ref(), executor.as_ref()).await
        }
        Commands::PostComplete { id, exit_code, log } => {
            post_complete::run_post_complete(
                config,
                registry.as_ref(),
                id.as_deref(),
                *exit_code,
                log.clone(),
            )
            .await
        }
        Commands::StatusCmd {
            status_filter,
            json,
            flat,
            all,
            include_done,
            since,
            reverse,
        } => {
            let opts = status::StatusOpts {
                status_filter: status_filter.clone(),
                json: *json,
                flat: *flat,
                all: *all,
                include_done: *include_done,
                since: since.clone(),
                reverse: *reverse,
                no_pager: args.no_pager || *json,
            };
            status::run_status(registry, Some(&config.pager), opts).await
        }
        Commands::Logs { arg, follow } => logs::run_logs(registry, config, arg, *follow).await,
        Commands::Info { id, json } => info::run_info(registry, id, *json).await,
        Commands::Watch {
            id,
            all_running,
            format,
            pretty,
            socket,
        } => {
            let effective_format = if *pretty { "pretty" } else { format.as_str() };
            watch::run_watch(
                config,
                registry,
                id.as_deref(),
                *all_running,
                effective_format,
                socket.clone(),
            )
            .await
        }
        Commands::Health { json, all, auto } => {
            health::run_health(config, registry, executor, *json, *all, *auto).await
        }
        Commands::Stop { id } => stop::run_stop(config, registry.as_ref(), id).await,
        Commands::Cleanup { id, done } => {
            cleanup::run_cleanup(config, registry.as_ref(), id.as_deref(), *done).await
        }
        _ => anyhow::bail!(CLOUD_UNSUPPORTED_COMMAND_MSG),
    }
}

#[cfg(test)]
mod tests {
    use super::{cloud_command_supported, parse_params, Args, Commands};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn tui_visible_alias_parses_as_sessions_command() {
        let args = Args::try_parse_from(["atc", "tui", "--json", "--once"]).unwrap();
        match args.command {
            Commands::Sessions { json, once, .. } => {
                assert!(json);
                assert!(once);
            }
            _ => panic!("expected sessions command"),
        }
    }

    #[test]
    fn cloud_command_support_gate_matches_run_cloud_arms() {
        // Supported in the cloud slice -> must be gated true so main proceeds.
        let supported = Args::try_parse_from(["atc", "status"]).unwrap();
        assert!(cloud_command_supported(&supported.command));

        // Not wired to the cloud backend -> gated false so main bails before
        // resolving Postgres (the unsupported-command message, not a DB error).
        let unsupported = Args::try_parse_from(["atc", "init"]).unwrap();
        assert!(!cloud_command_supported(&unsupported.command));
    }

    #[test]
    fn parse_params_error_escapes_terminal_controls() {
        let params = vec!["bad\x1b[2J\u{202e}gpj".to_string()];
        let err = parse_params(&params).unwrap_err().to_string();

        assert!(err.contains("bad\\x1b[2J\\u{202e}gpj"), "got: {err}");
        assert!(!err.contains('\x1b'), "got: {err}");
        assert!(!err.contains('\u{202e}'), "got: {err}");
    }

    #[test]
    fn cli_json_stdout_emitters_use_terminal_safe_helpers() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let forbidden = [
            concat!("serde_json::", "to_string_pretty"),
            concat!("println!(\"{}\", ", "serde_json::to_string"),
            concat!("stdout.push(", "serde_json::to_string"),
            concat!("let json = ", "serde_json::to_string(event)"),
        ];
        let mut offenders = Vec::new();
        scan_rust_files(&src_dir, &forbidden, &mut offenders);

        assert!(
            offenders.is_empty(),
            "CLI terminal JSON output must use atc_core::terminal_text::terminal_safe_json* helpers:\n{}",
            offenders.join("\n")
        );
    }

    fn scan_rust_files(dir: &Path, forbidden: &[&str], offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                scan_rust_files(&path, forbidden, offenders);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }

            let source = std::fs::read_to_string(&path).unwrap();
            for pattern in forbidden {
                if source.contains(pattern) {
                    offenders.push(format!("{} contains `{pattern}`", path.display()));
                }
            }
        }
    }
}
