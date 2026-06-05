# Skill: Monitor a Dispatched Agent

Use this when the user asks you to watch, monitor, or check on a running agent.

## Check What's Running

```bash
atc status --flat
atc sessions --json
```

Shows dispatch/session state. `atc status --flat` focuses on dispatch lifecycle; `atc sessions --json` also includes terminal locator, derived terminal status, and open-shell preview state. Statuses: `running`, `done`, `failed`, `stopped`, `needs-review`, `needs-human`, `retrying`.

For machine-readable output: `atc status --flat --json`.

## View Agent Logs

```bash
# By task slug (resolves to most recent dispatch for that task)
atc logs <task-slug>

# By dispatch ID (always unambiguous)
atc logs "<dispatch-id>"

# Follow mode (like tail -f)
atc logs <slug-or-id> -f
```

## Open Agent Session

```bash
atc open-session <dispatch-id>
atc open-session atc://session/<dispatch-id>
atc open-session <task-slug> --json
```

Use `--json` to resolve and preview the terminal action without attaching. Normal mode attaches through ATC's tmux adapter.

### Reading the Log

| Line format | Meaning |
|-------------|---------|
| `>>> <text>` | Agent reasoning / text output |
| `[tool] Read: {...}` | Agent reading a file |
| `[tool] Edit: {...}` | Agent editing a file |
| `[tool] Bash: {...}` | Agent running a command |
| `[tool] Agent: {...}` | Agent spawning a sub-agent |
| `=== RESULT: ... cost=$X turns=N ===` | Agent completed |

## Live Streaming

```bash
atc watch --id "<dispatch-id>"               # raw JSON events
atc watch --id "<dispatch-id>" --pretty      # formatted output
atc watch --all-running --pretty             # all active agents
```

Events emitted: `Started`, `LogLine`, `CostThreshold`, `Completed`, `Failed`, `SessionDied`.

## Detailed Dispatch Info

```bash
atc info <dispatch-id>
```

Shows full record: all metadata, PR URLs, worktree path, cost breakdown, provider output.

## Assessing Agent Progress

Look for these milestones in the log:

1. **Exploring** — `Read` and `Bash` calls, understanding the codebase
2. **Editing** — `Edit` calls, making changes
3. **Committing** — `git add` + `git commit` in Bash calls
4. **Verifying** — `cargo test`, `cargo clippy`, or similar in Bash calls
5. **Pushing** — `git push` in Bash calls
6. **Review flywheel** (review-fix only) — agent states a confidence score, loops if < threshold

## When to Intervene

| Symptom | Action |
|---------|--------|
| Agent loops on same action | `atc redirect <id> "try a different approach: ..."` |
| Agent went off-scope | `atc redirect <id> "stop. focus only on ..."` |
| Agent stuck on push (hook failure) | Check if pre-push hook fails on unrelated tests. Push manually from worktree if needed. |
| Agent exhausted budget | `atc retry <id>` (doubles budget automatically) |
| Agent hit max turns | `atc retry <id>` (doubles turns automatically) |
| Agent needs to stop | `atc stop <id>` |

## After Completion

```bash
# Verify what the agent did
cd <worktree-path>
git log --oneline <default-branch>..HEAD

# Run health checks
atc health

# Auto-dispatch review-fix for any NeedsReview dispatches
atc health --auto

# Clean up completed dispatches
atc cleanup --done

# Close a task (marks done in KB, removes worktree)
atc close <slug> --pr <url>
```
