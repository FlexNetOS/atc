# ATC (Air Traffic Control)

A headless agent orchestrator for AI coding agents. ATC dispatches agents to work on tasks in isolated git worktrees, monitors their lifecycle through a 6-signal health system, and tracks everything in a SQLite registry.

## Install

```bash
brew tap harmony-labs/tap
brew install atc
```

Or build from source:

```bash
cargo install --path crates/atc-cli
```

## Quick Start

```bash
# Dispatch an agent to implement a task
atc run task tasks/my-task

# Dispatch with a template
atc run pr-review --param pr=https://github.com/org/repo/pull/123

# Dispatch with a raw prompt
atc run 'Fix the auth timeout bug in src/auth.rs'

# Check what's running
atc status

# Tail agent logs
atc logs -f <id>

# Check health of all dispatches
atc health
```

## How It Works

ATC manages the full lifecycle of an AI agent dispatch:

```text
atc run → resolve input → create worktree → assemble prompt → run providers → spawn agent → monitor → post-complete
```

1. **Input Resolution** — `atc run` accepts three input types, resolved by pluggable `InputResolver` implementations:
   - `task <slug>` — fetch task from GitKB, claim via CAS, pipe document to agent
   - `<template>` — render Handlebars template with `--param` variables
   - `'<prompt>'` — pass raw string directly to agent

2. **Worktree Isolation** — each dispatch gets its own git worktree. Existing worktrees are reused. Collision detection prevents two agents on the same worktree.

3. **Prompt Assembly** — system prompts are assembled from composable component `.md` files per mode config. Templates use [Handlebars](https://handlebarsjs.com/) syntax with 3-level partial resolution.

4. **Context Providers** — pluggable pre-dispatch data assembly. Providers run after prompt assembly, before agent spawn:
   - **PR Context** — fetches PR metadata, comments, reviews, threads in parallel. Generates `triage.md` and `summary.md` in `.dispatch-prefetch/`.
   - **KB Context** — fetches related documents and active context from GitKB.
   - **Rebase** — detects if branch is behind main and injects rebase instructions.

5. **Agent Execution** — spawns `claude` in a tmux session (detached) or inline (synchronous for CI). Stream-json output is logged to JSONL files.

6. **Post-Completion** — extracts artifacts from stream-json logs (cost, PR URLs, commits, summary), updates registry, sends notifications (macOS + webhook), and auto-cleans worktrees on PR merge.

7. **Health Monitoring** — 6-signal state machine evaluates dispatches without consuming tokens:
   - Agent exited (tmux session gone)
   - Branch pushed to remote
   - PR created
   - CI passed
   - Reviews approved
   - Review threads resolved

## Commands

| Command | Description |
|---------|-------------|
| `atc run <input>` | Dispatch an agent (task, template, or prompt) |
| `atc status` | Table view of all dispatches |
| `atc info <id>` | Detailed view of a single dispatch |
| `atc logs [-f] <id>` | Tail stream-json logs (human-readable) |
| `atc health [--auto]` | Run 6-signal health checks; `--auto` dispatches review-fix |
| `atc watch [--format ndjson]` | Stream live events from running agents |
| `atc stop <id>` | Kill tmux session, mark stopped |
| `atc cleanup <id>` | Remove worktree, unassign task |
| `atc retry <id>` | Re-dispatch with adaptive config (double turns / double budget) |
| `atc redirect <id> '<msg>'` | Send message to running agent via tmux |
| `atc close <slug>` | Verify task completion and close |
| `atc post-complete [--id <id>]` | Run post-completion (auto or manual recovery) |
| `atc prompt <mode>` | Preview rendered system prompt |

## Configuration

ATC loads config from (in priority order):
1. `--config <path>` flag
2. `ATC_CONFIG` environment variable
3. `./atc.toml` (current directory)
4. `~/.config/atc/config.toml`

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
components_dir = ".claude/prompts/components"
templates_dir = ".claude/prompts/templates"
partials_dir = ".claude/prompts/partials"

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
```

## Architecture

```text
atc-core/                          atc-cli/
├── config.rs        Config        ├── pipeline.rs      DispatchPipeline
├── executor.rs      AgentExecutor ├── resolvers/
├── health.rs        HealthChecker │   ├── task.rs       TaskResolver (GitKB)
├── post_completion.rs             │   ├── template.rs   TemplateResolver
├── prompt_engine.rs  Handlebars   │   └── prompt.rs     PromptResolver
├── providers/                     ├── dispatch.rs      Shared dispatch utils
│   ├── pr_context.rs              ├── resolve.rs       Resolver invocation
│   ├── kb_context.rs              ├── subprocess.rs    Subprocess execution
│   └── rebase.rs                  ├── watch.rs         Agent watcher
├── registry.rs      SQLite        ├── status.rs        Status table
├── resolver.rs      InputResolver ├── info.rs          Detail view
├── stream_json.rs   Log parser    ├── logs.rs          Log viewer
├── templates.rs     Prompt render ├── stop.rs          Stop command
├── project_env.rs   .dispatch/env ├── cleanup.rs       Cleanup command
├── types.rs         Core types    ├── retry.rs         Adaptive retry
└── worktree.rs      Cleanup       ├── health.rs        Health CLI
                                   ├── redirect.rs      Tmux injection
                                   ├── close.rs         Task closure
                                   └── post_complete.rs Post-completion
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
    async fn prepare(&self, ctx: &DispatchContext) -> Result<ContextOutput>;
}
```

Registered per-mode in config (`providers = ["pr-context", "rebase"]`). Provider errors are non-fatal — logged and skipped.

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
  mode TEXT NOT NULL,
  resolver TEXT NOT NULL,       -- "task", "template", "prompt"
  ...
);
```

## Agent Watcher

`atc watch` streams structured events from running agents, consumable by other AI harnesses:

```bash
# NDJSON for AI harness consumption
atc watch --format ndjson | your-orchestrator

# Human-readable for terminal
atc watch

# Watch all running dispatches
atc watch --all-running

# Multi-consumer via Unix socket
atc watch --socket /tmp/atc-events.sock
```

Events: `started`, `log_line`, `cost_threshold`, `completed`, `failed`, `session_died`.

## Per-Project Environment

Place a `.dispatch/env` file in your repo to set agent-specific environment variables:

```bash
# .dispatch/env
RUST_LOG=debug
JOBS=16
CARGO_FLAGS="--features experimental"
```

Loaded automatically on dispatch. Disable with `dispatch.project_env = false`.

## Health Check Signals

```text
Signal 1: agent_exited_clean  → tmux session terminated
Signal 2: branch_pushed       → git ls-remote finds branch on origin
Signal 3: pr_created          → gh pr list finds PR for branch
Signal 4: ci_passed           → gh pr checks shows no failures
Signal 5: reviews_approved    → gh pr view shows APPROVED
Signal 6: threads_resolved    → GraphQL query finds 0 unresolved threads
```

All signals pass → **Done**. Agent exited + any failure → **NeedsReview** or **Failed**.

`atc health --auto` auto-dispatches `review-fix` for newly-transitioned NeedsReview records.

## Adaptive Retry

```bash
atc retry <id>
```

Classifies failures and adjusts config:
- `error_max_turns` → doubles `max_turns`
- `error_max_budget_usd` → doubles budget
- Other → retries with same config
- Max 3 retries, then escalates to `needs-human`

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ATC_CONFIG` | Config file path |
| `ATC_ROOT` | Data directory (default `~/.local/share/atc`) |
| `ATC_CI` | Set to `true` for inline mode (no tmux) |
| `ATC_NOTIFY_WEBHOOK` | Webhook URL for completion notifications |
| `GITKB_ROOT` | Set by TaskResolver for agent's KB access |
| `GITKB_WORKTREE` | Set by TaskResolver for per-branch indexing |
| `GH_TOKEN` | GitHub auth (resolved from env or `gh auth token`) |
| `AGENT_ALLOWED_PATHS` | File sandbox paths for agent (computed by ATC; user values extend the worktree anchor) |
| `CLAUDECODE` | Always set to empty string by ATC to prevent recursive agent-spawning |

## License

MIT
