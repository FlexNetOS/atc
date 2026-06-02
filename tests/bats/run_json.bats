#!/usr/bin/env bats
# End-to-end tests for `atc run --json`.
#
# Pipeline-level unit tests in pipeline.rs lock the envelope shape; these
# tests verify that the binary actually emits the envelope on stdout (with
# tracing logs going to stderr), and that error paths produce a structured
# envelope rather than an anyhow stack trace.

load helpers/common

# ---------------------------------------------------------------------------
# Help text documents the schema
# ---------------------------------------------------------------------------

@test "atc run --help documents the v1 JSON schema" {
    run atc run --help
    assert_success
    assert_output --partial "JSON OUTPUT"
    assert_output --partial "schema_version"
    assert_output --partial "\"kind\": \"dispatch\" | \"error\""
    assert_output --partial "is_dry_run"
    assert_output --partial "Future fields are additive"
    assert_output --partial "RESUME / RETRY / REDIRECT"
    assert_output --partial "--resume <dispatch-id|task-slug>"
}

# ---------------------------------------------------------------------------
# Dry-run success envelope
# ---------------------------------------------------------------------------

@test "atc run --json --dry-run emits dispatch envelope on stdout" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement --dry-run --json
    [ "$SPLIT_STATUS" -eq 0 ]

    # Envelope shape — schema_version + kind=dispatch + data object
    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data | type == "object"' >/dev/null

    # Required fields populated
    echo "$STDOUT" | jq -e '.data.dispatch_id | type == "string"' >/dev/null
    echo "$STDOUT" | jq -e '.data.directive == "implement"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == true' >/dev/null
    echo "$STDOUT" | jq -e '.data.status == "preview"' >/dev/null
    echo "$STDOUT" | jq -e '.data.log_file == null' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_provider == "claude"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id == null' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_capabilities.supports_resume_by_session_id == true' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_capabilities.supports_explicit_session_id_on_start == true' >/dev/null
    echo "$STDOUT" | jq -e '.data | has("agent_capabilities_json") | not' >/dev/null
    echo "$STDOUT" | jq -e '.data.dispatched_at | type == "string"' >/dev/null
}

@test "atc run --json --dry-run combined with --no-worktree still emits dispatch envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement \
        --dry-run --json --no-worktree
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    # --no-worktree maps to WorktreePolicy::Current at the pipeline layer
    echo "$STDOUT" | jq -e '.data.worktree_policy == "current"' >/dev/null
}

@test "atc run --json --dry-run combined with --inline still emits dispatch envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement \
        --dry-run --json --inline
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == true' >/dev/null
}

@test "atc run --json --inline emits durable agent session metadata" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace" "$TEST_TMPDIR/bin"

    cat > "$TEST_TMPDIR/bin/claude" <<'SH'
#!/bin/sh
printf '%s\n' "$@" > "$ATC_ARG_CAPTURE"
cat >/dev/null
printf '%s\n' '{"type":"result","subtype":"success","total_cost_usd":0.01,"num_turns":1,"duration_ms":10}'
exit 0
SH
    chmod +x "$TEST_TMPDIR/bin/claude"
    export PATH="$TEST_TMPDIR/bin:$PATH"
    export ATC_ARG_CAPTURE="$TEST_TMPDIR/claude.args"

    cd "$TEST_TMPDIR/workspace"
    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement --inline --no-worktree --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == false' >/dev/null
    echo "$STDOUT" | jq -e '.data.status == "done"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_provider == "claude"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id | test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id != .data.session' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_transcript_cwd | type == "string"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_capabilities.supports_resume_by_session_id == true' >/dev/null
    echo "$STDOUT" | jq -e '.data | has("agent_capabilities_json") | not' >/dev/null
    [ "$(grep -c -- '^--session-id$' "$ATC_ARG_CAPTURE")" -eq 1 ]
    grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' "$ATC_ARG_CAPTURE"
}

@test "atc run --json --resume continues source provider session" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace" "$TEST_TMPDIR/other" "$TEST_TMPDIR/bin"

    cat > "$TEST_TMPDIR/bin/claude" <<'SH'
#!/bin/sh
printf '%s\n' "$@" > "$ATC_ARG_CAPTURE"
pwd > "$ATC_CWD_CAPTURE"
cat >/dev/null
printf '%s\n' '{"type":"result","subtype":"success","total_cost_usd":0.01,"num_turns":1,"duration_ms":10}'
exit 0
SH
    chmod +x "$TEST_TMPDIR/bin/claude"
    export PATH="$TEST_TMPDIR/bin:$PATH"
    export ATC_ARG_CAPTURE="$TEST_TMPDIR/claude.args"
    export ATC_CWD_CAPTURE="$TEST_TMPDIR/claude.cwd"

    cd "$TEST_TMPDIR/workspace"
    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement --inline --no-worktree --json
    [ "$SPLIT_STATUS" -eq 0 ]

    source_id="$(echo "$STDOUT" | jq -r '.data.dispatch_id')"
    source_session="$(echo "$STDOUT" | jq -r '.data.agent_session_id')"
    source_cwd="$(echo "$STDOUT" | jq -r '.data.agent_transcript_cwd')"
    [ -n "$source_id" ]
    [ -n "$source_session" ]
    [ -d "$source_cwd" ]
    [ "$(grep -c -- '^--session-id$' "$ATC_ARG_CAPTURE")" -eq 1 ]
    if grep -q -- '^--resume$' "$ATC_ARG_CAPTURE"; then
        fail "fresh dispatch unexpectedly passed --resume"
    fi

    cd "$TEST_TMPDIR/other"
    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Follow up on the previous work" --directive implement \
        --resume "$source_id" --inline --no-worktree --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.status == "done"' >/dev/null
    echo "$STDOUT" | jq -e --arg source_id "$source_id" '.data.resume_of_dispatch_id == $source_id' >/dev/null
    echo "$STDOUT" | jq -e --arg source_session "$source_session" '.data.agent_session_id == $source_session' >/dev/null
    echo "$STDOUT" | jq -e --arg source_cwd "$source_cwd" '.data.agent_transcript_cwd == $source_cwd' >/dev/null
    echo "$STDOUT" | jq -e --arg source_cwd "$source_cwd" '.data.worktree_path == $source_cwd' >/dev/null
    resumed_id="$(echo "$STDOUT" | jq -r '.data.dispatch_id')"

    [ "$(grep -c -- '^--resume$' "$ATC_ARG_CAPTURE")" -eq 1 ]
    grep -Fx "$source_session" "$ATC_ARG_CAPTURE" >/dev/null
    if grep -q -- '^--session-id$' "$ATC_ARG_CAPTURE"; then
        fail "resume dispatch also passed --session-id"
    fi
    [ "$(cat "$ATC_CWD_CAPTURE")" = "$source_cwd" ]

    run_split atc --config "$TEST_TMPDIR/atc.toml" info "$resumed_id" --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e --arg source_id "$source_id" '.record.resume_of_dispatch_id == $source_id' >/dev/null
    echo "$STDOUT" | jq -e --arg source_session "$source_session" '.record.agent_session_id == $source_session' >/dev/null
    echo "$STDOUT" | jq -e --arg source_cwd "$source_cwd" '.record.agent_transcript_cwd == $source_cwd' >/dev/null
}

@test "atc run --json --resume task slug --dry-run emits source session metadata" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace" "$TEST_TMPDIR/worktree"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-resume" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_session_id = '00000000-0000-4000-8000-000000000701',
    worktree_path = '$TEST_TMPDIR/workspace',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true}'
WHERE id = 'disp-resume';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Preview resume" --directive implement --resume tasks/test-1 --dry-run --json
    [ "$SPLIT_STATUS" -eq 0 ]

    expected_cwd="$(cd "$TEST_TMPDIR/workspace" && pwd -P)"
    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == true' >/dev/null
    echo "$STDOUT" | jq -e '.data.status == "preview"' >/dev/null
    echo "$STDOUT" | jq -e '.data.resume_of_dispatch_id == "disp-resume"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id == "00000000-0000-4000-8000-000000000701"' >/dev/null
    echo "$STDOUT" | jq -e --arg cwd "$expected_cwd" '.data.agent_transcript_cwd == $cwd' >/dev/null
    echo "$STDOUT" | jq -e --arg cwd "$expected_cwd" '.data.worktree_path == $cwd' >/dev/null
    echo "$STDOUT" | jq -e '.data.log_file == null' >/dev/null
}

@test "atc run --json --resume task slug launches provider session" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace" "$TEST_TMPDIR/bin"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-resume-spawn" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_session_id = '00000000-0000-4000-8000-000000000705',
    worktree_path = '$TEST_TMPDIR/workspace',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true}'
WHERE id = 'disp-resume-spawn';
SQL

    cat > "$TEST_TMPDIR/bin/claude" <<'SH'
#!/bin/sh
printf '%s\n' "$@" > "$ATC_ARG_CAPTURE"
pwd > "$ATC_CWD_CAPTURE"
cat >/dev/null
printf '%s\n' '{"type":"result","subtype":"success","total_cost_usd":0.01,"num_turns":1,"duration_ms":10}'
exit 0
SH
    chmod +x "$TEST_TMPDIR/bin/claude"
    export PATH="$TEST_TMPDIR/bin:$PATH"
    export ATC_ARG_CAPTURE="$TEST_TMPDIR/claude.args"
    export ATC_CWD_CAPTURE="$TEST_TMPDIR/claude.cwd"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Follow up via task slug" --directive implement \
        --resume tasks/test-1 --inline --no-worktree --json
    [ "$SPLIT_STATUS" -eq 0 ]

    expected_cwd="$(cd "$TEST_TMPDIR/workspace" && pwd -P)"
    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.status == "done"' >/dev/null
    echo "$STDOUT" | jq -e '.data.resume_of_dispatch_id == "disp-resume-spawn"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id == "00000000-0000-4000-8000-000000000705"' >/dev/null
    echo "$STDOUT" | jq -e --arg cwd "$expected_cwd" '.data.worktree_path == $cwd' >/dev/null
    [ "$(grep -c -- '^--resume$' "$ATC_ARG_CAPTURE")" -eq 1 ]
    grep -Fx "00000000-0000-4000-8000-000000000705" "$ATC_ARG_CAPTURE" >/dev/null
    if grep -q -- '^--session-id$' "$ATC_ARG_CAPTURE"; then
        fail "task-slug resume dispatch also passed --session-id"
    fi
    [ "$(cat "$ATC_CWD_CAPTURE")" = "$expected_cwd" ]
}

@test "atc run --json --resume dry-run has no registry or log side effects" {
    require_jq
    cat > "$TEST_TMPDIR/atc.toml" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"
log_dir = "$TEST_TMPDIR/logs"

[registry]
path = "$TEST_TMPDIR/atc.db"
EOF
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-resume-preview" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_session_id = '00000000-0000-4000-8000-000000000706',
    worktree_path = '$TEST_TMPDIR/workspace',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true}'
WHERE id = 'disp-resume-preview';
SQL

    before_count="$(sqlite3 "$TEST_TMPDIR/atc.db" 'SELECT COUNT(*) FROM dispatches;')"
    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Preview only" --directive implement \
        --resume disp-resume-preview --dry-run --json
    [ "$SPLIT_STATUS" -eq 0 ]
    after_count="$(sqlite3 "$TEST_TMPDIR/atc.db" 'SELECT COUNT(*) FROM dispatches;')"

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == true' >/dev/null
    echo "$STDOUT" | jq -e '.data.resume_of_dispatch_id == "disp-resume-preview"' >/dev/null
    [ "$after_count" = "$before_count" ]
    [ ! -e "$TEST_TMPDIR/logs" ]
}

@test "atc run --json --resume active source emits conflict unless forced" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-active-resume" "tasks/test-1" "running"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_session_id = '00000000-0000-4000-8000-000000000707',
    worktree_path = '$TEST_TMPDIR/workspace',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true}'
WHERE id = 'disp-active-resume';
SQL

    before_count="$(sqlite3 "$TEST_TMPDIR/atc.db" 'SELECT COUNT(*) FROM dispatches;')"
    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Preview active resume" --directive implement \
        --resume disp-active-resume --dry-run --json
    [ "$SPLIT_STATUS" -ne 0 ]
    after_reject_count="$(sqlite3 "$TEST_TMPDIR/atc.db" 'SELECT COUNT(*) FROM dispatches;')"

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("already active")' >/dev/null
    [ "$after_reject_count" = "$before_count" ]

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Preview active resume" --directive implement \
        --resume disp-active-resume --force --dry-run --json
    [ "$SPLIT_STATUS" -eq 0 ]
    after_force_count="$(sqlite3 "$TEST_TMPDIR/atc.db" 'SELECT COUNT(*) FROM dispatches;')"

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.is_dry_run == true' >/dev/null
    echo "$STDOUT" | jq -e '.data.resume_of_dispatch_id == "disp-active-resume"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id == "00000000-0000-4000-8000-000000000707"' >/dev/null
    [ "$after_force_count" = "$before_count" ]
}

# ---------------------------------------------------------------------------
# Error envelope
# ---------------------------------------------------------------------------

@test "atc run --json with no input emits error envelope and exits non-zero" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"

    run_split atc --config "$TEST_TMPDIR/atc.toml" run --json
    [ "$SPLIT_STATUS" -ne 0 ]

    # Structured envelope on stdout, not a panic or anyhow trace
    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.code == "dispatch_error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("input is required")' >/dev/null
}

@test "atc run --json --directive review-fix without PR URL emits error envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Address review feedback" --directive review-fix --json --dry-run
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("requires a PR URL")' >/dev/null
}

@test "atc run --json --resume with missing source session emits error envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace" "$TEST_TMPDIR/worktree"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-missing-session" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_session_id = NULL,
    agent_transcript_cwd = '$TEST_TMPDIR/worktree',
    agent_capabilities_json = '{"supports_resume_by_session_id":true}'
WHERE id = 'disp-missing-session';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Resume without session" --directive implement \
        --resume disp-missing-session --dry-run --json
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("missing agent_session_id")' >/dev/null
}

@test "atc run --json --resume with unsupported provider emits error envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-unsupported-provider" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_provider = 'codex',
    worktree_path = '$TEST_TMPDIR/workspace',
    agent_session_id = '00000000-0000-4000-8000-000000000703',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":true}'
WHERE id = 'disp-unsupported-provider';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Resume unsupported provider" --directive implement \
        --resume disp-unsupported-provider --dry-run --json
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e ".data.message | test(\"provider 'codex' is not supported\")" >/dev/null
}

@test "atc run --json --resume without resume capability emits error envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-unsupported-capability" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET worktree_path = '$TEST_TMPDIR/workspace',
    agent_session_id = '00000000-0000-4000-8000-000000000704',
    agent_transcript_cwd = '$TEST_TMPDIR/workspace',
    agent_capabilities_json = '{"supports_resume_by_session_id":false}'
WHERE id = 'disp-unsupported-capability';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Resume unsupported capability" --directive implement \
        --resume disp-unsupported-capability --dry-run --json
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("does not support resume by session id")' >/dev/null
}

@test "atc run --json --resume rejects unsafe transcript cwd" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    init_test_db "$TEST_TMPDIR/atc.db"
    mkdir -p "$TEST_TMPDIR/workspace"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-unsafe-cwd" "tasks/test-1" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET worktree_path = '/',
    agent_session_id = '00000000-0000-4000-8000-000000000702',
    agent_transcript_cwd = '/',
    agent_capabilities_json = '{"supports_resume_by_session_id":true}'
WHERE id = 'disp-unsafe-cwd';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Resume from unsafe cwd" --directive implement \
        --resume disp-unsafe-cwd --dry-run --json
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("unsafe transcript cwd")' >/dev/null
}

@test "atc run --json --resume with --ephemeral is rejected" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run_split atc --config "$TEST_TMPDIR/atc.toml" \
        run "Do a tiny thing" --directive implement \
        --resume disp-001 --ephemeral --inline --json
    [ "$SPLIT_STATUS" -ne 0 ]

    echo "$STDOUT" | jq -e '.kind == "error"' >/dev/null
    echo "$STDOUT" | jq -e '.data.message | test("--resume is not supported with --ephemeral")' >/dev/null
}

# ---------------------------------------------------------------------------
# Non-JSON path is unchanged (regression guard)
# ---------------------------------------------------------------------------

@test "atc run --dry-run without --json still emits human-readable preview" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    run atc --config "$TEST_TMPDIR/atc.toml" \
        run "Fix the auth bug" --directive implement --dry-run
    assert_success
    assert_output --partial "=== DRY RUN ==="
    assert_output --partial "Directive:"
    refute_output --partial "\"schema_version\""
    refute_output --partial "\"kind\""
}

# ---------------------------------------------------------------------------
# --list with --json — covered by the additive-flag contract
# ---------------------------------------------------------------------------

@test "atc run --list --json emits a templates envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"

    run_split atc --config "$TEST_TMPDIR/atc.toml" run --list --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.kind == "templates"' >/dev/null
    echo "$STDOUT" | jq -e '.data.templates | type == "array"' >/dev/null
}
