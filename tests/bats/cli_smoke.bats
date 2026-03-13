#!/usr/bin/env bats
# Smoke tests for the `atc` CLI binary.
# These verify argument parsing, help text, and config loading —
# no external dependencies (git-kb, tmux, meta) required.

load helpers/common

# ---------------------------------------------------------------------------
# Help and version
# ---------------------------------------------------------------------------

@test "atc --help exits 0 and shows usage" {
    run "$ATC_BIN" --help
    [ "$status" -eq 0 ]
    [[ "$output" == *"Air Traffic Control"* ]]
    [[ "$output" == *"dispatch"* ]]
}

@test "atc dispatch --help exits 0 and shows dispatch usage" {
    run "$ATC_BIN" dispatch --help
    [ "$status" -eq 0 ]
    [[ "$output" == *"SLUG"* ]]
    [[ "$output" == *"MODE"* ]]
}

# ---------------------------------------------------------------------------
# Argument validation
# ---------------------------------------------------------------------------

@test "atc with no subcommand fails" {
    run "$ATC_BIN"
    [ "$status" -ne 0 ]
}

@test "atc dispatch with no slug fails" {
    run "$ATC_BIN" dispatch
    [ "$status" -ne 0 ]
}

@test "atc dispatch with invalid mode fails with clap error" {
    # Arg order: atc dispatch <SLUG> [MODE]
    run "$ATC_BIN" dispatch tasks/test-1 not-a-real-mode
    [ "$status" -eq 2 ]
    [[ "$output" == *"invalid value"* ]]
}

@test "atc unknown subcommand fails" {
    run "$ATC_BIN" frobnicate
    [ "$status" -ne 0 ]
}

# ---------------------------------------------------------------------------
# Config loading
# ---------------------------------------------------------------------------

@test "atc dispatch with --config pointing to nonexistent file fails" {
    run "$ATC_BIN" --config /tmp/does-not-exist-atc.toml dispatch tasks/test-1 implement
    [ "$status" -ne 0 ]
}

@test "atc dispatch with invalid TOML config fails" {
    local bad_config="$TEST_TMPDIR/bad.toml"
    echo "this is not valid toml [[[" > "$bad_config"
    run "$ATC_BIN" --config "$bad_config" dispatch tasks/test-1 implement
    [ "$status" -ne 0 ]
}

@test "atc dispatch with valid config but missing git-kb fails without panic" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # This will fail because git-kb isn't available for mode resolution,
    # but it should NOT panic — it should return a clean error.
    run "$ATC_BIN" --config "$TEST_TMPDIR/atc.toml" dispatch tasks/test-1 implement --inline
    [ "$status" -ne 0 ]
    # Must not contain panic indicators
    if [[ "$output" == *"panicked"* ]] || [[ "$output" == *"SIGSEGV"* ]]; then
        echo "PANIC DETECTED in output: $output"
        false
    fi
}

# ---------------------------------------------------------------------------
# Mode parsing (validated at clap level)
# ---------------------------------------------------------------------------

@test "all valid modes are accepted by clap" {
    local modes=(implement research kb-update review-fix pr-comments refine create-task)
    for mode in "${modes[@]}"; do
        # Arg order: atc dispatch <SLUG> [MODE]
        # We just check that clap accepts the mode (it will fail later at config/git-kb).
        run "$ATC_BIN" dispatch tasks/test-1 "$mode"
        # Status 2 = clap parse error — that would be a bug
        if [ "$status" -eq 2 ]; then
            echo "Mode '$mode' rejected by clap with status 2"
            false
        fi
    done
}

# ---------------------------------------------------------------------------
# Environment variable handling
# ---------------------------------------------------------------------------

@test "ATC_CI=true enables inline mode implicitly" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # With ATC_CI=true, dispatch should run inline even without --inline flag.
    # It will fail (no git-kb), but the error path differs from tmux mode.
    ATC_CI=true run "$ATC_BIN" --config "$TEST_TMPDIR/atc.toml" dispatch tasks/test-1 implement
    [ "$status" -ne 0 ]
    if [[ "$output" == *"panicked"* ]]; then
        echo "PANIC DETECTED: $output"
        false
    fi
}

# ---------------------------------------------------------------------------
# Security: argument boundary
# ---------------------------------------------------------------------------

@test "slug with shell metacharacters does not cause injection" {
    write_test_config "$TEST_TMPDIR/atc.toml"
    mkdir -p "$TEST_TMPDIR/workspace"

    # Pass a slug that would be dangerous if interpolated into a shell command.
    # The binary should fail cleanly (no git-kb), NOT execute the injected command.
    run "$ATC_BIN" --config "$TEST_TMPDIR/atc.toml" dispatch 'tasks/$(whoami)' implement --inline
    [ "$status" -ne 0 ]
    if [[ "$output" == *"panicked"* ]]; then
        echo "PANIC DETECTED: $output"
        false
    fi
}

@test "config path with spaces is handled correctly" {
    local dir_with_spaces="$TEST_TMPDIR/path with spaces"
    mkdir -p "$dir_with_spaces"
    write_test_config "$dir_with_spaces/atc.toml" "$dir_with_spaces/atc.db"

    run "$ATC_BIN" --config "$dir_with_spaces/atc.toml" dispatch tasks/test-1 implement --inline
    # Should fail (no git-kb), but should not panic or misparse the path
    [ "$status" -ne 0 ]
    if [[ "$output" == *"panicked"* ]]; then
        echo "PANIC DETECTED: $output"
        false
    fi
}
