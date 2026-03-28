---
description: Validate build and push branch to remote
directive: implement
worktree: current
---

# Push Branch

{{#if worktree}}
1. `cd {{worktree}}` — you are working in this worktree.
{{/if}}

Validate the branch is clean, then push to remote.

## Steps

1. **Verify branch state**:
   - `git log --oneline {{default_branch}}..HEAD` — confirm there are commits to push
   - `git status` — confirm working tree is clean (no uncommitted changes)
   - If dirty: abort and list the uncommitted changes

2. **Run full CI checks**:
   {{>verify}}
   - If anything fails: report the failures and **stop** — do NOT push

3. **Push**:
   - `git push origin HEAD` (push current branch to remote)
   - If push fails (e.g. pre-push hook), report the error

## Output

- Branch name and commit count
- Lint status: pass/fail
- Test status: pass/fail (with failure details if any)
- Push status: success/failed
