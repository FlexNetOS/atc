---
description: Local review flywheel — iterate until confident
directive: review-fix
worktree: current
---

# Branch Review

{{#if worktree}}
1. `cd {{worktree}}` — you are reviewing the branch checked out here.
{{/if}}

Review all changes on this branch vs main. Iterate until confident.

## Loop

Repeat:

1. **Review** (`git diff {{default_branch}}...HEAD`, read each changed file in full):
   - Deduplication & reuse
   - Efficiency & performance
   - Maintainability & extensibility
   - Observability
   - Test coverage (unit + integration)
   - Security (red team: malformed input, service failures, races, exploits)

2. **Fix ALL issues.** Don't list them. Fix, commit, move on.
   - If you identified a bug or weakness, fix it — don't just document it and move to Summary.
   - Genuine design choices that need human input are valid to flag, but a known bug with a clear fix is not a design choice.

3. **Build gate** — run verification:
{{>verify}}
   - If anything fails: fix it, commit, loop back to step 1
   - Pre-existing failures unrelated to your branch: note them in Summary but don't block

4. **Confidence** (0-100%): "How confident am I there are zero remaining issues?"
   - < 100% and you know how to fix the gaps: loop back to step 1. Do not proceed to Summary with known fixable issues.
   - < 100% but blocked on human input (design choices, product requirements): proceed to Summary
   - 100%: proceed to Summary

{{#if task}}
Also verify every acceptance criterion in `git kb show {{task}}` is satisfied.
{{/if}}

## Summary

When done, output:
- Findings fixed (commit hashes + what each addressed)
- Tests added
- Build status (clippy + tests: pass/fail, note any pre-existing failures)
- Remaining concerns (if any — things requiring human decision)
- Final confidence rating

You can: read/explore code (code intelligence, grep, read), modify code, run tests, and git commit.
