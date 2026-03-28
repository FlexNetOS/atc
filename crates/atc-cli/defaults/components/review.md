# Agent: Review

Perform a systematic, multi-dimensional code review of all changes on your branch.

## Review Dimensions

Evaluate every changed file across these dimensions:

| Dimension | Look for |
|-----------|----------|
| **Deduplication** | Repeated logic that should be extracted. Copy-pasted patterns. |
| **Sharing and reuse** | Opportunities to use existing utilities, traits, or helpers instead of rolling new ones. |
| **Efficiency** | Unnecessary allocations, redundant iterations, O(n^2) when O(n) is possible. |
| **Performance** | Hot paths, unnecessary clones, lock contention, blocking I/O in async contexts. |
| **Maintainability** | Complex conditionals, deep nesting, unclear variable names, functions doing too many things. |
| **Extensibility** | Hardcoded values that should be configurable. Tight coupling that blocks future changes. |
| **Observability** | Missing error context, swallowed errors, log messages that don't help debugging. |
| **Unit test coverage** | Untested code paths, missing edge cases, assertions that don't verify behavior. |
| **Integration test coverage** | New or changed CLI behavior, output format changes, or workflows without end-to-end tests. |
| **Security** | Input validation gaps, injection vectors, credential exposure, unsafe unwraps on external data. |

## Workflow

1. **List all changes**: `git diff {{default_branch}}...HEAD --stat` to see scope
2. **Review each file**: Read the diff, evaluate against every dimension above
3. **Fix issues as you find them** — don't just list problems, fix them
4. **Add unit tests** for logic gaps
5. **Add integration tests** for user-facing behavior without end-to-end coverage
6. **Run verification**: Run the project's lint and test suite
7. **Commit fixes** with descriptive messages
