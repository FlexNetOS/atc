#!/usr/bin/env bats
# Tests for `atc quick` subcommand and ephemeral mode.
# No external dependencies (claude, git-kb, tmux) required —
# these test argument parsing, template listing, dry-run output, and error paths.

load helpers/common

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Create a minimal .atc/ directory with templates for testing.
setup_atc_dir() {
    mkdir -p "$TEST_TMPDIR/.atc/templates" "$TEST_TMPDIR/.atc/components" "$TEST_TMPDIR/.atc/partials"
    # Write commit-message template
    cat > "$TEST_TMPDIR/.atc/templates/commit-message.md" <<'EOF'
---
required_params: [slug, diff]
max_turns: 1
---
Summarize the following document changes as a git commit message.
Document: {{slug}}
Diff:
{{diff}}
EOF
    # Write doc-edit template
    cat > "$TEST_TMPDIR/.atc/templates/doc-edit.md" <<'EOF'
---
directive: implement
required_params: [slug, directive]
---
Edit the GitKB document at {{slug}}.
User directive: {{directive}}
EOF
    # Write minimal atc.toml
    cat > "$TEST_TMPDIR/.atc/config.toml" <<'EOF'
[prompt]
templates_dir = ".atc/templates"
components_dir = ".atc/components"
partials_dir = ".atc/partials"
EOF
}

# ---------------------------------------------------------------------------
# atc quick
# ---------------------------------------------------------------------------

@test "atc quick --help exits 0 and shows usage" {
    run atc quick --help
    assert_success
    assert_output --partial "Lightweight AI dispatch"
    assert_output --partial "template"
}

@test "atc quick --list shows commit-message and doc-edit" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    run atc quick --list dummy
    assert_success
    assert_output --partial "commit-message"
    assert_output --partial "doc-edit"
}

@test "atc quick commit-message --dry-run succeeds and shows ephemeral" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    run atc quick --dry-run commit-message --param slug=test/doc --param diff="added field"
    assert_success
    assert_output --partial "(ephemeral)"
}

# ---------------------------------------------------------------------------
# atc run --ephemeral
# ---------------------------------------------------------------------------

@test "atc run --ephemeral without --inline fails" {
    run atc run --ephemeral commit-message --param slug=x --param diff=y
    assert_failure
    assert_output --partial "--ephemeral requires --inline"
}
