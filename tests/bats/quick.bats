#!/usr/bin/env bats
# Tests for `atc quick` subcommand and ephemeral mode.
# No external dependencies (claude, git-kb, tmux) required —
# these test argument parsing, template listing, dry-run output, and error paths.

load helpers/common

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        skip "jq not installed"
    fi
}

run_split() {
    local stdout_file="$TEST_TMPDIR/.stdout"
    local stderr_file="$TEST_TMPDIR/.stderr"
    "$@" >"$stdout_file" 2>"$stderr_file" && SPLIT_STATUS=0 || SPLIT_STATUS=$?
    STDOUT="$(cat "$stdout_file")"
    STDERR="$(cat "$stderr_file")"
}

init_named_git_branch() {
    git init --quiet
    git symbolic-ref HEAD refs/heads/main
}

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
    cat > "$TEST_TMPDIR/.atc/config.toml" <<EOF
[prompt]
templates_dir = "$TEST_TMPDIR/.atc/templates"
components_dir = "$TEST_TMPDIR/.atc/components"
partials_dir = "$TEST_TMPDIR/.atc/partials"

[registry]
path = "$TEST_TMPDIR/atc.db"
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
    run atc quick --list
    assert_success
    assert_output --partial "commit-message"
    assert_output --partial "doc-edit"
}

@test "atc quick commit-message --dry-run succeeds and shows ephemeral" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    init_named_git_branch
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

@test "atc run --ephemeral --inline --json does not pass provider session id" {
    require_jq
    setup_atc_dir
    mkdir -p "$TEST_TMPDIR/bin"
    cat > "$TEST_TMPDIR/bin/claude" <<'SH'
#!/bin/sh
printf '%s\n' "$@" > "$ATC_ARG_CAPTURE"
exit 0
SH
    chmod +x "$TEST_TMPDIR/bin/claude"
    export PATH="$TEST_TMPDIR/bin:$PATH"
    export ATC_ARG_CAPTURE="$TEST_TMPDIR/claude.args"

    cd "$TEST_TMPDIR"
    init_named_git_branch
    run_split atc run commit-message --ephemeral --inline --no-worktree --json \
        --param slug=test/doc --param diff="added field"
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.kind == "dispatch"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_provider == "claude"' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_session_id == null' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_transcript_cwd == null' >/dev/null
    echo "$STDOUT" | jq -e '.data.agent_capabilities == null' >/dev/null
    [[ "$STDOUT" != *"--session-id"* ]]
    if grep -q -- "--session-id" "$ATC_ARG_CAPTURE"; then
        fail "ephemeral dispatch passed --session-id to claude"
    fi
    if [[ -f "$TEST_TMPDIR/atc.db" ]]; then
        [ "$(sqlite3 "$TEST_TMPDIR/atc.db" "SELECT CASE WHEN EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'dispatches') THEN (SELECT COUNT(*) FROM dispatches) ELSE 0 END;")" -eq 0 ]
    fi
}
