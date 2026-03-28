🤖 **Agent dispatch: `{{directive}}`**

{{#if task}}**Task:** `{{task}}`
{{/if}}**Branch:** `{{branch}}`
**Worktree:** `{{worktree}}`
**Session:** `{{session}}`

```bash
# Watch live:
atc watch --id {{session}}
# View logs:
atc logs {{session}}
# Attach to tmux:
tmux attach -t {{session}}
```
