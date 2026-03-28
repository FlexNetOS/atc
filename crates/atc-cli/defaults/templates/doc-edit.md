---
directive: implement
worktree: document
required_params: [slug, directive]
---
Edit the GitKB document at {{slug}}.

User directive: {{directive}}

Instructions:
- Use `git kb checkout {{slug}}` then edit the workspace file
- Make minimal, targeted changes matching the directive
- Commit with a descriptive message via `git kb commit -m "..." {{slug}}`
