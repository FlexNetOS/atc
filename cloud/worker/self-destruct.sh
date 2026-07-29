#!/usr/bin/env bash
#
# Self-destruct the worker Machine and its forked volume ([[tasks/harmony-844]]).
#
# Called at the end of entrypoint.sh. Uses the Fly Machines API with the
# Machine's own id (from $FLY_MACHINE_ID, injected by Fly) and a scoped
# FLY_API_TOKEN (Fly app secret). Destroying the Machine releases the forked
# volume created at `fly machine run --volume`.
#
# Arg $1: the claude exit code (logged; the Machine is torn down regardless).
set -euo pipefail

CLAUDE_RC="${1:-0}"
log() { printf '[atc-worker] %s\n' "$*" >&2; }

: "${FLY_API_TOKEN:?missing FLY_API_TOKEN}"
: "${FLY_APP_NAME:?missing FLY_APP_NAME}"
: "${FLY_MACHINE_ID:?missing FLY_MACHINE_ID}"
API="${FLY_API_HOSTNAME:-https://api.machines.dev}"

# Give the control-plane NATS consumer a moment to drain the final result line
# before the Machine disappears.
sleep "${ATC_DRAIN_SECONDS:-3}"

log "self-destruct: destroying Machine $FLY_MACHINE_ID (claude rc=$CLAUDE_RC)"
curl -fsS -X DELETE \
  -H "Authorization: Bearer ${FLY_API_TOKEN}" \
  "${API}/v1/apps/${FLY_APP_NAME}/machines/${FLY_MACHINE_ID}?force=true" \
  || log "self-destruct API call failed; Machine will stop on exit (--restart no)"

# If the DELETE is async or fails, exiting still stops the Machine because it was
# created with `--restart no`.
exit "$CLAUDE_RC"
