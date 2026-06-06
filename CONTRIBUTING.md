# Contributing to atc

## CI for external contributors

When you open a PR from a fork, CI runs automatically on GitHub-hosted ubuntu-latest
runners. This is free for you — public repository PRs don't consume your GitHub Actions
minutes against your personal account.

You'll see the full check suite: format check, clippy, build, and tests. If any check
fails, the failure annotations will appear on your PR. Run `cargo fmt` locally before
pushing to avoid the format check round-trip.

## CI for maintainers

Internal PRs (from branches in the FlexNetOS/atc repo) run on our self-hosted Mac Studio
runners. This is the same environment used for `main` builds and release tags, so
"passes CI" means "passes on the same hardware that builds the release."
