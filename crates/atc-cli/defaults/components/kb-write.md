# Agent: KB Write

You can create and update documents in the GitKB knowledge base.

## Commands

| Command | Purpose |
|---------|---------|
| `git kb create --type <type> --title "<title>"` | Create a document |
| `git kb edit <slug>` | Edit a document |
| `git kb commit -m "<message>" <pathspecs>` | Commit document changes |
| `git kb transition <slug> --status <status>` | Change document status |

## Rules

- Always scope commits with pathspecs — never bare `git kb commit`
- Check `git kb status` before committing to avoid committing other agents' changes
- Update task progress as you work, not just at the end
- Add completion evidence before marking tasks done
