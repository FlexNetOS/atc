---
description: Deep SWOT analysis of a competitor vs GitKB
directive: research
worktree: none
required_params: [competitor, name]
---

Do a deep SWOT analysis of {{competitor}} and GitKB.

## Phase 1: Deep Research

### Understand GitKB

Deeply explore the GitKB codebase and use `git kb` diligently to understand its concepts, capabilities, and future vision:

- `git kb context` — load project context documents
- `git kb list --path "spec/*"` — find specs and design documents
- `git kb show <slug>` — read specific documents for vision, architecture, roadmap
- `git kb code symbols --file <path>` — explore code structure in `~/development/gitkb`
- `git kb code callers <symbol>` / `git kb code callees <symbol>` — understand call graphs
- `git kb search <query>` — search for relevant concepts

Spend real time here. Understand not just what GitKB does today, but where it's headed.

### Research {{competitor}}

- Search the web for {{competitor}} — docs, blog posts, pricing, architecture
- Clone their code repository into `/tmp/research/` if publicly available
- Read their README, docs, and source code to understand their approach
- Identify their target users, positioning, and technical architecture

## Phase 2: SWOT Analysis

Perform a structured SWOT analysis across these dimensions for **both** products:

- **Strengths**: What does each product do well? Technical advantages? Unique capabilities?
- **Weaknesses**: Where does each fall short? Technical debt? Missing features? UX gaps?
- **Opportunities**: Market gaps, integration possibilities, underserved use cases, emerging trends?
- **Threats**: Competitive pressure, technology shifts, adoption barriers, market risks?

## Phase 3: Document Findings

Create a KB document with your findings:

```
git kb create --type spec --slug swot/{{name}} --title "SWOT: {{competitor}} vs GitKB"
```

Include concrete evidence throughout — code examples, feature comparisons, architecture differences, and direct quotes from documentation. Cite URLs for all external sources.

You can: read/explore KB documents, read/explore code, create KB documents, and search the web.
