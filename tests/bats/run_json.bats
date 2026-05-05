#!/usr/bin/env bats
# End-to-end tests for `atc run --json`.
#
# Pipeline-level unit tests in pipeline.rs lock the envelope shape; these
# tests verify that the binary actually emits the envelope on stdout (with
# tracing logs going to stderr), and that error paths produce a structured
# envelope rather than an anyhow stack trace.

load helpers/common

# All --json tests pipe through `jq -e` so a malformed envelope fails the
# test loudly instead of silently passing on substring matches.
require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        skip "jq not installed"
    fi
}

# Run a command and split stdout/stderr so we can assert on the envelope
# without tracing noise. BATS `run` merges them by default. We must NOT
# return a non-zero status from this function — bats' implicit errexit on the
# test body would abort the test before our exit-code assertion runs. Stash
# the rc in $SPLIT_STATUS for the caller to read.
run_split() {
    local stdout_file="$TEST_TMPDIR/.stdout"
    local stderr_file="$TEST_TMPDIR/.stderr"
    "$@" >"$stdout_file" 2>"$stderr_file" && SPLIT_STATUS=0 || SPLIT_STATUS=$?
    STDOUT="$(cat "$stdout_file")"
    STDERR="$(cat "$stderr_file")"
}

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
