# Git Hooks

Optional pre-commit hooks for local development.

## Setup

Enable hooks for this repo:

```bash
git config core.hooksPath .githooks
```

Or use the Makefile:

```bash
make install-hooks
```

## Available Hooks

### pre-commit

Runs `cargo fmt --check` before commit. Blocks commit if formatting is wrong.

To fix: run `cargo fmt --all` and re-stage files.

To bypass (not recommended): `git commit --no-verify`

### pre-push

Runs format check, clippy, and tests before push. Matches CI checks exactly.

To bypass (not recommended): `git push --no-verify`

## CI Auto-Format

If you forget to format locally, the CI will auto-format and push a commit to your PR branch. This only works for PRs from the same repo (not forks).
