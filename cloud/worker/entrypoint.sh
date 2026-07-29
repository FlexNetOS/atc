#!/usr/bin/env bash
#
# ATC Cloud worker entrypoint — Cloud ATC vertical slice ([[tasks/harmony-844]]).
#
# Runs on an ephemeral Fly Machine. It forks a warm bare-mirror volume, mints a
# short-lived GitHub App installation token, creates a worktree, runs `claude`
# headless with `--output-format stream-json`, tees every event line to the
# per-dispatch NATS subject (the control plane re-materializes that stream into
# the durable `log_file`), and finally self-destructs the Machine + volume fork.
#
# The agent itself pushes the branch and opens the PR as part of its directive,
# exactly as a local ATC dispatch does — the PR URL is extracted from the
# re-materialized stream-json log by post-completion on the control plane.
#
# Inputs are passed as Machine env (`fly machine run -e ...`) by RemoteExecutor.
# Secrets (GitHub App key, NATS creds, FLY_API_TOKEN) come from Fly app secrets.
set -euo pipefail

log() { printf '[atc-worker] %s\n' "$*" >&2; }

# --- Required inputs (set by RemoteExecutor::worker_env) ---
: "${ATC_DISPATCH_ID:?missing ATC_DISPATCH_ID}"
: "${ATC_SLUG:?missing ATC_SLUG}"
: "${ATC_DIRECTIVE:?missing ATC_DIRECTIVE}"
: "${ATC_NATS_URL:?missing ATC_NATS_URL}"
: "${ATC_NATS_SUBJECT:?missing ATC_NATS_SUBJECT}"
: "${ATC_REPO_REMOTE:?missing ATC_REPO_REMOTE}"
: "${ATC_BRANCH:?missing ATC_BRANCH}"
ATC_MAX_TURNS="${ATC_MAX_TURNS:-10000}"
ATC_MAX_BUDGET_USD="${ATC_MAX_BUDGET_USD:-25}"

WORKSPACE="${ATC_WORKSPACE:-/workspace}"   # mount of the forked warm volume
MIRROR="$WORKSPACE/repo.git"               # bare mirror of the target repo
WORKTREE="$WORKSPACE/worktree"

READY_START=$(date +%s)

# --- 1. GitHub App auth: short-lived installation token, no PAT in the image ---
log "minting GitHub App installation token"
GH_TOKEN="$(/usr/local/bin/gh-app-token.sh)"
export GH_TOKEN GITHUB_TOKEN="$GH_TOKEN"
# $GH_TOKEN must stay unexpanded here so git evaluates it when it runs the helper.
# shellcheck disable=SC2016
git config --global credential.helper \
  '!f() { echo "username=x-access-token"; echo "password=$GH_TOKEN"; }; f'
git config --global user.email "${ATC_GIT_EMAIL:-atc-bot@gitkb.dev}"
git config --global user.name "${ATC_GIT_NAME:-ATC Cloud Worker}"

# --- 2. Warm bare mirror -> fetch -> worktree ---
if [ ! -d "$MIRROR" ]; then
  # Cold path (volume was empty). The warm volume is expected to already hold a
  # bare mirror; cloning here is the fallback measured for Decision-5.
  log "bare mirror missing; cold clone (cold-cache path)"
  git clone --bare "$ATC_REPO_REMOTE" "$MIRROR"
fi
git -C "$MIRROR" remote set-url origin "$ATC_REPO_REMOTE"
log "fetching latest refs into bare mirror"
git -C "$MIRROR" fetch --prune origin '+refs/heads/*:refs/heads/*'
DEFAULT_BRANCH="$(git -C "$MIRROR" symbolic-ref --short HEAD 2>/dev/null || echo main)"

rm -rf "$WORKTREE"
log "creating worktree on branch $ATC_BRANCH from origin/$DEFAULT_BRANCH"
git -C "$MIRROR" worktree add -B "$ATC_BRANCH" "$WORKTREE" "origin/$DEFAULT_BRANCH" 2>/dev/null \
  || git -C "$MIRROR" worktree add -B "$ATC_BRANCH" "$WORKTREE" "$DEFAULT_BRANCH"

READY_END=$(date +%s)
log "time-to-ready: $((READY_END - READY_START))s"

# --- 3. Decode the prompt + task stdin passed via env (base64, newline-safe) ---
printf '%s' "${ATC_SYSTEM_PROMPT_B64:-}" | base64 -d > /tmp/system_prompt.md
printf '%s' "${ATC_STDIN_B64:-}"         | base64 -d > /tmp/stdin.txt

# The user prompt mirrors ClaudeExecutor::build_user_prompt: stdin carries the
# task document; the system prompt carries the directive instructions.
USER_PROMPT="Directive: ${ATC_DIRECTIVE}
Task: ${ATC_SLUG}
Working directory: ${WORKTREE}

The task document follows on stdin — it IS your plan. Follow the system prompt instructions exactly."

# --- 4. Run claude headless; tee each stream-json line to NATS ---
# Flags mirror ClaudeExecutor::spawn_inline so the cloud run is format-identical
# to a local dispatch (the control-plane re-materializer writes these exact
# lines to the durable log_file).
log "running claude (stream-json) -> NATS subject $ATC_NATS_SUBJECT"
cd "$WORKTREE"
set +e
claude -p "$USER_PROMPT" \
  --append-system-prompt-file /tmp/system_prompt.md \
  --dangerously-skip-permissions \
  --output-format stream-json --verbose \
  --max-turns "$ATC_MAX_TURNS" --max-budget-usd "$ATC_MAX_BUDGET_USD" \
  < /tmp/stdin.txt \
  | while IFS= read -r line; do
      printf '%s\n' "$line"                                            # Machine console log
      # Publish synchronously (no `&`): backgrounding each line races the writes
      # and can reorder or drop events, corrupting the stream-json sequence the
      # control plane re-materializes. Abort the stream if a publish fails so the
      # truncation surfaces rather than silently passing.
      if ! printf '%s' "$line" | nats pub --server "$ATC_NATS_URL" "$ATC_NATS_SUBJECT" --; then
        log "NATS publish failed; aborting stream"
        exit 86
      fi
    done
CLAUDE_RC=${PIPESTATUS[0]}
NATS_RC=${PIPESTATUS[1]:-0}
set -e
# A publish failure is fatal even when claude itself finished cleanly — a partial
# stream on the control plane is an incomplete dispatch.
if [ "$CLAUDE_RC" -eq 0 ] && [ "$NATS_RC" -ne 0 ]; then
  CLAUDE_RC="$NATS_RC"
fi
log "claude exited rc=$CLAUDE_RC (nats rc=$NATS_RC)"

# --- 5. Self-destruct: tear down this Machine + its forked volume ---
exec /usr/local/bin/self-destruct.sh "$CLAUDE_RC"
