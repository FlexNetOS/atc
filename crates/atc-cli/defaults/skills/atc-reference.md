# ATC Quick Reference

ATC (Agent Task Controller) dispatches Claude Code agents to isolated git worktrees via tmux sessions.

## Dispatch

```bash
# Task implementation
atc run task <slug>                              # e.g. atc run task tasks/my-task-42
atc run task <slug> --directive <directive>      # e.g. atc run task tasks/my-task-42 --directive research

# PR review (iterative fix flywheel)
atc run pr-review --param pr=<github-pr-url>

# PR comment fix (lightweight, targeted)
atc run pr-comment --param pr=<github-pr-url>

# Local branch review (no PR, current checkout)
atc run branch-review

# Raw prompt (fallback)
atc run '<prompt text>' --directive <directive>

# Continue a previous provider conversation
atc run --resume <dispatch-id-or-task> '<prompt text>'

# Preview without executing
atc run <args> --dry-run

# List available templates
atc run --list
```

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `GITKB_ROOT` | KB root path (required in meta workspaces for task resolution) |
| `DISPATCH_WORKTREE_REPO` | Sub-repo path for worktree creation (e.g. `open-source/my-repo`) |
| `ATC_CI` | Set to `true` to force inline mode (no tmux, for CI pipelines) |
| `ATC_CONFIG` | Override config file path |
| `ATC_ROOT` | Override data directory (default: `~/.local/share/atc`) |
| `ATC_NOTIFY_WEBHOOK` | Webhook URL for POST notifications on completion |

## Monitoring

```bash
atc status                           # grouped by work unit
atc status --flat                    # one row per dispatch
atc status --flat --json             # machine-readable
atc logs <slug-or-id>                # tail agent log
atc logs <slug-or-id> -f             # follow (like tail -f)
atc sessions --json                  # session rows with locator/status/open-shell
atc open-session <slug-or-id>        # attach to tmux-backed session
atc open-session <slug-or-id> --json # preview open-session without attaching
atc watch --id "<dispatch-id>"       # live event stream
atc watch --id "<id>" --pretty       # formatted stream
atc watch --all-running --pretty     # watch all active agents
atc info <id>                        # detailed single dispatch info
```

## Agent Control

```bash
atc redirect <slug-or-id> "<message>"   # send instruction to running agent
atc stop <id>                           # kill session, mark stopped
atc retry <id>                          # re-dispatch failed task fresh with adaptive config
atc close <slug> --pr <url>             # mark done, remove worktree, update KB
atc cleanup <id>                        # remove worktree and session
atc cleanup --done                      # batch cleanup all completed dispatches
```

`atc run --resume` creates a new dispatch that continues the source Claude conversation. `atc retry` intentionally starts a fresh provider conversation, and `atc redirect` targets a running tmux-backed session without creating a new dispatch.

## Health Checks

```bash
atc health                   # 6-signal health check on all active dispatches
atc health --auto            # auto-dispatch review-fix for NeedsReview records
atc health --all --json      # include done/failed, machine-readable
```

Signals checked: agent exited clean, branch pushed, PR created, CI passed, reviews approved, threads resolved.

## Queue & Daemon

```bash
# Enqueue work
atc enqueue task <slug>
atc enqueue --ready --limit 3        # enqueue ready tasks from KB board
atc enqueue --board --status ready   # enqueue from board with filters

# Process queue
atc queue                            # list pending items
atc queue drain                      # dispatch all pending, then exit
atc queue clear                      # remove all pending items

# Continuous daemon
atc daemon                           # start with default sources
atc daemon --source ready --max-concurrent 5
atc daemon status
atc daemon stop
```

## Templates

| Template | Directive | Required Params | Worktree | Purpose |
|----------|-----------|-----------------|----------|---------|
| `pr-review` | `review-fix` | `pr` | branch | Iterative PR review + fix flywheel |
| `pr-comment` | `pr-comments` | `pr` | branch | Fix targeted review comments |
| `branch-review` | `review-fix` | — | current | Local review, no PR interaction |
| `close` | `close` | `task` | document | Verify and close a task |
| `push-branch` | `implement` | — | current | Validate and push to remote |
| `swot` | `research` | `competitor`, `name` | none | Competitive SWOT analysis |
| `doc-edit` | `implement` | `slug`, `directive` | document | Edit a KB document |

## Directives

| Directive | Purpose |
|-----------|---------|
| `implement` | Write code (default) |
| `review-fix` | Iterative PR review + fix (requires PR URL) |
| `pr-comments` | Fix specific PR comments (requires PR URL) |
| `research` | Read-only exploration and analysis |
| `refine` | Iteratively improve a KB document |
| `create-task` | Create a new KB task |
| `kb-update` | Update KB documents |
| `close` | Verify completion and close a task |

## Run Flags

| Flag | Purpose |
|------|---------|
| `--directive <name>` | Override directive |
| `--param key=value` | Template parameter (repeatable) |
| `--pr-url <url>` | PR URL (alternative to `--param pr=`) |
| `--repo <path>` | Target repo in meta workspace (repeatable) |
| `--dry-run` | Preview without executing |
| `--inline` | Run synchronously (no tmux) |
| `--force` | Force dispatch even if worktree/session exists |
| `--no-worktree` | Skip worktree creation |
| `--max-budget-usd <N>` | Override budget limit |
| `--max-turns <N>` | Override turn limit |
| `--resume <id-or-task>` | Continue the source provider conversation in a new dispatch |
| `--ephemeral` | No registry/logs/system-prompt (requires `--inline`) |
| `--timeout <secs>` | Kill after N seconds (inline only) |
| `--list` | List available templates |

## Project Setup

```bash
atc init                    # scaffold .atc/, then prompt to wire your agent (TTY)
atc init --no-interactive   # scaffold only, skip the picker (CI / scripts)
atc init --force            # overwrite .atc/, then prompt
atc init claude             # wire .atc/skills into .claude/skills/atc (symlink)
atc init agents             # wire .atc/skills into .agents/skills/atc
atc init <agent> --copy     # copy files instead of symlinking
atc init --all-agents       # wire every detected agent in one shot
atc init --list-agents      # show registry + current wire-up status
atc init --interactive      # picker only (skip .atc/ scaffold; for re-wire flows)
```

`atc init <agent>` is idempotent: a correct symlink is a no-op, a wrong-target
symlink errors without `--force`, and a real user directory is never deleted.

## Configuration

ATC looks for config in this order:
1. `--config <path>` flag
2. `ATC_CONFIG` env var
3. `.atc/config.toml` (walking up from CWD)
4. `atc.toml` (walking up from CWD)
5. `~/.config/atc/config.toml`

## Resolver Chain

Input is matched against resolvers in order (first match wins):
1. **Task** — matches task slugs via `git kb show`
2. **Template** — matches `.md` filenames in templates directory
3. **Prompt** — catch-all, wraps raw text as prompt

PR URLs passed as positional args hit the Prompt resolver (wrong). Always use `--param pr=` or `--pr-url`.
