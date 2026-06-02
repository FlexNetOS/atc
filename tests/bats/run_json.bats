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
    echo "$STDOUT" | jq -e '.data.agent_capabilities == null' >/dev/null
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
