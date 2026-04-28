---
name: atc-monitor
description: Use when the user asks to watch, monitor, inspect, or check on a running or completed ATC agent dispatch. Covers `atc status`, `atc logs`, `atc watch`, `atc info`, interpreting log output, deciding when to intervene, and post-run cleanup or verification.
metadata:
  short-description: Monitor an ATC dispatch
---

# Monitor a Dispatched Agent

Use this when the user asks you to watch, monitor, inspect, or assess a running or completed ATC dispatch.

If the user only wants a command summary, event glossary, or flag lookup, read [../atc-reference.md](../atc-reference.md) instead of loading this workflow.

## Workflow

## 1. Identify the dispatch

- Prefer a dispatch ID when the user provides one.
- If the user gives a task slug or fuzzy description, use `atc status --flat` first to identify the current or most relevant dispatch.
- Use `atc info <dispatch-id>` when you need exact metadata such as worktree path, PR URL, or provider output.

## 2. Inspect the live state

```bash
atc status --flat
atc logs <slug-or-id> -f
atc watch --id "<dispatch-id>" --pretty
```

For command variants and event names, read [../monitor.md](../monitor.md).

## 3. Assess progress generically

- Exploration: reading files, running discovery commands, gathering context
- Editing: making code or document changes
- Verification: running project-appropriate checks for that repo or artifact
- Delivery: committing, pushing, updating a PR, or otherwise closing the loop
- Review iteration: applying feedback or looping through review-fix steps

Do not assume Rust tooling or a `main` branch. Base your assessment on the repo, PR, or task context you actually see.

## 4. Intervene when the agent is off track

| Symptom | Action |
|---------|--------|
| Agent loops on same action | `atc redirect <id> "try a different approach: ..."` |
| Agent went off-scope | `atc redirect <id> "stop. focus only on ..."` |
| Agent stuck on push (hook failure) | Check if pre-push hook fails on unrelated tests. Push manually from worktree if needed. |
| Agent exhausted budget | `atc retry <id>` (doubles budget automatically) |
| Agent hit max turns | `atc retry <id>` (doubles turns automatically) |
| Agent needs to stop | `atc stop <id>` |

## 5. Verify a completed dispatch

```bash
cd <worktree-path>
git status
git log --oneline --decorate -n 10
atc health
```

Then run project-appropriate validation commands for that repo rather than assuming a fixed stack.

## References

- [../monitor.md](../monitor.md): detailed logging, watch, and intervention examples
- [../atc-reference.md](../atc-reference.md): full ATC command and flag reference
