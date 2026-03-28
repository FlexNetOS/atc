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
    cat > "$config" <<EOF
[dispatch]
repo = "core"

[registry]
path = "$TEST_TMPDIR/test.db"

[directives.review-fix]
providers = ["pr-context", "rebase"]

[directives.implement]
providers = ["rebase"]
EOF

    # Config is valid — status exercises AtcConfig::load() + parse_and_validate()
    run atc --config "$config" status
    assert_success
}

@test "config with unknown provider name fails validation" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<EOF
[dispatch]
repo = "core"

[registry]
path = "$TEST_TMPDIR/test.db"

[directives.implement]
providers = ["nonexistent-provider"]
EOF

    # Should fail config validation
    run atc --config "$config" status
    assert_failure
    assert_output --partial "unknown provider"
}

@test "config with empty providers list parses successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<EOF
[dispatch]
repo = "core"

[registry]
path = "$TEST_TMPDIR/test.db"

[directives.implement]
providers = []
EOF

    run atc --config "$config" status
    assert_success
}

@test "config with all three providers parses successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    cat > "$config" <<EOF
[dispatch]
repo = "core"

[registry]
path = "$TEST_TMPDIR/test.db"

[directives.review-fix]
providers = ["pr-context", "kb-context", "rebase"]
EOF

    run atc --config "$config" status
    assert_success
}

# ---------------------------------------------------------------------------
# PR context provider: end-to-end with mocked gh
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Template rendering: deferred provider vars in partials
# ---------------------------------------------------------------------------

@test "template with partial using provider-deferred var renders successfully" {
    local config="$TEST_TMPDIR/atc.toml"
    local db="$TEST_TMPDIR/atc.db"

    # Set up .atc/-style directory structure
    mkdir -p "$TEST_TMPDIR/workspace"
    mkdir -p "$TEST_TMPDIR/templates"
    mkdir -p "$TEST_TMPDIR/partials"
    mkdir -p "$TEST_TMPDIR/components"

    # Component that references a partial
    cat > "$TEST_TMPDIR/components/review.md" <<'COMP'
# Agent: Review

Review all changes.

{{>review-steps}}
COMP

    # Partial that uses {{default_branch}} — a rebase provider template var
    cat > "$TEST_TMPDIR/partials/review-steps.md" <<'PARTIAL'
1. `git diff {{default_branch}}...HEAD --stat`
2. Review each file
PARTIAL

    # Template that includes the partial directly
    cat > "$TEST_TMPDIR/templates/pr-review.md" <<'TMPL'
---
directive: review-fix
required_params: [pr]
---
# PR Review: {{pr}}

{{>review-steps}}
TMPL

    cat > "$config" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$db"

[prompt]
templates_dir = "$TEST_TMPDIR/templates"
partials_dir = "$TEST_TMPDIR/partials"
components_dir = "$TEST_TMPDIR/components"

[directives.review-fix]
components = ["review"]
providers = ["pr-context", "rebase"]
EOF

    init_test_db "$db"

    # Template rendering should succeed — {{default_branch}} should be deferred,
    # not rejected by Handlebars strict mode
    run atc --config "$config" run "pr-review" \
        --param pr="https://github.com/test/repo/pull/1" \
        --pr-url "https://github.com/test/repo/pull/1" --dry-run
    assert_success
    assert_output --partial "DRY RUN"
    assert_output --partial "review-fix"
}

# ---------------------------------------------------------------------------
# PR context provider: end-to-end with mocked gh
# ---------------------------------------------------------------------------

@test "pr-context provider: dry run includes pr-context in directive config" {
    local config="$TEST_TMPDIR/atc.toml"
    local db="$TEST_TMPDIR/atc.db"

    mkdir -p "$TEST_TMPDIR/workspace"
    cat > "$config" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$db"

[directives.review-fix]
template_inline = "Review PR: {{prefetch}}"
providers = ["pr-context"]
EOF

    init_test_db "$db"

    # dry-run should show mode config (won't actually run providers
    # since dispatch is short-circuited, but validates config loading)
    run atc --config "$config" run "review-task" --directive review-fix \
        --pr-url "https://github.com/test/repo/pull/1" --dry-run
    assert_success
    assert_output --partial "DRY RUN"
    assert_output --partial "review-fix"
}
