# Cloud ATC P1 — measurements & surprises

The spike's success criterion is **one observed run, end to end**, plus the
recorded measurements below. The control-plane and worker code is implemented
and unit-tested; the live run requires provisioned infrastructure and
credentials (Fly, NATS, Neon, GitHub App on `gitkb`) that are **not available
to the implementing agent**. This file is the template to fill on the first
live run, plus the integration friction already surfaced while building it.

## Measurements (fill on first live run)

| Metric | How to capture | Result |
|---|---|---|
| **Time-to-ready** | `entrypoint.sh` logs `time-to-ready: <n>s` (volume fork + fetch + worktree, before claude starts) | _TBD_ |
| **First-build I/O: bare-mirror fork vs raw clone** | Compare `time-to-ready` with a warm forked `atc_mirror` volume vs the cold-clone fallback path (Decision-5 benchmark) | _TBD_ |
| **Total run cost** | `atc status` / Postgres `cost_usd` after `atc post-complete` | _TBD_ |
| **Integration friction** | Notes below | see below |

## Integration friction already surfaced (build time)

- **Re-materializer hosting.** `RemoteExecutor::spawn` starts the `nats sub`
  re-materializer detached, but `atc run` (non-inline) returns immediately —
  the same fire-and-forget contract as a tmux dispatch. The consumer therefore
  needs a long-lived host (the daemon) to fully drain the stream. The clean
  follow-on is daemon-hosted consumers (or a foreground `atc cloud consume`).
- **Core NATS is fire-and-forget.** Subscribing before `fly machine run` shrinks
  the early-event gap but does not close it. JetStream (durable, replayable) is
  the right substrate for at-least-once event delivery — a follow-on.
- **Prompt/stdin transport.** The system prompt + task doc are passed as
  base64 env vars (`ATC_SYSTEM_PROMPT_B64` / `ATC_STDIN_B64`). Fly Machine env
  has size limits; very large prompts will need a side channel (NATS request,
  or staged on the volume). Recorded as a known constraint to measure.
- **Machine-id parsing.** `RemoteExecutor::parse_machine_id` scrapes the 14-char
  hex id from `fly machine run` text output. If/when `fly` stabilizes a JSON
  output for `machine run`, switch to it (less brittle).
- **Health Signals 2–6 vs cloud.** Only Signal 1 was made cloud-aware (heartbeat
  TTL on the log mtime). Signals 2–6 still probe a local worktree/git and will
  no-op for a cloud dispatch; the PR URL is recovered from the re-materialized
  stream by post-completion (`add_pr_url`), not by Signal 3. Scoped per the task.
- **`set_artifacts` implemented (minor deviation).** The task listed
  `set_artifacts` among the bail-defaulted methods, but `post_completion` calls
  it with `?`; leaving it as `bail!` would abort the run *after* the metrics row
  is written. `PgRegistry` implements it (one `UPDATE`) so the happy path
  completes cleanly. Noted for review.

## What a passing run looks like

1. `atc run task tasks/<slug>` → `RemoteExecutor` creates a Machine; the record
   is inserted into Postgres with a `TerminalLocator::Cloud` (worker id).
2. `atc logs <id> --follow` shows the re-materialized stream-json as the worker
   streams it over NATS.
3. The agent pushes a branch and opens a **real PR** on the private repo.
4. The Machine self-destructs; the log goes idle; health Signal 1 (cloud) flips
   to "exited" after the TTL.
5. `atc post-complete --id <id>` extracts the result event + PR URL and writes
   one row: `cost_usd`, `num_turns`, `duration_ms`, `pr_url`, `status`.
6. `atc status` shows that one row. ✅
