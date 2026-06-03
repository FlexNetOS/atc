# Git Hooks

Optional local hooks for development.

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

### commit-msg

Validates the first line of the commit message with the same Conventional
Commits shape required by the PR-title CI check. This keeps local commits and
squash-merge titles parseable by Release Please.

Examples:

```text
feat: add resumable runs
feat!: change dispatch metadata format
fix(cli): preserve json output envelope
chore(main): release 0.1.6
```

To bypass local hooks in an emergency: `git commit --no-verify`. The required
PR-title check still protects squash merges.

### pre-push

Runs format check, clippy, and tests before push. This is intended to mirror CI quality gates.

To bypass (not recommended): `git push --no-verify`

## CI Auto-Format

If you forget to format locally, the CI will auto-format and push a commit to your PR branch. This only works for PRs from the same repo (not forks).
