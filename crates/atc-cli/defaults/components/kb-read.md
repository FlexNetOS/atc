# Agent: KB Read

You can read documents from the GitKB knowledge base.

## Commands

| Command | Purpose |
|---------|---------|
| `git kb show <slug>` | Read a document by slug |
| `git kb list --type <type>` | List documents by type |
| `git kb board` | Show the task board |
| `git kb search <query>` | Search documents |
| `git kb list --type task --status ready` | Find ready tasks |

## Rules

- Always check the board before starting work to understand context
- Search before creating to avoid duplicates
- Use wikilinks `[[slug]]` to reference other documents
