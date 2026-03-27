### Loading Comments

Read `.dispatch-prefetch/triage.md` — this is your **pre-triaged comment checklist**, already sorted by priority with resolution status, severity, and pre-built `gh api` commands.

If `triage.md` doesn't exist, fall back to raw JSON:
- `.dispatch-prefetch/comments.json` — all review comments
- `.dispatch-prefetch/reviews.json` — review summaries with verdicts

If `.dispatch-prefetch/` doesn't exist at all, fetch directly:
```bash
PR_REPO=$(printf '%s\n' "$PR_URL" | sed -E 's|^https://github.com/([^/]+/[^/]+)/pull/.*$|\1|')
PR_NUMBER=$(printf '%s\n' "$PR_URL" | sed -E 's|^https://github.com/[^/]+/[^/]+/pull/([0-9]+).*$|\1|')
gh api repos/$PR_REPO/pulls/$PR_NUMBER/comments
gh api repos/$PR_REPO/pulls/$PR_NUMBER/reviews
```

### Working Through the Triage

The checklist is sorted by priority: human changes_requested → human comments → CodeRabbit (critical→major→minor→nitpick) → Greptile → informational. Work through it top to bottom.

For each `- [ ]` entry:
1. Read the quoted comment and understand the requested change
2. **Check if already fixed** — if the code already addresses the comment, reply noting it's resolved and resolve the thread
3. If the entry has a `Suggestion:` block, you can apply it directly
4. Locate the relevant code using the file path and line in the entry
5. Make the code change
6. Commit with a message describing the fix
7. Reply using the pre-built `Reply:` command in the entry
8. Resolve the thread using the pre-built `Resolve:` command in the entry

Resolved and outdated threads are collapsed in `<details>` — skip them unless you need to verify a previous fix.

### Completion

Loop until:
- All `- [ ]` items in the Unresolved section are addressed
- All conversation threads are resolved
- No pending `changes_requested` reviews remain
