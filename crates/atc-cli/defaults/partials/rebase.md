Rebase your branch onto {{default_branch}} before starting work:
1. `git fetch origin {{default_branch}}`
2. `git rebase origin/{{default_branch}}`
3. If conflicts arise: resolve them, `git add <file>`, `git rebase --continue`
4. If conflicts are extensive (5+ files), note them and stop
5. Verify the build still works after rebase
