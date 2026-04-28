# ATC (Air Traffic Control)

A headless agent orchestrator for AI coding agents. ATC dispatches agents to work on tasks in isolated git worktrees, monitors their lifecycle through a 6-signal health system, and tracks everything in a SQLite registry.

**Two dispatch modes:**

| | `atc run` | `atc enqueue` + `atc daemon` |
|---|---|---|
| **When** | You want one agent now | You want continuous autonomous dispatch |
| **How** | Direct to pipeline | Queue to daemon to pipeline |
| **Blocking** | Yes (waits for tmux or inline) | No (enqueue returns immediately) |

## Install

```bash
brew tap gitkb/tap
brew install atc
```

Or build from source:

```bash
cargo install --path crates/atc-cli
```

## Quick Start

```bash
# === Direct dispatch (immediate) ===

atc run task tasks/my-task
atc run pr-review --param pr=https://github.com/org/repo/pull/123
atc run 'Fix the auth timeout bug in src/auth.rs'

# === Queue-based dispatch (continuous) ===

# Enqueue specific work
atc enqueue task tasks/my-task
atc enqueue "fix the auth timeout bug"
atc enqueue task tasks/foo --priority critical --queue ci-fixes

# Enqueue using selection strategies
atc enqueue --ready                               # best task by kb_ready scoring
atc enqueue --ready --limit 3                     # top 3 scored tasks
atc enqueue --board --status ready --unblocked    # all ready, unblocked tasks
atc enqueue --view views/sprint-3                 # all results from a saved view
git kb list --type task --status ready --format slugs | atc enqueue --stdin

# One-shot drain
atc queue drain

# Continuous daemon
atc daemon
atc daemon --source ready --source board --max-concurrent 5
atc daemon --queue default --queue ci-fixes

# Check what's happening
atc status
atc health
atc daemon status
```

## How It Works

ATC manages the full lifecycle of an AI agent dispatch:

```text
Direct:     atc run    -> resolve -> worktree -> prompt -> providers -> spawn -> monitor -> post-complete
Queue:      atc enqueue -> dispatch_queue -> atc daemon -> [same pipeline as above]
Continuous: atc daemon --source ready -> kb_ready -> enqueue -> drain -> pipeline
```

### Direct Dispatch (`atc run`)

1. **Input Resolution** — `atc run` accepts three input types, resolved by pluggable `InputResolver` implementations:
   - `task <slug>` — fetch task from GitKB, claim via CAS, pipe document to agent
   - `<template>` — render Handlebars template with `--param` variables
   - `'<prompt>'` — pass raw string directly to agent

2. **Worktree Isolation** — each dispatch gets its own git worktree. Existing worktrees are reused. Collision detection prevents two agents on the same worktree.

3. **Prompt Assembly** — system prompts are assembled from composable component `.md` files per directive config. Templates use [Handlebars](https://handlebarsjs.com/) syntax with 3-level partial resolution.

4. **Context Providers** — pluggable pre-dispatch data assembly. Providers run after prompt assembly, before agent spawn:
   - **PR Context** — fetches PR metadata, comments, reviews, threads in parallel
   - **KB Context** — fetches related documents and active context from GitKB
   - **Rebase** — detects if branch is behind default branch, exports `default_branch` and `rebase_behind_count` as template variables for use in partials

5. **Agent Execution** — spawns `claude` in a tmux session (detached) or inline (synchronous for CI). Stream-json output is logged to JSONL files.

6. **Post-Completion** — extracts artifacts from stream-json logs (cost, PR URLs, commits, summary), updates registry, sends notifications (macOS + webhook), and auto-cleans worktrees on PR merge.

7. **Health Monitoring** — 6-signal state machine evaluates dispatches without consuming tokens.

### Queue-Based Dispatch (`atc enqueue` + `atc daemon`)

The queue is the universal interface between selection (what to dispatch) and scheduling (when to dispatch):

```text
  Selection strategies (what)        Queue (buffer)          Daemon (when + execute)
  ---------------------------        --------------          ----------------------
  atc enqueue task X        ---+
  atc enqueue --ready       ---+
  atc enqueue --board ...   ---+--->  dispatch_queue  --->  atc daemon ---> Pipeline
  atc enqueue --view V      ---+
  daemon source (on loop)   ---+
  POST /queue (ACP)         ---+
```

**Selection strategies:**

| Strategy | One-shot | Continuous (daemon source) |
|---|---|---|
| Explicit (you pick) | `atc enqueue task X` | N/A |
| Board query (filter picks) | `atc enqueue --board --status ready` | `atc daemon --source board` |
| Saved view (view picks) | `atc enqueue --view views/sprint-3` | Source with `view = "slug"` |
| kb_ready (scoring picks) | `atc enqueue --ready --limit 3` | `atc daemon --source ready` |
| kb_events (events pick) | N/A | `atc daemon --source events` |
| Script (script picks) | `./script.sh \| atc enqueue --stdin` | Source with `command = "./script.sh"` |

**Dedup** is built into `enqueue()` — every write path (CLI, source, future ACP) goes through it. Before inserting, it checks: is this input already pending in the queue? Already running in the registry? If so, skip.

**Priority ordering** within a queue: critical (0) > high (25) > medium (50) > low (75). Same priority: oldest first (FIFO).

## Commands

| Command | Description |
|---------|-------------|
| `atc run <input>` | Direct dispatch (task, template, or prompt) |
| `atc enqueue <input>` | Add work to the dispatch queue |
| `atc enqueue --ready` | Enqueue top-scored tasks via kb_ready |
| `atc enqueue --board` | Enqueue from a board query |
| `atc enqueue --view <slug>` | Enqueue from a saved view |
| `atc enqueue --stdin` | Batch enqueue from stdin (pipe-friendly) |
| `atc queue` | Show pending queue items |
| `atc queue drain` | One-shot: dispatch all pending, exit |
| `atc queue clear` | Clear all pending items |
| `atc daemon` | Start continuous dispatch daemon |
| `atc daemon --source <name>` | Daemon with auto-feed sources |
| `atc daemon stop` | Graceful shutdown |
| `atc daemon status` | Uptime, queue depth, active dispatches |
| `atc status` | Table view of all dispatches |
| `atc info <id>` | Detailed view of a single dispatch |
| `atc logs [-f] <id>` | Tail stream-json logs (human-readable) |
| `atc health [--auto]` | Run 6-signal health checks (`--auto` dispatches review-fix for NeedsReview) |
| `atc watch [--format ndjson]` | Stream live events from running agents |
| `atc stop <id>` | Kill tmux session, mark stopped |
| `atc cleanup <id>` | Remove worktree, unassign task |
| `atc retry <id>` | Re-dispatch with adaptive config |
| `atc redirect <id> '<msg>'` | Send message to running agent via tmux |
| `atc close <slug>` | Verify task completion and close |
| `atc post-complete` | Run post-completion (auto or manual recovery) |
| `atc prompt <directive>` | Preview rendered system prompt |
| `atc init` | Initialize `.atc/` project directory, then prompt to wire your coding agent (TTY) |
| `atc init --force` | Re-initialize `.atc/`, overwriting all files |
| `atc init <agent>` | Wire `.atc/skills/` into a coding agent (e.g. `claude`, `agents`) |
| `atc init --list-agents` | Show supported agents and current wire-up status |
| `atc init --all-agents` | Wire every detected agent (skips entries whose parent dir is missing) |

## `.atc/` Project Directory

`atc init` scaffolds a per-project `.atc/` directory with default content embedded in the ATC binary. No external files are copied — the binary is the source of truth for defaults:

```text
.atc/
├── config.toml              # Base config (registry, dispatch, batch, etc.)
├── directives/
│   ├── implement.toml       # Components, budget, providers for implement directive
│   ├── review-fix.toml
│   ├── research.toml
│   └── ...
├── templates/
│   ├── pr-review.md         # Handlebars templates with frontmatter
│   ├── swot.md
│   └── ...
├── components/
│   ├── base.md              # System prompt building blocks
│   ├── code-read.md
│   └── ...
└── skills/
    ├── atc-reference.md     # Shared ATC command reference
    ├── dispatch.md          # Shared dispatch examples and notes
    ├── monitor.md           # Shared monitor examples and notes
    ├── dispatch/
    │   ├── SKILL.md         # Codex skill: dispatch an agent via ATC
    │   └── agents/
    │       └── openai.yaml  # UI metadata for Codex/OpenAI surfaces
    └── monitor/
        ├── SKILL.md         # Codex skill: monitor an ATC dispatch
        └── agents/
            └── openai.yaml
```

## Hooking Into Your Coding Agent

ATC ships agent skills under `.atc/skills/`. To make them visible to a specific
coding agent, run `atc init <agent>` — it creates a directory-level symlink (or a
copy with `--copy`) from the agent's skills directory back to `.atc/skills`.

The shared markdown files stay available as references, while `dispatch/SKILL.md`
and `monitor/SKILL.md` provide the actual Codex skill entrypoints with frontmatter
and product metadata.

```bash
atc init claude        # .claude/skills/atc -> ../../.atc/skills (symlink)
atc init agents        # .agents/skills/atc -> ../../.atc/skills (symlink)
atc init claude --copy # copy files instead (Windows-friendly)
atc init --all-agents  # wire every entry whose parent dir already exists
atc init --list-agents # show the registry + current wire-up status
```

`atc init` (no subcommand) on a TTY scaffolds `.atc/` and then opens a multi-select
picker so you can wire your agents in one shot. Pass `--no-interactive` to skip the
picker (CI / scripts) or `--interactive` to open the picker without re-scaffolding
`.atc/` (post-init re-wire).

| Agent  | Target path          | Purpose |
|--------|----------------------|---------|
| `claude` | `.claude/skills/atc` | Claude Code |
| `agents` | `.agents/skills/atc` | Generic `.agents/` skills convention |

`atc init <agent>` is idempotent — re-running on a correct symlink is a no-op. A
wrong-target symlink errors without `--force`. A real user directory at the
target is never deleted, with or without `--force`. On platforms where symlink
syscalls fail (e.g. Windows without dev-mode), ATC falls back to copy mode and
prints a one-line warning.

**Re-init behavior:**

| Scenario | `atc init` | `atc init --force` |
|---|---|---|
| No `.atc/` exists | Create everything | Create everything |
| `.atc/` exists, no customizations | Skip existing files, add new only | Overwrite all |
| `.atc/` exists, user customized files | Skip existing, add new files only | Overwrite all |
| New ATC version adds new directives/templates | Add new files, don't touch existing | Overwrite all |

## Templates

Templates are Handlebars `.md` files with YAML frontmatter. They're the primary way to create reusable dispatch configurations.

### Template Frontmatter Schema

```yaml
---
description: Review and fix PR — iterative flywheel until confident
directive: review-fix            # which directive to run under (singular)
required_params: [pr]            # params that must be provided before dispatch
---
```

Three fields:
- `description` — human-readable description
- `directive` — the directive name (maps to `.atc/directives/<name>.toml`)
- `required_params` — param validation before rendering

The directive file (`.atc/directives/review-fix.toml`) owns the component list and provider list. Templates do **not** redeclare components — they just specify which directive they run under.

### Built-in Templates

| Template | Directive | Required Params | Description |
|---|---|---|---|
| `pr-review` | `review-fix` | `pr` | Review and fix a PR |
| `pr-comment` | `pr-comments` | `pr` | Resolve PR comments and threads |
| `branch-review` | `review-fix` | — | Review all changes on current branch |
| `close` | `close` | `task` | Verify task completion and close |
| `push-branch` | `implement` | — | Implement and push branch |
| `swot` | `research` | `competitor`, `name` | SWOT analysis of a competitor |

### Usage

```bash
atc run pr-review --param pr=https://github.com/org/repo/pull/123
atc run swot --param competitor=https://example.com --param name="Acme Corp"
atc run close --param task=tasks/my-task
atc run --list    # show available templates
```

## Multi-Repo Dispatch

ATC supports dispatching to multiple repositories in a single command using the `--repo` flag:

```bash
# Dispatch to specific repos within a meta workspace
atc run task tasks/cross-repo-fix --repo open-source/atc --repo open-source/kb
```

Each `--repo` gets its own worktree, agent session, and PR. The dispatch ID links them together.

## Configuration

ATC loads config from (in priority order):
1. `--config <path>` flag
2. `ATC_CONFIG` environment variable
3. `.atc/config.toml` (project directory, created by `atc init`)
4. `./atc.toml` (legacy, still supported)
5. `~/.config/atc/config.toml`

```toml
[registry]
path = "~/.local/share/atc/registry.db"

[dispatch]
worktree_base = "/tmp/worktrees"
claude_bin = "claude"
sandbox = false
max_turns = 10000
max_budget_usd = 25.0
project_env = true  # load .dispatch/env from worktree

[health]
signal_timeout_secs = 30
auto_review = false
cost_warning_threshold = 10.0

[watch]
poll_interval_secs = 5
cost_threshold = 10.0

[notifications]
macos = true
# webhook_url = "https://..."

[prompt]
components_dir = ".atc/components"
templates_dir = ".atc/templates"
partials_dir = ".atc/partials"

# Per-directive configuration
[directives.implement]
components = ["base", "constraints", "kb-read", "kb-write", "code-read", "code-write", "git", "github"]
max_budget_usd = 25.0
providers = ["rebase"]

[directives.review-fix]
components = ["base", "constraints", "code-read", "code-write", "git", "github", "review"]
max_budget_usd = 10.0
providers = ["pr-context", "rebase"]

[directives.research]
components = ["base", "constraints", "kb-read", "code-read"]
max_budget_usd = 7.0

# Resolver chain (first match wins)
[resolvers]
order = ["task", "template", "prompt"]

[resolvers.task]
enabled = true  # set false to disable GitKB integration

# Daemon configuration
[daemon]
drain_interval_secs = 1
max_concurrent = 5
graceful_shutdown_timeout_secs = 300

# Source configurations (activated via atc daemon --source <name>)
[sources.ready]
type = "ready"
poll_interval_secs = 10
limit = 3
queue = "default"

[sources.board]
type = "board"
poll_interval_secs = 10
queue = "default"
filter_status = ["ready"]
require_unassigned = true
require_unblocked = true

[sources.events]
type = "events"
poll_interval_secs = 5
queue = "default"
filter = "document:updated"
path = "tasks/"
trigger_on_status = ["ready"]

[sources.deploy-scripts]
type = "script"
command = "./scripts/find-dispatchable-work.sh"
poll_interval_secs = 60
queue = "default"
```

## Architecture

```text
atc-core/                           atc-cli/
+-- config.rs        Config         +-- pipeline.rs       DispatchPipeline
+-- executor.rs      AgentExecutor  +-- resolvers/
+-- health.rs        HealthChecker  |   +-- task.rs        TaskResolver (GitKB)
+-- post_completion.rs              |   +-- template.rs    TemplateResolver
+-- prompt_engine.rs  Handlebars    |   +-- prompt.rs      PromptResolver
+-- providers/                      +-- enqueue.rs        atc enqueue
|   +-- pr_context.rs               +-- queue_cmd.rs      atc queue (list/drain/clear)
|   +-- kb_context.rs               +-- daemon.rs         atc daemon (drain loop + sources)
|   +-- rebase.rs                   +-- dispatch.rs       Shared dispatch utils
+-- queue.rs         DispatchQueue  +-- resolve.rs        Resolver invocation
+-- source.rs        Source trait   +-- subprocess.rs     Subprocess execution
+-- registry.rs      SQLite         +-- watch.rs          Agent watcher
+-- resolver.rs      InputResolver  +-- status.rs         Status table
+-- stream_json.rs   Log parser     +-- info.rs           Detail view
+-- templates.rs     Prompt render  +-- logs.rs           Log viewer
+-- project_env.rs   .dispatch/env  +-- stop.rs           Stop command
+-- types.rs         Core types     +-- cleanup.rs        Cleanup command
+-- worktree.rs      Cleanup        +-- retry.rs          Adaptive retry
                                    +-- health.rs         Health CLI
                                    +-- redirect.rs       Tmux injection
                                    +-- close.rs          Task closure
                                    +-- post_complete.rs  Post-completion
                                    +-- init.rs           atc init (embedded defaults)
```

### Daemon Architecture

```text
atc daemon (tokio runtime)
|
+-- QueueDrainer (primary)
|   +-- Sleep(drain_interval)          default: 1s
|   +-- health_check_all_running()     6-signal eval per running dispatch
|   +-- peek_queue(available_slots)    SELECT by priority, enqueued_at
|   +-- dispatch_batch()               claim -> pipeline -> update
|
+-- Sources (optional, per --source flag)
|   +-- ReadySource   -> poll kb_ready -> enqueue
|   +-- BoardSource   -> poll kb_list/kb_view -> enqueue
|   +-- EventSource   -> subscribe kb_events -> enqueue
|   +-- ScriptSource  -> run user command -> enqueue
|
+-- SignalHandler
    +-- SIGTERM/SIGINT -> graceful shutdown
```

### Queue Schema

```sql
CREATE TABLE dispatch_queue (
    id          TEXT PRIMARY KEY,
    queue_name  TEXT NOT NULL DEFAULT 'default',
    input_type  TEXT NOT NULL,               -- 'task', 'template', 'prompt'
    input_value TEXT NOT NULL,
    mode        TEXT,
    priority    INTEGER NOT NULL DEFAULT 50, -- 0=critical, 25=high, 50=medium, 75=low
    params      TEXT,                        -- JSON
    status      TEXT NOT NULL DEFAULT 'pending',
    dispatch_id TEXT,                        -- FK to dispatches.id once dispatched
    enqueued_at TEXT NOT NULL,
    enqueued_by TEXT,
    claimed_at  TEXT,
    dispatched_at TEXT,
    error       TEXT
);
```

### InputResolver Trait

ATC's core is decoupled from any specific task system. The `InputResolver` trait is the boundary:

```rust
pub trait InputResolver: Send + Sync {
    fn name(&self) -> &str;
    async fn can_resolve(&self, input: &str, config: &AtcConfig) -> bool;
    async fn resolve(&self, input: &str, opts: &RunOpts, config: &AtcConfig) -> Result<ResolvedInput>;
    async fn on_cleanup(&self, record: &DispatchRecord, config: &AtcConfig, registry: Option<&dyn Registry>);
}
```

- **TaskResolver** — all GitKB integration (`git kb show/assign/unassign`). Can be disabled via config.
- **TemplateResolver** — renders Handlebars templates from the prompts directory.
- **PromptResolver** — fallback; wraps raw strings as prompts.

### Context Providers

Providers run between prompt assembly and agent spawn:

```rust
pub trait ContextProvider: Send + Sync {
    fn name(&self) -> &str;
    fn declared_template_vars(&self) -> &[&str] { &[] }
    async fn prepare(&self, ctx: &DispatchContext) -> Result<ContextOutput>;
}
```

Providers export data via `template_vars` (substituted into templates/partials via deferred placeholders) and `preamble_sections` (prepended to the system prompt). Registered per-directive in config (`providers = ["pr-context", "rebase"]`). Provider errors are non-fatal.

### Registry

SQLite with WAL mode. Dispatch records are queryable by ID, task slug, branch, PR URL, or worktree path.

```sql
CREATE TABLE dispatches (
  id TEXT PRIMARY KEY,          -- e.g. "tasks--foo@implement@1710769200"
  task_slug TEXT,               -- nullable for template/prompt dispatches
  branch TEXT NOT NULL,
  worktree_path TEXT NOT NULL,
  session TEXT NOT NULL,
  log_file TEXT NOT NULL,
  status TEXT NOT NULL,         -- running, done, failed, needs-review, needs-human, stopped, retrying
  directive TEXT NOT NULL,
  resolver TEXT NOT NULL,       -- "task", "template", "prompt"
  ...
);
```

## Competitive Comparison (as of March 2026)

| Capability | **ATC** | **Symphony** | **Composio AO** | **Gastown** | **Overstory** | **Vibe Kanban** |
|---|---|---|---|---|---|---|
| **Dispatch model** | Queue + daemon + direct | Daemon (Linear poll) | Daemon (30s poll) | Daemon (patrol cycles) | Coordinator spawn | Manual (kanban) |
| **Tracker integration** | GitKB + pluggable | Linear only | GitHub/Linear/Jira | Built-in (git-backed) | None | Built-in kanban |
| **Agent runtimes** | Any CLI agent | Codex + similar | Claude/Codex/Aider/OpenCode | Claude/Copilot | 11 runtimes | Claude/Codex/Cursor/Gemini/Amp |
| **Knowledge/context** | GitKB graph + code intel | WORKFLOW.md only | Codebase reading | Git-backed state | SQLite mail | MCP bidirectional |
| **Code intelligence** | AST call graphs (17 langs) | None | None | None | None | None |
| **Persistence** | SQLite registry + queue | In-memory | File-based | Git-backed | SQLite (WAL) | File-based |
| **Health monitoring** | 6-signal (exit->CI->reviews) | Stall detection | 3x retry + escalate | Witness + Deacon | 3-tier watchdog | None |
| **Workspace isolation** | Git worktrees | Per-issue dirs | Git worktrees + tmux | Git worktrees | Git worktrees + tmux | Git worktrees |
| **Multi-repo** | Native (meta) | No | No | Multi-project rigs | Multi-repo | No |
| **CI feedback loop** | Via health signals | Yes | Auto-fix | Via Witness | Via Watchdog | No |
| **Adaptive retry** | Yes (failure-aware) | Exponential backoff | 3x retry | Recovery cycles | Tiered escalation | No |
| **License** | MIT | Apache-2.0 | MIT | MIT | Unspecified | Apache-2.0 |

### Key Differentiators

**1. Knowledge-backed dispatch (unique to ATC)**
No other orchestrator integrates with a structured knowledge base. Symphony agents read a Linear ticket and start coding. ATC agents get full task documents with acceptance criteria, code call graphs, impact analysis, and organizational memory that persists across sessions.

**2. Agent-agnostic**
ATC's `AgentExecutor` trait means any CLI agent works. Most orchestrators support a limited set of runtimes. ATC lets teams use the best agent for the job without code changes.

**3. Multi-repo native**
ATC + meta creates worktrees across multiple repos for a single task. While Gastown and Overstory support multi-project coordination, ATC's meta-repo approach handles cross-repo changes as a single atomic dispatch.

**4. 6-signal health (most comprehensive open-source)**
ATC checks the full lifecycle: agent exited > branch pushed > PR created > CI passed > reviews approved > threads resolved. Other tools implement partial monitoring (Symphony checks proof-of-work, Overstory uses tiered watchdogs), but ATC's 6-signal chain covers the complete dispatch-to-merge path.

**5. Queue-first architecture**
ATC separates selection (what) from scheduling (when) -- `kb_ready` scoring, board queries, saved views, event streams, custom scripts, or manual enqueue all feed the same queue. Other tools offer some dispatch flexibility (Composio AO's planner/executor, Gastown's formulas), but ATC's queue abstraction makes every selection strategy composable with every scheduling policy.

## Health Check Signals

```text
Signal 1: agent_exited_clean  -> tmux session terminated
Signal 2: branch_pushed       -> git ls-remote finds branch on origin
Signal 3: pr_created          -> gh pr list finds PR for branch
Signal 4: ci_passed           -> gh pr checks shows no failures
Signal 5: reviews_approved    -> gh pr view shows APPROVED
Signal 6: threads_resolved    -> GraphQL query finds 0 unresolved threads
```

All signals pass -> **Done**. Agent exited + any failure -> **NeedsReview** or **Failed**.

`atc health --auto` auto-dispatches `review-fix` for NeedsReview records.

## Adaptive Retry

```bash
atc retry <id>
```

Classifies failures and adjusts config:
- `error_max_turns` -> doubles `max_turns`
- `error_max_budget_usd` -> doubles budget
- Other -> retries with same config
- Max 3 retries, then escalates to `needs-human`

## Per-Project Environment

Place a `.dispatch/env` file in your repo to set agent-specific environment variables:

```bash
# .dispatch/env
RUST_LOG=debug
JOBS=16
CARGO_FLAGS="--features experimental"
```

Loaded automatically on dispatch. Disable with `dispatch.project_env = false`.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ATC_CONFIG` | Config file path |
| `ATC_ROOT` | Data directory (default `~/.local/share/atc`) |
| `ATC_CI` | Set to `true` for inline execution (no tmux) |
| `ATC_NOTIFY_WEBHOOK` | Webhook URL for completion notifications |
| `GITKB_ROOT` | Set by TaskResolver for agent's KB access |
| `GITKB_WORKTREE` | Set by TaskResolver for per-branch indexing |
| `GH_TOKEN` | GitHub auth (resolved from env or `gh auth token`) |
| `AGENT_ALLOWED_PATHS` | File sandbox paths for agent (computed by ATC) |
| `CLAUDECODE` | Always set to empty string to prevent recursive agent-spawning |

## License

MIT
