---
name: atc-dispatch
description: Use when the user asks to dispatch, run, or send an agent via ATC to work on a task, PR, branch review, or raw prompt. Covers choosing the right `atc run ...` pattern, setting ATC environment like `GITKB_ROOT` or `DISPATCH_WORKTREE_REPO` when needed, previewing with `--dry-run`, and starting the dispatch cleanly.
metadata:
  short-description: Dispatch an ATC agent
---

# Dispatch an Agent via ATC

Use this when the user asks you to dispatch, run, or send an agent to work on a task, PR, branch review, or prompt.

If the user only wants ATC command lookup or CLI reference, read [../atc-reference.md](../atc-reference.md) instead of treating the request as a dispatch workflow.

## Workflow

## 1. Classify the request

- Task implementation: `atc run task <slug>`
- Task implementation with a specific directive: `atc run task <slug> --directive <directive>`
- PR review: `atc run pr-review --param pr=<url>`
- PR comment follow-up: `atc run pr-comment --param pr=<url>`
- Local branch review: `atc run branch-review`
- Raw prompt fallback: `atc run '<text>' --directive <directive>`

For more examples, read [../dispatch.md](../dispatch.md).

## 2. Set environment only when needed

In a meta workspace, set `GITKB_ROOT` to the workspace root so the task resolver can find KB documents:

```bash
export GITKB_ROOT=<workspace-root>
```

If the target code lives in a sub-repo, set `DISPATCH_WORKTREE_REPO` to its path within the meta tree:

```bash
export DISPATCH_WORKTREE_REPO=<relative/path/to/repo>
```

Discover valid repo paths with `meta project list --recursive`.

## 3. Dry-run if anything is ambiguous

Preview before dispatch when the target, resolver, directive, repo, or worktree policy is not obvious:

```bash
atc run <args> --dry-run
```

Verify in the output:
- **Resolver** is `task` or `template` (not `prompt` - that's the catch-all fallback)
- **Directive** matches intent
- **Branch** is correct (especially for PR reviews - should show the PR head branch)
- **Worktree** policy is appropriate (`branch` for PRs, `current` for local work, `none` for research)

## 4. Dispatch

Run the command without `--dry-run`. The output shows the dispatch ID, branch, worktree path, and suggested next-step commands.

Capture the dispatch ID when available so later monitoring and redirects are unambiguous.

## 5. Hand off to monitoring when needed

If the user asks to watch or assess the dispatch after starting it, use the `atc-monitor` skill.

## Common Mistakes

| Mistake | Why it's wrong | Correct |
|---------|---------------|---------|
| `atc run https://github.com/.../pull/123` | URL becomes a raw prompt | `atc run pr-review --param pr=<url>` |
| Forgetting `DISPATCH_WORKTREE_REPO` | Worktree created at wrong level | Set to the sub-repo path |
| Forgetting `GITKB_ROOT` | Task resolver can't find KB documents | Set to workspace root |
| `--directive review-fix` without PR URL | Pipeline bails - review-fix requires a PR | Add `--pr-url <url>` or `--param pr=<url>` |
| Using `--no-worktree` for code changes | Agent modifies the primary checkout | Let the default worktree policy handle isolation |

## References

- [../dispatch.md](../dispatch.md): longer dispatch examples and operational notes
- [../atc-reference.md](../atc-reference.md): full command, flags, templates, and resolver reference
