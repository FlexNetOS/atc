---
description: Review and fix PR — iterative flywheel until confident
directive: review-fix
worktree: branch
required_params: [pr]
---

# PR Review: {{pr}}

## Pre-fetched Data

{{prefetch}}

## Phase 1: Setup

1. Read `.dispatch-prefetch/summary.md` — if merged/closed, stop
{{#if worktree}}
1. `cd {{worktree}}` — confirm branch matches PR. Pull latest.
{{/if}}
{{#unless worktree}}
1. Checkout the PR branch, pull latest.
{{/unless}}
1. Rebase onto latest main

## Phase 2: Fix Review Comments

Read `.dispatch-prefetch/triage.md` — your pre-triaged checklist with resolution status and priority sorting.

{{>github-comments}}

## Phase 3: Review Flywheel

After all comments are resolved, review ALL branch changes. Iterate until confident.

{{>review}}

After fixing all issues found, assess **Confidence** (0-100%): "How confident am I this code is production-ready?"
- < 100% and fixable: loop back to review
- < 100% but blocked (needs human direction): leave a PR comment describing gaps, proceed to Phase 4
- 100%: proceed to Phase 4

{{#if task}}
Also verify every acceptance criterion in `git kb show {{task}}` is satisfied.
{{/if}}

## Phase 4: Push and Finalize

1. Push all commits
2. `gh pr comment {{pr}}` — post a summary:
   - **If fixes were made**: include a fix table with @mentions to notify reviewers and trigger bot re-reviews:
     ```
     | Commit | Comment | Fix |
     |--------|---------|-----|
     | abc123 | Critical: description (@reviewer) | What you changed |
     ```
   - **If no fixes were needed**: post a short summary only — do NOT @mention anyone
   - Always include: build status (clippy + tests), confidence rating, anything flagged for human review
3. Resolve all review threads fixed by this push

**Exit when**: final summary posted, all threads resolved. Do NOT poll or wait for CI checks or external review bots (CodeRabbit, Greptile, etc.) — you already ran lint + tests locally.

You can: read/explore code (code intelligence, grep, read), modify code, run tests, git commit, push/create PRs with gh, rebase onto main.
