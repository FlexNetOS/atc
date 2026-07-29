---
name: verify
description: Build the atc CLI and drive it at its real surface to verify a change. Use when verifying atc changes rather than running the test suite.
---

# Verifying atc

atc is a CLI. The surface is a terminal: build the binary, run it in a
throwaway directory, read what it prints and what it writes to disk.

## Build

```bash
cd /home/flexnetos/meta/src/atc
rtk proxy -- cargo build --bin atc
ATC=/run/user/1001/yazelix/volatile/cargo-target/debug/atc
```

`rtk proxy --` is required — a bare `cargo` is denied by the hook. The
shared target dir lives on a 50G tmpfs; a full workspace build can fill
it, and once it is full the harness loses command output entirely with
ENOSPC. Clear `/run/user/1001/yazelix/volatile/cargo-target/debug` if
that happens — it is a regenerable cache.

## Drive

Always work in a fresh temp dir. `atc init` writes into `./.atc`, so
running it in the repo scribbles on the source tree.

```bash
D=$(mktemp -d); cd $D
$ATC init                    # scaffolds .atc/{components,partials,skills}
find .atc/skills -type f     # nested Codex skills must appear
```

The scaffold must produce both flat and nested skills:

```
.atc/skills/dispatch.md                    # flat, legacy
.atc/skills/dispatch/SKILL.md              # nested Codex form
.atc/skills/dispatch/agents/openai.yaml    # two levels deep
```

The two-level path is the one that regresses. Anything that touches
`DEFAULT_SKILLS` or the writers in `init/{scaffold,agents,picker}.rs`
must create parent directories; when that breaks, ~16 tests fail with
`NotFound` on `dispatch/SKILL.md`.

Agent wiring is a positional argument, not a flag, and the parent dir
must already exist:

```bash
mkdir -p .claude
$ATC init claude --copy      # mirror; symlink is the default
$ATC init --list-agents      # registry + wire status
find .claude -type f
```

Copy mode drops a `.atc-skills-managed` marker — `is_atc_skills_copy`
keys off that file, not off filenames.

## Probes worth running

- Re-run `init claude --copy`: file count must not change.
- Add a file under the target dir, re-mirror: user files must survive.
- Delete a skill from `.atc/skills/`, re-mirror: the now-empty target
  dirs must be cleaned (`remove_empty_parents`), user files untouched.
- Pass an agent name containing `\033[2J` and U+202E: the error must
  print `\x1b[2J\u{202e}` escaped, with zero literal ESC bytes
  (`od -c | grep 033` → 0). Unknown agent exits 1.

## Gotchas

- `--agent` does not exist; it is `atc init <AGENT>`.
- `crates/atc-cli/src/init.rs` is retired. The module is the
  `init/` directory. If both ever exist, rustc fails with E0761.
- `base64` and sqlx's `postgres` feature live in the root
  `[workspace.dependencies]`; a crate adding `base64.workspace = true`
  without the root entry makes the whole workspace fail to parse.
- `tmux::tests::session_alive_uses_only_has_session_probe` is a flake
  from shared tmux server state — it fails roughly two runs in five
  regardless of the change, passes alone and with `--test-threads=1`.
  Do not attribute it to a diff.
