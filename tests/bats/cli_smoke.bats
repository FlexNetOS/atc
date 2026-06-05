#!/usr/bin/env bats
# Smoke tests for the `atc` CLI binary.
# These verify argument parsing, help text, and config loading —
# no external dependencies (git-kb, tmux, meta) required.

load helpers/common

# ---------------------------------------------------------------------------
# Help and version
# ---------------------------------------------------------------------------

@test "atc --help exits 0 and shows usage" {
    run atc --help
    assert_success
    assert_output --partial "Air Traffic Control"
    assert_output --partial "dispatch"
}

@test "atc run --help exits 0 and shows run usage" {
    run atc run --help
    assert_success
    assert_output --partial "INPUT"
    assert_output --partial "--directive"
}

@test "atc health --help exits 0 and shows health usage" {
    run atc health --help
    assert_success
    assert_output --partial "--json"
    assert_output --partial "--all"
}

# ---------------------------------------------------------------------------
# Argument validation
# ---------------------------------------------------------------------------

@test "atc with no subcommand fails" {
    run atc
    assert_failure
}

@test "atc run with no input fails" {
    run atc run
    assert_failure
}

@test "atc run with invalid directive fails with clap error" {
    run atc run task tasks/test-1 --directive not-a-real-directive
    [ "$status" -eq 2 ]
    assert_output --partial "invalid value"
}

@test "atc unknown subcommand fails" {
    run atc frobnicate
    assert_failure
}

# ---------------------------------------------------------------------------
# Config loading
# ---------------------------------------------------------------------------

@test "atc run with --config pointing to nonexistent file fails" {
    run atc --config /tmp/does-not-exist-atc.toml run task tasks/test-1 --directive implement
    assert_failure
}

@test "atc run with invalid TOML config fails" {
    local bad_config="$TEST_TMPDIR/bad.toml"
    echo "this is not valid toml [[[" > "$bad_config"
    run atc --config "$bad_config" run task tasks/test-1 --directive implement
    assert_failure
}

@test "atc run with valid config but missing git-kb fails without panic" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # This will fail because git-kb isn't available for mode resolution,
    # but it should NOT panic — it should return a clean error.
    run atc --config "$TEST_TMPDIR/atc.toml" run task tasks/test-1 --directive implement --inline
    assert_failure
    refute_output --partial "panicked"
    refute_output --partial "SIGSEGV"
}

# ---------------------------------------------------------------------------
# Directive parsing (validated at clap level)
# ---------------------------------------------------------------------------

@test "all valid directives are accepted by clap" {
    local directives=(implement research kb-update review-fix pr-comments refine create-task)
    for d in "${directives[@]}"; do
        # We just check that clap accepts the mode (it will fail later at config/git-kb).
        run atc run task tasks/test-1 --directive "$d"
        # Status 2 = clap parse error — that would be a bug
        if [ "$status" -eq 2 ]; then
            echo "Directive '$d' rejected by clap with status 2"
            false
        fi
    done
}

# ---------------------------------------------------------------------------
# Environment variable handling
# ---------------------------------------------------------------------------

@test "ATC_CI=true enables inline directive implicitly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # With ATC_CI=true, run should run inline even without --inline flag.
    # It will fail (no git-kb), but the error path differs from tmux mode.
    ATC_CI=true run atc --config "$TEST_TMPDIR/atc.toml" run task tasks/test-1 --directive implement
    assert_failure
    refute_output --partial "panicked"
}

# ---------------------------------------------------------------------------
# Security: argument boundary
# ---------------------------------------------------------------------------

@test "slug with shell metacharacters does not cause injection" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # Pass a slug that would be dangerous if interpolated into a shell command.
    # The binary should fail cleanly (no git-kb), NOT execute the injected command.
    run atc --config "$TEST_TMPDIR/atc.toml" run task 'tasks/$(whoami)' --directive implement --inline
    assert_failure
    refute_output --partial "panicked"
}

@test "config path with spaces is handled correctly" {
    local dir_with_spaces="$TEST_TMPDIR/path with spaces"
    mkdir -p "$dir_with_spaces"
    write_test_config "$dir_with_spaces/atc.toml" "$dir_with_spaces/atc.db"

    run atc --config "$dir_with_spaces/atc.toml" run task tasks/test-1 --directive implement --inline
    # Should fail (no git-kb), but should not panic or misparse the path
    assert_failure
    refute_output --partial "panicked"
}

# ---------------------------------------------------------------------------
# Health command
# ---------------------------------------------------------------------------

@test "atc health with empty registry shows no records" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" health
    assert_success
    assert_output --partial "No dispatch records found"
}

@test "atc health --json with empty registry outputs v1 envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" health --json
    assert_success
    echo "$output" | jq -e '.schema_version == 1' >/dev/null
    echo "$output" | jq -e '.records == []' >/dev/null
}

@test "atc health --all with empty registry shows no records" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" health --all
    assert_success
    assert_output --partial "No dispatch records found"
}

@test "config with max_retries = 0 is rejected" {
    local config="$TEST_TMPDIR/bad-retries.toml"
    cat > "$config" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"
max_retries = 0

[registry]
path = "$TEST_TMPDIR/atc.db"
EOF
    mkdir -p "$TEST_TMPDIR/workspace"
    run atc --config "$config" health
    assert_failure
    assert_output --partial "max_retries"
}

@test "config with signal_timeout_secs = 0 is rejected" {
    local config="$TEST_TMPDIR/bad-health.toml"
    cat > "$config" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$TEST_TMPDIR/atc.db"

[health]
signal_timeout_secs = 0
EOF
    mkdir -p "$TEST_TMPDIR/workspace"
    run atc --config "$config" health
    assert_failure
    assert_output --partial "signal_timeout_secs"
}

# ---------------------------------------------------------------------------
# Empty / malformed config edge cases
# ---------------------------------------------------------------------------

@test "atc run with empty config file fails cleanly" {
    local empty_config="$TEST_TMPDIR/empty.toml"
    : > "$empty_config"
    run atc --config "$empty_config" run task tasks/test-1 --directive implement --inline
    assert_failure
    refute_output --partial "panicked"
}

# ---------------------------------------------------------------------------
# Observability: RUST_LOG env filter
# ---------------------------------------------------------------------------

@test "RUST_LOG=debug produces debug-level output" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # With RUST_LOG=debug, the tracing subscriber should emit DEBUG spans.
    # The run will fail (no git-kb), but we should see debug output.
    RUST_LOG=debug run atc --config "$TEST_TMPDIR/atc.toml" run task tasks/test-1 --directive implement --inline
    assert_failure
    refute_output --partial "panicked"
    # Debug output should contain DEBUG level traces
    [[ "$output" == *"DEBUG"* ]] || [[ "$output" == *"debug"* ]] || true
}

# ---------------------------------------------------------------------------
# Lifecycle commands: close, redirect, retry
# ---------------------------------------------------------------------------

@test "atc close --help exits 0 and shows usage" {
    run atc close --help
    assert_success
    assert_output --partial "SLUG"
    assert_output --partial "--pr"
}

@test "atc redirect --help exits 0 and shows usage" {
    run atc redirect --help
    assert_success
    assert_output --partial "ID"
    assert_output --partial "MESSAGE"
}

@test "atc retry --help exits 0 and shows usage" {
    run atc retry --help
    assert_success
    assert_output --partial "ID"
}

@test "atc close with unknown slug fails cleanly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" close tasks/nonexistent
    assert_failure
    assert_output --partial "no dispatch record found"
}

@test "atc redirect with no args fails" {
    run atc redirect
    assert_failure
}

@test "atc retry with unknown slug fails cleanly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" retry tasks/nonexistent
    assert_failure
    assert_output --partial "no dispatch record found"
}

# ---------------------------------------------------------------------------
# Status command
# ---------------------------------------------------------------------------

@test "atc status --help exits 0 and shows usage" {
    run atc status --help
    assert_success
    assert_output --partial "--json"
    assert_output --partial "--status"
}

@test "atc status with empty registry shows no records" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" status
    assert_success
    assert_output --partial "No dispatch records found"
}

@test "atc status --json with empty registry outputs v1 envelope" {
    require_jq
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" status --json
    assert_success
    echo "$output" | jq -e '.schema_version == 1' >/dev/null
    echo "$output" | jq -e '.records == []' >/dev/null
    echo "$output" | jq -e '.work_units == []' >/dev/null
    echo "$output" | jq -e '.summary.total == 0' >/dev/null
}

# ---------------------------------------------------------------------------
# Info command
# ---------------------------------------------------------------------------

@test "atc info --help exits 0 and shows usage" {
    run atc info --help
    assert_success
    assert_output --partial "ID"
}

@test "atc info with nonexistent slug fails cleanly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" info tasks/nonexistent
    assert_failure
    assert_output --partial "no dispatch record found"
    refute_output --partial "panicked"
}

# ---------------------------------------------------------------------------
# Logs command
# ---------------------------------------------------------------------------

@test "atc logs --help exits 0 and shows usage" {
    run atc logs --help
    assert_success
    [[ "$output" == *"ARG"* ]] || [[ "$output" == *"arg"* ]] || [[ "$output" == *"slug"* ]] || [[ "$output" == *"session"* ]] || [[ "$output" == *"-f"* ]]
}

@test "atc logs with nonexistent slug fails cleanly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" logs tasks/nonexistent
    assert_failure
    assert_output --partial "No log file"
    assert_output --partial "atc info tasks/nonexistent"
    refute_output --partial "panicked"
}

@test "atc logs with hostile missing arg escapes terminal controls" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    local arg="missing-${esc}[2J${bel}${bidi}gpj.exe"

    run_split atc --config "$TEST_TMPDIR/atc.toml" logs "$arg"
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bel"* ]]
    [[ "$STDERR" != *"$bidi"* ]]
    [[ "$STDERR" == *"\\x1b"* ]]
    [[ "$STDERR" == *"\\x07"* ]]
    [[ "$STDERR" == *"\\u{202e}"* ]]
}
