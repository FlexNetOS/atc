# Skill: Dispatch an Agent via ATC

Use this when the user asks you to dispatch, run, or send an agent to work on a task, PR, or prompt.

## Step 1: Determine Dispatch Type

| User intent | Command pattern |
|-------------|----------------|
| Implement a task | `atc run task <slug>` |
| Implement with explicit directive | `atc run task <slug> --directive <directive>` |
| Review a PR | `atc run pr-review --param pr=<url>` |
| Fix specific PR comments | `atc run pr-comment --param pr=<url>` |
| Review current branch locally | `atc run branch-review` |
| Research/explore a task | `atc run task <slug> --directive research` |
| Close/verify a task | `atc run close --param task=<slug>` |
| Raw prompt | `atc run '<text>' --directive <directive>` |
| Continue previous provider context | `atc run --resume <dispatch-id-or-task> '<text>'` |

## Step 2: Set Environment

In a meta workspace, set `GITKB_ROOT` to the workspace root so the task resolver can find KB documents:

```bash
export GITKB_ROOT=<workspace-root>
```

If the target code lives in a sub-repo, set `DISPATCH_WORKTREE_REPO` to its path within the meta tree:

```bash
export DISPATCH_WORKTREE_REPO=<relative/path/to/repo>
```

Discover valid repo paths with `meta project list --recursive`.

## Step 3: Dry-Run First

When unsure about the command, always preview:

```bash
atc run <args> --dry-run
```

Verify in the output:
- **Resolver** is `task` or `template` (not `prompt` — that's the catch-all fallback)
- **Directive** matches intent
- **Branch** is correct (especially for PR reviews — should show the PR head branch)
- **Worktree** policy is appropriate (`branch` for PRs, `current` for local work, `none` for research)

## Step 4: Dispatch

Run the command without `--dry-run`. The output shows the dispatch ID, branch, worktree path, and suggested next-step commands.

## Step 5: Monitor

```bash
atc logs <slug-or-id>              # tail the log
atc watch --id "<dispatch-id>"     # live event stream
atc status --flat                  # overview of all dispatches
```

## Step 6: Intervene If Needed

```bash
atc redirect <slug-or-id> "<message>"   # redirect a running agent
atc stop <id>                           # kill a stuck agent
```

Use `atc run --resume <dispatch-id-or-task> <input>` when the user wants a new dispatch that continues the same provider conversation. Use `atc retry <id>` for failed dispatches that should start fresh with adaptive settings, and `atc redirect <id> "<message>"` only for a currently running tmux-backed session.

## Common Mistakes

| Mistake | Why it's wrong | Correct |
|---------|---------------|---------|
| `atc run https://github.com/.../pull/123` | URL becomes a raw prompt | `atc run pr-review --param pr=<url>` |
| Using `atc retry` to continue context | Retry starts a fresh provider conversation | `atc run --resume <id-or-task> '<text>'` |
| Forgetting `DISPATCH_WORKTREE_REPO` | Worktree created at wrong level | Set to the sub-repo path |
| Forgetting `GITKB_ROOT` | Task resolver can't find KB documents | Set to workspace root |
| `--directive review-fix` without PR URL | Pipeline bails — review-fix requires a PR | Add `--pr-url <url>` or `--param pr=<url>` |
| Using `--no-worktree` for code changes | Agent modifies the primary checkout | Let the default worktree policy handle isolation |
