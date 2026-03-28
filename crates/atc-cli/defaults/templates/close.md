---
description: Verify and close a completed task with evidence
directive: close
worktree: document
required_params: [task]
---

# Close: {{task}}

Verify that the task is actually complete, then close it with evidence.

## Step 1: Understand the Task

The task document is provided on stdin. Read it carefully:
- Note ALL acceptance criteria (`- [ ]` checkboxes)
- Note the goals and overview
- Understand what "done" means for this task

## Step 2: Review the Implementation

{{#if pr}}
Review the merged PR to understand what shipped:

1. `gh pr view {{pr}} --json title,body,state,mergeCommit,mergedAt`
2. `gh pr diff {{pr}}` — read the actual changes
3. Identify what was implemented and how
{{/if}}
{{#unless pr}}
No PR was provided. Use `git kb graph {{task}}` and git log to find related commits and changes.
{{/unless}}

## Step 3: Audit Acceptance Criteria

For each `- [ ]` criterion in the task document:
1. Determine if the merged code satisfies it
2. Look for evidence: tests, implementation, documentation
3. Mark your assessment: satisfied or unsatisfied

## Step 4: Check for Orphaned Work

Run `git kb graph {{task}}` to find related documents (child tasks, incidents, specs).
Verify no important linked work is left incomplete.

## Step 5: Update the Task Document

1. `git kb checkout {{task}}`
2. Edit `.kb/workspace/{{task}}.md`:
   - Check off (`- [x]`) all satisfied acceptance criteria
   - Add notes to any unsatisfied criteria explaining what's missing
   - Add a `## Completion Evidence` section:
{{#if pr}}
     - **PR**: {{pr}}
     - **Merge commit**: (SHA from `gh pr view`)
     - **Merged at**: (timestamp from `gh pr view`)
{{/if}}
     - **Summary**: Brief description of what shipped
     - **Verified by**: close agent audit
   - Add a progress log entry with today's date
3. `git kb commit -m "Close {{task}} with verification" {{task}}`

## Step 6: Set Final Status

- If ALL acceptance criteria are satisfied:
  `git kb set {{task}} --status done`
- If some criteria are NOT satisfied:
  `git kb set {{task}} --status needs-review`
  Add a note explaining which criteria are unmet and why.

You can: read KB documents, modify KB documents, use gh CLI for GitHub operations.
