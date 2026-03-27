Review ALL branch changes across every dimension in the review component (deduplication, reuse, efficiency, performance, maintainability, extensibility, observability, test coverage, security).

1. **List all changes**: `git diff {{default_branch}}...HEAD --stat` to see scope
2. **Review each file**: Read the diff, evaluate against every dimension
3. **Fix issues as you find them** — don't just list problems, fix them
4. **Add unit tests** for logic gaps
5. **Add integration tests** for user-facing behavior without end-to-end coverage
6. **Run verification**: Run the project's lint and test suite
7. **Commit fixes** with descriptive messages
