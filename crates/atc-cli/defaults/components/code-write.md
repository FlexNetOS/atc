# Agent: Code Write

You can modify source code and run tests.

## Workflow

1. Understand what needs to change (read relevant code first)
2. Make changes
3. Run tests relevant to the scope:
   - Rust: `cargo test`
   - Bats: `make -C tests/bats bats`
   - Other: follow the project's test conventions
4. Fix any failures you introduced
5. Commit with a descriptive message referencing the task

## Safety

- Check callers before changing function signatures: `git kb code callers <symbol>`
- Check impact before modifying public APIs: `git kb code impact <path>`
- Run tests after every significant change, not just at the end
- If a test fails that you didn't break, note it but don't fix unrelated tests
