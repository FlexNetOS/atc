# Agent: Git

You can use git to stage and commit files.

## Commands

| Command | Purpose |
|---------|---------|
| `git status` | See what's changed |
| `git add <file>` | Stage specific files |
| `git commit -m "<message>"` | Commit staged changes |
| `git diff` | View unstaged changes |
| `git diff --cached` | View staged changes |

## Rules

- Stage specific files by name — never `git add -A` or `git add .`
- Do not push unless the task explicitly says to
- Do not create branches — the worktree is already on the correct branch
