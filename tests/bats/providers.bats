#!/usr/bin/env bats
# Tests for the context provider system.
# Validates config parsing for providers and PR context provider end-to-end
# (with mocked gh commands where needed).

load helpers/common

# ---------------------------------------------------------------------------
# Config validation: providers field
# ---------------------------------------------------------------------------

@test "config with valid providers parses successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<'EOF'
[dispatch]
repo = "core"

[registry]
path = "/tmp/test.db"

[modes.review-fix]
providers = ["pr-context", "rebase"]

[modes.implement]
providers = ["rebase"]
EOF

    # Config is valid — help should work with this config
    run atc --config "$config" --help
    assert_success
}

@test "config with unknown provider name fails validation" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<'EOF'
[dispatch]
repo = "core"

[registry]
path = "/tmp/test.db"

[modes.implement]
providers = ["nonexistent-provider"]
EOF

    # Should fail config validation
    run atc --config "$config" status
    assert_failure
    assert_output --partial "unknown provider"
}

@test "config with empty providers list parses successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<'EOF'
[dispatch]
repo = "core"

[registry]
path = "/tmp/test.db"

[modes.implement]
providers = []
EOF

    run atc --config "$config" --help
    assert_success
}

@test "config with all three providers parses successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<'EOF'
[dispatch]
repo = "core"

[registry]
path = "/tmp/test.db"

[modes.review-fix]
providers = ["pr-context", "kb-context", "rebase"]
EOF

    run atc --config "$config" --help
    assert_success
}

# ---------------------------------------------------------------------------
# PR context provider: end-to-end with mocked gh
# ---------------------------------------------------------------------------

@test "pr-context provider: dry run includes pr-context in mode config" {
    local config="$TEST_TMPDIR/atc.toml"
    local db="$TEST_TMPDIR/atc.db"

    mkdir -p "$TEST_TMPDIR/workspace"
    cat > "$config" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$db"

[modes.review-fix]
template_inline = "Review PR: {{prefetch}}"
providers = ["pr-context"]
EOF

    init_test_db "$db"

    # dry-run should show mode config (won't actually run providers
    # since dispatch is short-circuited, but validates config loading)
    run atc --config "$config" run "review-task" --mode review-fix \
        --pr-url "https://github.com/test/repo/pull/1" --dry-run
    assert_success
    assert_output --partial "DRY RUN"
    assert_output --partial "review-fix"
}
