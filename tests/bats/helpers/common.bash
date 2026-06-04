#!/usr/bin/env bash
# Common helpers for ATC BATS tests.
#
# BATS_TEST_DIRNAME = directory of the .bats file = tests/bats/
# Repo root is two levels up from there.

# Load BATS libraries (relative to the .bats file, i.e. tests/bats/)
load "${BATS_TEST_DIRNAME}/bats/bats-support/load"
load "${BATS_TEST_DIRNAME}/bats/bats-assert/load"
load "${BATS_TEST_DIRNAME}/bats/bats-file/load"

_repo_root() {
    cd "$BATS_TEST_DIRNAME/../.." && pwd
}

# ---------------------------------------------------------------------------
# atc() wrapper — like gitkb's git_kb() wrapper for debug output
# ---------------------------------------------------------------------------
atc() {
    [[ -n "${DEBUG:-}" ]] && echo "   atc $*" >&3
    "$ATC_BIN" "$@"
}

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        skip "jq not installed"
    fi
}

# Run a command with stdout/stderr split into globals so JSON tests can parse
# stdout without tracing or warning lines.
run_split() {
    local stdout_file="$TEST_TMPDIR/.stdout"
    local stderr_file="$TEST_TMPDIR/.stderr"
    "$@" >"$stdout_file" 2>"$stderr_file" && SPLIT_STATUS=0 || SPLIT_STATUS=$?
    STDOUT="$(cat "$stdout_file")"
    STDERR="$(cat "$stderr_file")"
}

# ---------------------------------------------------------------------------
# Build the atc binary once per test file (not per test).
# ---------------------------------------------------------------------------
setup_file() {
    local root
    root="$(_repo_root)"
    export ATC_BIN="$root/target/debug/atc"
    cargo build --manifest-path "$root/Cargo.toml" --quiet
}

# ---------------------------------------------------------------------------
# Per-test setup: create isolated temp directory with proper env isolation.
# ---------------------------------------------------------------------------
setup() {
    setup_test_dir
}

# ---------------------------------------------------------------------------
# Per-test teardown: clean up temp directory.
# ---------------------------------------------------------------------------
teardown() {
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# setup_test_dir — isolated temp dir with env var cleanup
# ---------------------------------------------------------------------------
setup_test_dir() {
    TEST_TMPDIR="$(mktemp -d -t atc-bats-XXXXXX)"
    export TEST_TMPDIR
    export ATC_BIN="${ATC_BIN:-$(_repo_root)/target/debug/atc}"

    # Unset any inherited env vars that could leak into tests
    unset ATC_CONFIG ATC_ROOT ATC_CI ATC_NOTIFY_WEBHOOK RUST_LOG
}

# ---------------------------------------------------------------------------
# teardown_test_dir — clean up temp directory
# ---------------------------------------------------------------------------
teardown_test_dir() {
    if [[ -d "${TEST_TMPDIR:-}" ]]; then
        rm -rf "$TEST_TMPDIR"
    fi
}

# ---------------------------------------------------------------------------
# write_test_config — write a minimal valid ATC config file
# ---------------------------------------------------------------------------
write_test_config() {
    local config_file="$1"
    local db_path="${2:-$TEST_TMPDIR/atc.db}"
    cat > "$config_file" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$db_path"
EOF
}

# ---------------------------------------------------------------------------
# init_test_db — create the SQLite registry with the dispatches schema
# ---------------------------------------------------------------------------
init_test_db() {
    local db_path="${1:-$TEST_TMPDIR/atc.db}"
    sqlite3 "$db_path" <<'SCHEMA'
CREATE TABLE IF NOT EXISTS dispatches (
  id                        TEXT PRIMARY KEY,
  task_slug                 TEXT,
  branch                    TEXT NOT NULL,
  worktree_path             TEXT NOT NULL,
  session                   TEXT NOT NULL,
  log_file                  TEXT NOT NULL,
  status                    TEXT NOT NULL DEFAULT 'running',
  directive                 TEXT NOT NULL,
  retries                   INTEGER NOT NULL DEFAULT 0,
  resolver                  TEXT NOT NULL,
  pr_url                    TEXT,
  pr_urls                   TEXT NOT NULL DEFAULT '[]',
  no_worktree               INTEGER NOT NULL DEFAULT 0,
  original_input            TEXT,
  kb_root                   TEXT,
  check_agent_exited_clean  INTEGER NOT NULL DEFAULT 0,
  check_branch_pushed       INTEGER NOT NULL DEFAULT 0,
  check_pr_created          INTEGER NOT NULL DEFAULT 0,
  check_ci_passed           INTEGER NOT NULL DEFAULT 0,
  check_reviews_approved    INTEGER NOT NULL DEFAULT 0,
  check_threads_resolved    INTEGER NOT NULL DEFAULT 0,
  cost_usd                  REAL,
  num_turns                 INTEGER,
  duration_ms               INTEGER,
  artifacts                 TEXT,
  work_unit_id              TEXT,
  agent_provider            TEXT NOT NULL DEFAULT 'claude',
  agent_session_id          TEXT,
  agent_transcript_cwd      TEXT,
  resume_of_dispatch_id     TEXT,
  agent_capabilities_json   TEXT,
  dispatched_at             TEXT NOT NULL,
  updated_at                TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dispatches_status ON dispatches(status);
CREATE INDEX IF NOT EXISTS idx_dispatches_task_slug ON dispatches(task_slug);
CREATE INDEX IF NOT EXISTS idx_dispatches_branch ON dispatches(branch);
CREATE INDEX IF NOT EXISTS idx_dispatches_worktree ON dispatches(worktree_path);
CREATE INDEX IF NOT EXISTS idx_dispatches_pr_url ON dispatches(pr_url);
CREATE INDEX IF NOT EXISTS idx_dispatches_work_unit ON dispatches(work_unit_id);
CREATE INDEX IF NOT EXISTS idx_dispatches_updated_at ON dispatches(updated_at);
CREATE TABLE IF NOT EXISTS work_units (
  id          TEXT PRIMARY KEY,
  task_slug   TEXT,
  branch      TEXT,
  repos       TEXT NOT NULL DEFAULT '[]',
  pr_urls     TEXT NOT NULL DEFAULT '[]',
  status      TEXT NOT NULL DEFAULT 'active',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_work_units_task ON work_units(task_slug);
CREATE INDEX IF NOT EXISTS idx_work_units_branch ON work_units(branch);
CREATE INDEX IF NOT EXISTS idx_work_units_status ON work_units(status);
SCHEMA
}

# ---------------------------------------------------------------------------
# insert_test_dispatch — insert a dispatch record directly into the registry
# ---------------------------------------------------------------------------
insert_test_dispatch() {
    local db="$1" id="$2" task_slug="$3" status="${4:-running}" mode="${5:-implement}" retries="${6:-0}"
    # Use RFC 3339 timestamps — atc's registry deserializes with parse_from_rfc3339
    local now
    now="$(date -u +%Y-%m-%dT%H:%M:%S+00:00)"
    sqlite3 "$db" <<SQL
INSERT INTO dispatches (id, task_slug, branch, worktree_path, session, log_file, status, directive, retries, resolver, dispatched_at, updated_at)
VALUES ('${id//\'/\'\'}', '${task_slug//\'/\'\'}', 'test-branch', '${TEST_TMPDIR//\'/\'\'}/worktree', '${id//\'/\'\'}', '${TEST_TMPDIR//\'/\'\'}/${id//\'/\'\'}.jsonl', '${status//\'/\'\'}', '${mode//\'/\'\'}', ${retries}, 'task', '$now', '$now');
SQL
}

# ---------------------------------------------------------------------------
# insert_test_work_unit — insert a work-unit record directly into the registry
# ---------------------------------------------------------------------------
insert_test_work_unit() {
    local db="$1" id="$2" task_slug="$3" branch="${4:-test-branch}" status="${5:-active}"
    local now
    now="$(date -u +%Y-%m-%dT%H:%M:%S+00:00)"
    sqlite3 "$db" <<SQL
INSERT INTO work_units (id, task_slug, branch, repos, pr_urls, status, created_at, updated_at)
VALUES ('${id//\'/\'\'}', '${task_slug//\'/\'\'}', '${branch//\'/\'\'}', '["open-source/atc"]', '[]', '${status//\'/\'\'}', '$now', '$now');
SQL
}

# ---------------------------------------------------------------------------
# write_test_log — write a canned stream-json log file
# ---------------------------------------------------------------------------
write_test_log() {
    local log_file="$1" subtype="${2:-success}" cost="${3:-2.50}"
    cat > "$log_file" <<EOF
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on the task..."}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Created PR https://github.com/org/repo/pull/42"}]}}
{"type":"result","subtype":"$subtype","total_cost_usd":$cost,"num_turns":15,"duration_ms":45000}
EOF
}

# ---------------------------------------------------------------------------
# setup_lifecycle — convenience: config + db + workspace dir
# ---------------------------------------------------------------------------
setup_lifecycle() {
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
}

# ---------------------------------------------------------------------------
# setup_test_git_worktree — create a real git repo at $TEST_TMPDIR/worktree
# with branch "test-branch" pushed to a bare origin, so health-checker
# signal 2 (git ls-remote) succeeds.
# ---------------------------------------------------------------------------
setup_test_git_worktree() {
    # Create a bare "origin" repo
    git init --bare "$TEST_TMPDIR/origin.git" --quiet
    # Clone it as the worktree
    git clone "$TEST_TMPDIR/origin.git" "$TEST_TMPDIR/worktree" --quiet
    # Create a commit and push the test branch
    (
        cd "$TEST_TMPDIR/worktree" || return 1
        git config user.email "test@example.com"
        git config user.name "ATC Test"
        git checkout -b test-branch --quiet
        echo "test" > file.txt
        git add file.txt
        git commit -m "init" --quiet
        git push origin test-branch --quiet
    ) || return 1
}

# ---------------------------------------------------------------------------
# query_dispatch_field — read a single field from the dispatches table
# ---------------------------------------------------------------------------
query_dispatch_field() {
    local db="$1" id="$2" field="$3"
    # NOTE: Assumes trusted input — $field is a column name, $id is a dispatch UUID
    sqlite3 "$db" "SELECT $field FROM dispatches WHERE id = '${id//\'/\'\'}';"
}
