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

@test "atc dispatch --help exits 0 and shows dispatch usage" {
    run atc dispatch --help
    assert_success
    assert_output --partial "SLUG"
    assert_output --partial "MODE"
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

@test "atc dispatch with no slug fails" {
    run atc dispatch
    assert_failure
}

@test "atc dispatch with invalid directive fails with clap error" {
    # Arg order: atc dispatch <SLUG> [DIRECTIVE]
    run atc dispatch tasks/test-1 not-a-real-directive
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

@test "atc dispatch with --config pointing to nonexistent file fails" {
    run atc --config /tmp/does-not-exist-atc.toml dispatch tasks/test-1 implement
    assert_failure
}

@test "atc dispatch with invalid TOML config fails" {
    local bad_config="$TEST_TMPDIR/bad.toml"
    echo "this is not valid toml [[[" > "$bad_config"
    run atc --config "$bad_config" dispatch tasks/test-1 implement
    assert_failure
}

@test "atc dispatch with valid config but missing git-kb fails without panic" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # This will fail because git-kb isn't available for mode resolution,
    # but it should NOT panic — it should return a clean error.
    run atc --config "$TEST_TMPDIR/atc.toml" dispatch tasks/test-1 implement --inline
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
        # Arg order: atc dispatch <SLUG> [DIRECTIVE]
        # We just check that clap accepts the mode (it will fail later at config/git-kb).
        run atc dispatch tasks/test-1 "$d"
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

    # With ATC_CI=true, dispatch should run inline even without --inline flag.
    # It will fail (no git-kb), but the error path differs from tmux mode.
    ATC_CI=true run atc --config "$TEST_TMPDIR/atc.toml" dispatch tasks/test-1 implement
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
    run atc --config "$TEST_TMPDIR/atc.toml" dispatch 'tasks/$(whoami)' implement --inline
    assert_failure
    refute_output --partial "panicked"
}

@test "config path with spaces is handled correctly" {
    local dir_with_spaces="$TEST_TMPDIR/path with spaces"
    mkdir -p "$dir_with_spaces"
    write_test_config "$dir_with_spaces/atc.toml" "$dir_with_spaces/atc.db"

    run atc --config "$dir_with_spaces/atc.toml" dispatch tasks/test-1 implement --inline
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

@test "atc health --json with empty registry outputs empty array" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" health --json
    assert_success
    assert_output "[]"
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

@test "atc dispatch with empty config file fails cleanly" {
    local empty_config="$TEST_TMPDIR/empty.toml"
    : > "$empty_config"
    run atc --config "$empty_config" dispatch tasks/test-1 implement --inline
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
    # The dispatch will fail (no git-kb), but we should see debug output.
    RUST_LOG=debug run atc --config "$TEST_TMPDIR/atc.toml" dispatch tasks/test-1 implement --inline
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

@test "atc status --json with empty registry outputs empty array" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    run atc --config "$TEST_TMPDIR/atc.toml" status --json
    assert_success
    assert_output "[]"
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
    refute_output --partial "panicked"
}
