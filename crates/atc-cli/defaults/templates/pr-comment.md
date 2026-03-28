---
description: Fix specific PR comments without full review — lightweight and fast
directive: pr-comments
worktree: branch
required_params: [pr]
---

# Fix PR Comments: {{pr}}

## Pre-fetched Data

{{prefetch}}

## Setup

1. Read `.dispatch-prefetch/summary.md` for PR status overview — if merged/closed, stop
{{#if worktree}}
1. `cd {{worktree}}` and confirm the checked-out branch matches the PR branch. Pull latest.
{{/if}}
{{#unless worktree}}
1. Checkout the PR branch and pull latest
{{/unless}}

## Fix Comments

{{#if comment}}
A specific comment has been targeted. Look for the **TARGET COMMENT** section in the pre-fetched data above.

1. Read the target comment carefully
2. If it references a file/line, go to that location. Otherwise search for the relevant code.
3. Make the requested change
4. Commit with a descriptive message
5. Reply to the comment: explain what you did
6. Resolve the thread if you have permission

**Scope limit**: Only fix what this one comment asks for. Do NOT address other comments, do NOT run a full code review, do NOT refactor surrounding code.
{{/if}}
{{#unless comment}}
Address all unresolved PR comments. Read `.dispatch-prefetch/triage.md` for the pre-triaged checklist.

{{>github-comments}}

**Scope limit**: Only fix what the comments ask for. Do NOT run a full code review, do NOT look for additional issues, do NOT refactor surrounding code. Stay focused on the comments.
{{/unless}}

## Push, Comment, Resolve

After fixing:
1. Push all commits
2. `gh pr comment {{pr}}` summarizing what this push fixed (commit hashes + what each addressed)
3. Resolve each review thread that was fixed

**Exit when**: all targeted comments addressed, threads resolved. Do NOT poll or wait for CI checks or external review bots — you already ran lint + tests locally.

You can: read/explore code, modify code, run tests, git commit, push/create PRs with gh.
