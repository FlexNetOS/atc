# Cloud ATC — vertical slice (P1)

The thinnest end-to-end Cloud ATC path: a task dispatched to an **ephemeral Fly
Machine worker** forks a warm bare-mirror volume, runs `claude` headless,
streams `stream-json` back over **NATS**, opens a **real PR**, writes one
metrics row to a **Postgres** registry, and the Machine **self-destructs**.

This is the spike for `tasks/harmony-844` (P1 of `specs/cloud-atc`). It proves
the architecture's central claim — **remote = a sibling trait impl, not a
pipeline refactor** — and seeds the net-new critical-path code.

## How it fits the existing ATC code

```
                          control plane (atc)                         worker (Fly Machine)
  atc run  ──► run_cloud ──► DispatchPipeline ──► RemoteExecutor::spawn ─┐
                                  │                       │              │  entrypoint.sh:
                                  │                       │              │   • gh-app-token.sh  (GitHub App)
                                  │                       │              │   • fork volume → worktree
                                  │                       │              │   • claude --output-format stream-json
                                  │                       │              │        │ tee per line
                                  │                       ▼              │        ▼
                                  │            nats sub ──◄──────────────┼──── nats pub  atc.dispatch.<id>.events
                                  │                  │ (re-materialize)  │
                                  │                  ▼                   │
                                  │            durable log_file ◄────────┘  (byte-identical to a local dispatch)
                                  ▼                  │
                          PgRegistry.insert          ▼
                                          stream_json → post_completion
                                                       • update_cost  (cost/turns/duration row)
                                                       • add_pr_url    (PR extracted from the stream)
                          health Signal 1 (cloud) ── heartbeat TTL on the log_file mtime
```

The only net-new control-plane code is **two trait impls** selected at `main`:

| Seam | Local (today) | Cloud (this slice) |
|---|---|---|
| `AgentExecutor` | `ClaudeExecutor` (tmux) | `RemoteExecutor` (Fly + NATS) |
| `Registry` | `SqliteRegistry` | `PgRegistry` |
| `TerminalLocator` | `Tmux` | `Cloud` (worker/Machine id) |
| health Signal 1 | `tmux has-session` | log-mtime heartbeat TTL |

`stream_json`, `post_completion`, `atc logs`/`atc watch`, and the dispatch
pipeline are **unchanged** — the NATS consumer re-materializes the JSONL to the
same `log_file` a local tmux dispatch would have written.

## Config

Enable the cloud path in `.atc/config.toml`:

```toml
[cloud]
enabled = true
fly_app = "atc-workers"
fly_image = "registry.fly.io/atc-workers:latest"
fly_region = "iad"
worker_volume = "atc_mirror"
nats_url = "nats://<host>:4222"           # or set NATS_URL
nats_subject_prefix = "atc.dispatch"
database_url = "postgres://…/atc"          # or set DATABASE_URL
repo_remote = "https://github.com/gitkb/atc.git"
liveness_ttl_secs = 120
```

All secret-bearing values fall back to env vars (`NATS_URL`, `DATABASE_URL`).

## Provisioning (one-time, needs credentials)

1. **Neon Postgres** — create a database; set `DATABASE_URL`. `PgRegistry::connect`
   creates the `dispatches` table on first run.
2. **NATS** — a server reachable by both the control plane and the worker. They
   **must share a NATS account** (per-app scoping caveat in `specs/cloud-atc`).
3. **GitHub App** on the `gitkb` org with `contents:write` + `pull_requests:write`,
   installed on the target repo. Capture `GH_APP_ID`, `GH_APP_INSTALLATION_ID`,
   `GH_APP_PRIVATE_KEY`.
4. **Fly** — `fly apps create atc-workers`; create the warm volume and seed it
   with a bare mirror:
   ```bash
   fly volumes create atc_mirror -a atc-workers -r iad -s 10
   # seed the mirror once (bare clone of the target repo into /workspace/repo.git)
   ```
   Build/push the image: `fly deploy --build-only --push -a atc-workers` (image
   only — there is no long-running service).
5. **Worker secrets** (never in the image):
   ```bash
   fly secrets set -a atc-workers \
     GH_APP_ID=… GH_APP_INSTALLATION_ID=… GH_APP_PRIVATE_KEY="$(cat key.pem)" \
     NATS_URL=… FLY_API_TOKEN=… FLY_APP_NAME=atc-workers ANTHROPIC_API_KEY=…
   ```

## Running the one observed dispatch

```bash
# control plane (with [cloud] enabled and a long-lived NATS consumer host)
atc run task tasks/<slug>
atc logs <dispatch-id> --follow      # reads the re-materialized stream-json
# after the worker self-destructs and the log drains:
atc post-complete --id <dispatch-id> # writes cost/turns/duration + PR url to Postgres
atc status                           # the one metrics row
```

> **Consumer hosting:** `RemoteExecutor::spawn` starts the NATS re-materializer
> detached. For the log to fully materialize it must run under a long-lived
> control-plane process (the daemon, a follow-on). For the hand-run spike, host
> the consumer in a process that outlives `atc run` (e.g. keep an `atc watch`
> open, or run the consumer foreground). See `MEASUREMENTS.md`.

## Files

| File | Role |
|---|---|
| `worker/Dockerfile` | Worker image (git, claude, nats, jq/curl/openssl) |
| `worker/entrypoint.sh` | Fork volume → worktree → claude stream-json → NATS → self-destruct |
| `worker/gh-app-token.sh` | Mint a short-lived GitHub App installation token |
| `worker/self-destruct.sh` | Destroy the Machine + forked volume via the Fly API |
| `worker/fly.toml` | Fly app/volume config for the worker |
| `MEASUREMENTS.md` | Spike measurements + open questions |

## Out of scope (follow-on tasks)

Full meta multi-repo worktree; Postgres `agent_jobs` queue + cloud scheduler;
full `PgRegistry`/`AgentSession` shape; `git kb agent heartbeat` for Signal 1;
`atc-eval` outcome verdict. See the task doc.
