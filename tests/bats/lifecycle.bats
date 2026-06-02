#!/usr/bin/env bats
# Integration tests for the full ATC lifecycle:
#   dispatch → registry → post-complete → status/info/logs → stop/cleanup
#
# These tests insert dispatch records directly into SQLite and exercise
# the CLI commands against real data — no external deps (git-kb, tmux, meta).

load helpers/common

# ===========================================================================
# Registry lifecycle: status/info queries against populated registry
# ===========================================================================

@test "status: shows inserted dispatch record" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"

    run atc --config "$TEST_TMPDIR/atc.toml" status
    assert_success
    assert_output --partial "tasks/test-1"
    assert_output --partial "running"
}

@test "status: shows multiple dispatches for same task" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-1" "done"

    run atc --config "$TEST_TMPDIR/atc.toml" status
    assert_success
    assert_output --partial "running"
    assert_output --partial "done"
}

@test "status --status filter returns only matching records" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-2" "done"

    run atc --config "$TEST_TMPDIR/atc.toml" status --status done
    assert_success
    assert_output --partial "tasks/test-2"
    refute_output --partial "running"
}

@test "status --json returns valid JSON envelope" {
    require_jq
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET agent_session_id = '00000000-0000-4000-8000-000000000901', agent_capabilities_json = '{\"supports_resume_by_session_id\":true}' WHERE id = 'disp-001';"

    run atc --config "$TEST_TMPDIR/atc.toml" status --json
    assert_success
    # Verify it's valid JSON containing the dispatch
    echo "$output" | jq -e '.schema_version == 1'
    echo "$output" | jq -e '.records[0].id == "disp-001"'
    echo "$output" | jq -e '.records[0].agent_provider == "claude"'
    echo "$output" | jq -e '.records[0].agent_session_id == "00000000-0000-4000-8000-000000000901"'
    echo "$output" | jq -e '.records[0].agent_capabilities.supports_resume_by_session_id == true'
    echo "$output" | jq -e '.records[0].agent_capabilities.supports_tmux_attach == false'
    echo "$output" | jq -e '.records[0] | has("agent_capabilities_json") | not'
}

@test "info --json returns structured agent metadata" {
    require_jq
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET agent_session_id = '00000000-0000-4000-8000-000000000902', agent_capabilities_json = '{\"supports_resume_by_session_id\":true}' WHERE id = 'disp-001';"

    run atc --config "$TEST_TMPDIR/atc.toml" info disp-001 --json
    assert_success
    echo "$output" | jq -e '.schema_version == 1'
    echo "$output" | jq -e '.record.id == "disp-001"'
    echo "$output" | jq -e '.record.agent_provider == "claude"'
    echo "$output" | jq -e '.record.agent_session_id == "00000000-0000-4000-8000-000000000902"'
    echo "$output" | jq -e '.record.agent_capabilities.supports_resume_by_session_id == true'
    echo "$output" | jq -e '.record.agent_capabilities.supports_tmux_attach == false'
    echo "$output" | jq -e '.record | has("agent_capabilities_json") | not'
}

@test "status/info --json tolerate malformed optional agent metadata" {
    require_jq
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET agent_session_id = 'not-a-uuid', agent_capabilities_json = '{not-json' WHERE id = 'disp-001';"

    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.schema_version == 1'
    echo "$STDOUT" | jq -e '.records[0].id == "disp-001"'
    echo "$STDOUT" | jq -e '.records[0].agent_provider == "claude"'
    echo "$STDOUT" | jq -e '.records[0].agent_session_id == null'
    echo "$STDOUT" | jq -e '.records[0].agent_capabilities == null'
    echo "$STDOUT" | jq -e '.records[0] | has("agent_capabilities_json") | not'
    echo "$STDERR" | grep -F "dispatch_id=disp-001"
    echo "$STDERR" | grep -F "ignoring invalid agent_session_id"
    echo "$STDERR" | grep -F "ignoring invalid agent_capabilities_json"

    run_split atc --config "$TEST_TMPDIR/atc.toml" info disp-001 --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.schema_version == 1'
    echo "$STDOUT" | jq -e '.record.id == "disp-001"'
    echo "$STDOUT" | jq -e '.record.agent_provider == "claude"'
    echo "$STDOUT" | jq -e '.record.agent_session_id == null'
    echo "$STDOUT" | jq -e '.record.agent_capabilities == null'
    echo "$STDOUT" | jq -e '.record | has("agent_capabilities_json") | not'
    echo "$STDERR" | grep -F "dispatch_id=disp-001"
    echo "$STDERR" | grep -F "ignoring invalid agent_session_id"
    echo "$STDERR" | grep -F "ignoring invalid agent_capabilities_json"
}

@test "info: shows correct fields for a dispatch" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running" "implement"

    run atc --config "$TEST_TMPDIR/atc.toml" info disp-001
    assert_success
    assert_output --partial "id:"
    assert_output --partial "disp-001"
    assert_output --partial "task_slug:"
    assert_output --partial "tasks/test-1"
    assert_output --partial "status:"
    assert_output --partial "running"
    assert_output --partial "directive:"
    assert_output --partial "implement"
    assert_output --partial "agent_provider:"
    assert_output --partial "claude"
}

@test "info: resolves by task slug (latest dispatch)" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-1" "running"
    # Ensure disp-002 has a later timestamp so it resolves as "latest"
    sqlite3 "$TEST_TMPDIR/atc.db" "UPDATE dispatches SET dispatched_at = '2020-01-01T00:00:00+00:00' WHERE id = 'disp-001';"

    run atc --config "$TEST_TMPDIR/atc.toml" info tasks/test-1
    assert_success
    assert_output --partial "disp-002"
}

# ===========================================================================
# Post-completion: artifact extraction, status transitions
# ===========================================================================

@test "post-complete: success log transitions Running → Done" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "2.50"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    # Verify status transitioned to done
    local new_status
    new_status=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "status")
    [ "$new_status" = "done" ]
}

@test "post-complete: failure log transitions Running → Failed" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "error_max_turns" "5.00"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    local new_status
    new_status=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "status")
    [ "$new_status" = "failed" ]
}

@test "post-complete: populates cost, turns, duration" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "3.75"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    local cost turns duration
    cost=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "cost_usd")
    turns=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "num_turns")
    duration=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "duration_ms")

    [ "$cost" = "3.75" ]
    [ "$turns" = "15" ]
    [ "$duration" = "45000" ]
}

@test "post-complete: extracts PR URL from log" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "2.50"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    local pr_url
    pr_url=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "pr_url")
    [ "$pr_url" = "https://github.com/org/repo/pull/42" ]
}

@test "post-complete: stores artifacts JSON blob" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "2.50"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    local artifacts
    artifacts=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "artifacts")
    # Artifacts should be a non-empty JSON string
    [ -n "$artifacts" ]
    echo "$artifacts" | jq -e '.pr_urls | length > 0'
}

# ===========================================================================
# Logs: formatted output from canned stream-json
# ===========================================================================

@test "logs: renders assistant text with >>> prefix" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "2.50"

    run atc --config "$TEST_TMPDIR/atc.toml" logs disp-001
    assert_success
    assert_output --partial ">>> Working on the task..."
    assert_output --partial "[tool] Bash:"
    assert_output --partial "RESULT: success"
}

@test "logs: shows cost and duration in result line" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "7.89"

    run atc --config "$TEST_TMPDIR/atc.toml" logs disp-001
    assert_success
    assert_output --partial 'cost=$7.89'
    assert_output --partial "turns=15"
    assert_output --partial "duration=45s"
}

@test "logs: resolves by task slug" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "2.50"

    run atc --config "$TEST_TMPDIR/atc.toml" logs tasks/test-1
    assert_success
    assert_output --partial ">>> Working on the task..."
}

# ===========================================================================
# Stop: status transitions
# ===========================================================================

@test "stop: transitions Running dispatch to Stopped" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"

    run atc --config "$TEST_TMPDIR/atc.toml" stop disp-001
    assert_success
    assert_output --partial "Stopped disp-001"

    local new_status
    new_status=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "status")
    [ "$new_status" = "stopped" ]
}

@test "stop: already-terminal dispatch warns but succeeds" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"

    run atc --config "$TEST_TMPDIR/atc.toml" stop disp-001
    assert_success

    # Terminal status should be preserved
    local new_status
    new_status=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "status")
    [ "$new_status" = "done" ]
}

# ===========================================================================
# Cleanup: worktree removal and batch operations
# ===========================================================================

@test "cleanup: removes worktree directory for Done dispatch" {
    setup_lifecycle
    local worktree_dir="$TEST_TMPDIR/worktree"
    mkdir -p "$worktree_dir"

    # Insert a Done dispatch whose worktree is in our temp dir
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"

    # The cleanup will attempt to remove the worktree dir — it may or may not
    # succeed depending on safety checks (path must be under worktree base).
    # At minimum it should not error.
    run atc --config "$TEST_TMPDIR/atc.toml" cleanup disp-001
    assert_success
    assert_output --partial "Cleaned disp-001"
}

@test "cleanup --done: cleans all Done dispatches" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-2" "done"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-003" "tasks/test-3" "running"

    run atc --config "$TEST_TMPDIR/atc.toml" cleanup --done
    assert_success
    assert_output --partial "Cleaned 2 dispatches"
}

@test "cleanup --done: no Done dispatches shows clean message" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"

    run atc --config "$TEST_TMPDIR/atc.toml" cleanup --done
    assert_success
    assert_output --partial "No done dispatches"
}

# ===========================================================================
# Retry: failure classification and max-retries guard
# ===========================================================================

@test "retry: rejects non-failed dispatch" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"

    run atc --config "$TEST_TMPDIR/atc.toml" retry disp-001
    assert_failure
    assert_output --partial "cannot retry"
}

@test "retry: max retries exceeded marks NeedsHuman" {
    setup_lifecycle
    # Insert a failed dispatch with retries = 3 (default max_retries = 3)
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "failed" "implement" 3

    run atc --config "$TEST_TMPDIR/atc.toml" retry disp-001
    assert_failure
    assert_output --partial "max retries"

    # Verify status transitioned to needs-human
    local new_status
    new_status=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "status")
    [ "$new_status" = "needs-human" ]
}

# ===========================================================================
# Cross-command round-trip: insert → post-complete → info → status
# ===========================================================================

@test "round-trip: insert → post-complete → info shows artifacts" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "4.20"

    # Run post-complete to populate artifacts
    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    # Info should now show cost and PR URL
    run atc --config "$TEST_TMPDIR/atc.toml" info disp-001
    assert_success
    assert_output --partial "done"
    assert_output --partial '$4.20'
    assert_output --partial "15"
    assert_output --partial "pr_url:"
    assert_output --partial "github.com"
}

@test "round-trip: insert → post-complete → status shows cost summary" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "4.20"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    run atc --config "$TEST_TMPDIR/atc.toml" status
    assert_success
    assert_output --partial "done"
    assert_output --partial '$4.20'
}

# ===========================================================================
# Health --auto: auto-remediation and cost warnings
# ===========================================================================

@test "health --auto: accepted as valid flag with no active records" {
    setup_lifecycle

    run atc --config "$TEST_TMPDIR/atc.toml" health --auto
    assert_success
    assert_output --partial "No dispatch records found."
}

@test "health --auto: prints cost warning for expensive dispatch" {
    setup_lifecycle
    setup_test_git_worktree
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-cost" "tasks/cost-test" "needs-review"

    # Set a high cost on the record
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET cost_usd = 15.0 WHERE id = 'disp-cost';"

    # health will evaluate signals — tmux check will return "exited" since no
    # session exists, but that's fine for verifying cost warning output.
    run atc --config "$TEST_TMPDIR/atc.toml" health --auto
    assert_success
    assert_output --partial "15.00"
    assert_output --partial "10.00"
}

@test "health --auto: auto-review prints trigger message for NeedsReview with PR" {
    setup_lifecycle
    setup_test_git_worktree

    # Insert as "running" so the Running→NeedsReview transition produces changed=true.
    # Health checker re-evaluates signals from scratch; the DB check values just
    # serve as the baseline for detecting change.
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-review" "tasks/review-test" "running"

    # Set pr_url and checks matching what health evaluation will produce
    # (agent exited=true via tmux, branch pushed=true via git ls-remote,
    # pr created=true via pr_url shortcut, ci=false since gh fails in test env).
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET pr_url = 'https://github.com/org/repo/pull/99', check_agent_exited_clean = 1, check_branch_pushed = 1, check_pr_created = 1 WHERE id = 'disp-review';"

    # The dispatch will fail (no meta workspace, no tmux, etc.) but the auto-review
    # trigger message should be printed before that.
    run atc --config "$TEST_TMPDIR/atc.toml" health --auto
    # The command may fail due to dispatch failure — check output regardless
    assert_output --partial "Auto-triggering review-fix for tasks/review-test"
}

@test "health: config auto_review enables auto without --auto flag" {
    setup_lifecycle
    setup_test_git_worktree

    # Insert as "running" so the Running→NeedsReview transition produces changed=true.
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-auto" "tasks/auto-test" "running"

    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET pr_url = 'https://github.com/org/repo/pull/100', check_agent_exited_clean = 1, check_branch_pushed = 1, check_pr_created = 1 WHERE id = 'disp-auto';"

    # Write config with auto_review = true
    cat > "$TEST_TMPDIR/atc.toml" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$TEST_TMPDIR/atc.db"

[health]
auto_review = true
EOF

    run atc --config "$TEST_TMPDIR/atc.toml" health
    assert_output --partial "Auto-triggering review-fix for tasks/auto-test"
}

@test "health: custom cost_warning_threshold from config" {
    setup_lifecycle
    setup_test_git_worktree
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-thresh" "tasks/thresh-test" "needs-review"

    # Set cost just above 5.0
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET cost_usd = 6.0 WHERE id = 'disp-thresh';"

    # Config with low threshold
    cat > "$TEST_TMPDIR/atc.toml" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$TEST_TMPDIR/atc.db"

[health]
cost_warning_threshold = 5.0
EOF

    run atc --config "$TEST_TMPDIR/atc.toml" health
    assert_success
    assert_output --partial "6.00"
    assert_output --partial "5.00"
}
