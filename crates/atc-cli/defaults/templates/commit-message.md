---
required_params: [slug, diff]
max_turns: 1
---
Summarize the following document changes as a git commit message.
Rules:
- One line, under 72 characters
- Use imperative mood ("Add", "Fix", "Update")
- Focus on WHAT changed and WHY, not HOW
- If frontmatter fields changed, mention the field names
- Do not include the document slug in the message

Document: {{slug}}

Diff:
{{diff}}
