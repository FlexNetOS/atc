### Loading Comments

All data is in `.dispatch-prefetch/` — **do NOT call `gh api` to re-fetch**.

1. `.dispatch-prefetch/triage.md` — **pre-triaged comment checklist** with full comment text and pre-built `gh api` reply/resolve commands
2. `.dispatch-prefetch/comments.json` — full review comments (read for detail when triage entries are truncated)
3. `.dispatch-prefetch/reviews.json` — review summaries with verdicts
4. `.dispatch-prefetch/threads.json` — conversation threads with resolution state

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
