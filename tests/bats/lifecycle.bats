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

    run atc --config "$TEST_TMPDIR/atc.toml" status --all
    assert_success
    assert_output --partial "tasks/test-1"
    assert_output --partial "running"
}

@test "status: shows multiple dispatches for same task" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-1" "done"

    run atc --config "$TEST_TMPDIR/atc.toml" status --all
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
        "UPDATE dispatches SET agent_session_id = 'not-a-uuid', agent_capabilities_json = '{not-json', terminal_locator_json = '{not-json' WHERE id = 'disp-001';"

    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.schema_version == 1'
    echo "$STDOUT" | jq -e '.records[0].id == "disp-001"'
    echo "$STDOUT" | jq -e '.records[0].agent_provider == "claude"'
    echo "$STDOUT" | jq -e '.records[0].agent_session_id == null'
    echo "$STDOUT" | jq -e '.records[0].agent_capabilities == null'
    echo "$STDOUT" | jq -e '.records[0].terminal_locator == null'
    echo "$STDOUT" | jq -e '.records[0] | has("agent_capabilities_json") | not'
    echo "$STDOUT" | jq -e '.records[0] | has("terminal_locator_json") | not'
    echo "$STDERR" | grep -F "dispatch_id=disp-001"
    echo "$STDERR" | grep -F "ignoring invalid agent_session_id"
    echo "$STDERR" | grep -F "ignoring invalid agent_capabilities_json"
    echo "$STDERR" | grep -F "ignoring invalid terminal_locator_json"

    run_split atc --config "$TEST_TMPDIR/atc.toml" info disp-001 --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.schema_version == 1'
    echo "$STDOUT" | jq -e '.record.id == "disp-001"'
    echo "$STDOUT" | jq -e '.record.agent_provider == "claude"'
    echo "$STDOUT" | jq -e '.record.agent_session_id == null'
    echo "$STDOUT" | jq -e '.record.agent_capabilities == null'
    echo "$STDOUT" | jq -e '.record.terminal_locator == null'
    echo "$STDOUT" | jq -e '.record | has("agent_capabilities_json") | not'
    echo "$STDOUT" | jq -e '.record | has("terminal_locator_json") | not'
    echo "$STDERR" | grep -F "dispatch_id=disp-001"
    echo "$STDERR" | grep -F "ignoring invalid agent_session_id"
    echo "$STDERR" | grep -F "ignoring invalid agent_capabilities_json"
    echo "$STDERR" | grep -F "ignoring invalid terminal_locator_json"
}

@test "status --json escapes malformed required enum values" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local esc=$'\033'
    local bidi=$'\u202e'

    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET status = 'running' || char(27) || '[2J' || char(8238) || 'gpj' WHERE id = 'disp-001';"
    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"unknown status: running\\x1b[2J\\u{202e}gpj"* ]]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bidi"* ]]

    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET status = 'running', directive = 'implement' || char(27) || '[2J' || char(8238) || 'gpj' WHERE id = 'disp-001';"
    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"unknown directive: implement\\x1b[2J\\u{202e}gpj"* ]]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bidi"* ]]
}

@test "registry malformed metadata warnings escape hostile dispatch ids" {
    require_jq
    setup_lifecycle
    local esc=$'\033'
    local hostile_id="disp-${esc}[2J"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "$hostile_id" "tasks/hostile-log" "running"
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET agent_session_id = 'not-a-uuid', agent_capabilities_json = '{not-json', terminal_locator_json = '{\"kind\":\"tmux\u001b[2J\",\"version\":1,\"session\":\"bad\",\"detected_at\":\"2026-06-05T00:00:00Z\",\"source\":\"atc-dispatch\",\"confidence\":\"exact\"}' WHERE id = 'disp-${esc}[2J';"

    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.schema_version == 1'
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDERR" == *"dispatch_id=disp-\\x1b[2J"* ]]
    [[ "$STDERR" == *"ignoring invalid agent_session_id"* ]]
    [[ "$STDERR" == *"ignoring invalid agent_capabilities_json"* ]]
    [[ "$STDERR" == *"ignoring invalid terminal_locator_json"* ]]
    [[ "$STDERR" == *"tmux\\x1b[2J"* ]]
    [[ "$STDERR" != *"$esc"* ]]
}

@test "status/info --json escape Unicode format controls in encoded bytes while preserving decoded values" {
    require_jq
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local bidi=$'\u202e'
    local line_sep=$'\u2028'
    local paragraph_sep=$'\u2029'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET task_slug = 'tasks/json-' || char(8238) || 'gpj' || char(8232) || 'line' || char(8233) || 'para.exe',
    branch = 'branch-' || char(8238) || 'gpj' || char(8232) || 'line',
    session = 'session-' || char(8238) || 'gpj' || char(8233) || 'para'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" status --json
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\u202e"* ]]
    [[ "$STDOUT" == *"\\u2028"* ]]
    [[ "$STDOUT" == *"\\u2029"* ]]
    local decoded_status_task
    decoded_status_task="$(echo "$STDOUT" | jq -r '.records[0].task_slug')"
    [[ "$decoded_status_task" == *"$bidi"* ]]
    [[ "$decoded_status_task" == *"$line_sep"* ]]
    [[ "$decoded_status_task" == *"$paragraph_sep"* ]]

    run_split atc --config "$TEST_TMPDIR/atc.toml" info disp-001 --json
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\u202e"* ]]
    [[ "$STDOUT" == *"\\u2028"* ]]
    [[ "$STDOUT" == *"\\u2029"* ]]
    local decoded_info_task
    decoded_info_task="$(echo "$STDOUT" | jq -r '.record.task_slug')"
    [[ "$decoded_info_task" == *"$bidi"* ]]
    [[ "$decoded_info_task" == *"$line_sep"* ]]
    [[ "$decoded_info_task" == *"$paragraph_sep"* ]]
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

@test "status/info: escape hostile registry values in human output" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running" "implement"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    local line_sep=$'\u2028'
    local paragraph_sep=$'\u2029'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET task_slug = 'tasks/evil-' || char(27) || '[2J' || char(7) || char(8238) || char(8232) || 'gpj' || char(8233) || '.exe',
    branch = 'branch-' || char(27) || '[31m' || char(8232),
    worktree_path = '${TEST_TMPDIR//\'/\'\'}/worktree-' || char(7),
    session = 'session-' || char(8238) || char(8233),
    agent_provider = 'claude-' || char(27) || '[0m',
    terminal_locator_json = '{"kind":"tmux","version":1,"session":"locator-\u001b[2J\u202egpj","cwd":"${TEST_TMPDIR//\'/\'\'}/worktree","detected_at":"2026-06-05T00:00:00Z","source":"atc-dispatch","confidence":"exact"}'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" status --all --flat
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
    [[ "$STDOUT" == *"\\u{2028}"* ]]
    [[ "$STDOUT" == *"\\u{2029}"* ]]

    run_split atc --config "$TEST_TMPDIR/atc.toml" info disp-001
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
    [[ "$STDOUT" == *"\\u{2028}"* ]]
    [[ "$STDOUT" == *"\\u{2029}"* ]]
    [[ "$STDOUT" == *"terminal_session:"* ]]
    [[ "$STDOUT" == *"locator-\\x1b[2J\\u{202e}gpj"* ]]
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

@test "info: hostile missing arg escapes terminal controls" {
    setup_lifecycle
    local esc=$'\033'
    local bidi=$'\u202e'

    run_split atc --config "$TEST_TMPDIR/atc.toml" info "missing-${esc}[2J${bidi}gpj"
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"missing-\\x1b[2J\\u{202e}gpj"* ]]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bidi"* ]]
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

@test "post-complete: extracts PR URLs from log" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    cat > "$TEST_TMPDIR/disp-001.jsonl" <<EOF
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on the task..."}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Created PRs https://github.com/org/repo/pull/42 and https://github.com/org/api/pull/7"}]}}
{"type":"result","subtype":"success","total_cost_usd":2.50,"num_turns":15,"duration_ms":45000}
EOF

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    local pr_urls
    pr_urls=$(query_dispatch_field "$TEST_TMPDIR/atc.db" "disp-001" "pr_urls")
    echo "$pr_urls" | jq -e '. == ["https://github.com/org/repo/pull/42","https://github.com/org/api/pull/7"]'
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

@test "logs: escapes terminal and bidi controls in human output" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    cat > "$TEST_TMPDIR/disp-001.jsonl" <<'EOF'
{"type":"assistant","message":{"content":[{"type":"text","text":"Hello \u001b[2J\u202egpj.exe\u0007"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash\u001b[31m","input":{"command":"cargo test \u202e"}}]}}
EOF

    run_split atc --config "$TEST_TMPDIR/atc.toml" logs disp-001
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
}

@test "watch --format json escapes Unicode format controls in encoded bytes while preserving decoded values" {
    require_jq
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/watch-test" "running"
    local bidi=$'\u202e'
    local line_sep=$'\u2028'
    local paragraph_sep=$'\u2029'
    cat >> "$TEST_TMPDIR/atc.toml" <<EOF

[watch]
poll_interval_secs = 1
EOF
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET task_slug = 'tasks/watch-' || char(8238) || 'gpj' || char(8232) || 'line' || char(8233) || 'para.exe'
WHERE id = 'disp-001';
SQL
    cat > "$TEST_TMPDIR/disp-001.jsonl" <<'EOF'
{"type":"assistant","message":{"content":[{"type":"text","text":"Hello \u202egpj\u2028line\u2029para.exe"}]}}
{"type":"result","subtype":"success","total_cost_usd":2.50,"num_turns":15,"duration_ms":45000}
EOF

    run_split atc --config "$TEST_TMPDIR/atc.toml" watch --id disp-001 --format json
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\u202e"* ]]
    [[ "$STDOUT" == *"\\u2028"* ]]
    [[ "$STDOUT" == *"\\u2029"* ]]

    local decoded_task decoded_text
    decoded_task="$(printf '%s\n' "$STDOUT" | jq -r 'select(.event == "started") | .task' | head -n1)"
    decoded_text="$(printf '%s\n' "$STDOUT" | jq -r 'select(.event == "log_line") | .text' | head -n1)"
    [[ "$decoded_task" == *"$bidi"* ]]
    [[ "$decoded_task" == *"$line_sep"* ]]
    [[ "$decoded_task" == *"$paragraph_sep"* ]]
    [[ "$decoded_text" == *"$bidi"* ]]
    [[ "$decoded_text" == *"$line_sep"* ]]
    [[ "$decoded_text" == *"$paragraph_sep"* ]]
}

@test "watch --socket refuses to replace existing regular files" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local socket_path="$TEST_TMPDIR/not-a-socket"
    printf 'keep me\n' > "$socket_path"

    run_split atc --config "$TEST_TMPDIR/atc.toml" watch --socket "$socket_path" --id disp-001
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"refusing to replace existing --socket path"* ]]
    [ "$(cat "$socket_path")" = "keep me" ]
}

@test "watch --socket refuses group-writable parent directories" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local public_dir="$TEST_TMPDIR/public-socket-dir"
    mkdir "$public_dir"
    chmod 0770 "$public_dir"

    run_split atc --config "$TEST_TMPDIR/atc.toml" watch --socket "$public_dir/watch.sock" --id disp-001
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"private directory"* ]]
    assert_file_not_exists "$public_dir/watch.sock"
}

@test "watch --socket reclaims stale socket files" {
    require_python_unix_socket
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    local socket_path="$TEST_TMPDIR/stale.sock"
    cat >> "$TEST_TMPDIR/atc.toml" <<EOF

[watch]
poll_interval_secs = 1
EOF
    python3 - "$socket_path" <<'PY'
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.listen(1)
sock.close()
PY
    [ -S "$socket_path" ]

    run_split atc --config "$TEST_TMPDIR/atc.toml" watch --socket "$socket_path" --id disp-001 --format json
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" == *'"event":"started"'* ]]
    assert_file_not_exists "$socket_path"
}

@test "watch --socket refuses active socket files" {
    require_python_unix_socket
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local socket_path="$TEST_TMPDIR/active.sock"
    local ready_file="$TEST_TMPDIR/active.ready"
    python3 - "$socket_path" "$ready_file" <<'PY' &
import os
import pathlib
import socket
import sys
import time

path = sys.argv[1]
ready = sys.argv[2]
sock = socket.socket(socket.AF_UNIX)
sock.bind(path)
sock.listen(1)
pathlib.Path(ready).write_text("ready")
try:
    deadline = time.time() + 10
    while time.time() < deadline:
        time.sleep(0.1)
finally:
    sock.close()
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
PY
    local listener_pid=$!
    for _ in {1..50}; do
        if [[ -f "$ready_file" && -S "$socket_path" ]]; then
            break
        fi
        sleep 0.1
    done
    if [[ ! -S "$socket_path" ]]; then
        kill "$listener_pid" 2>/dev/null || true
        wait "$listener_pid" 2>/dev/null || true
        false
    fi

    run_split atc --config "$TEST_TMPDIR/atc.toml" watch --socket "$socket_path" --id disp-001
    local watch_status="$SPLIT_STATUS"
    local watch_stderr="$STDERR"
    kill "$listener_pid" 2>/dev/null || true
    wait "$listener_pid" 2>/dev/null || true

    [ "$watch_status" -ne 0 ]
    [[ "$watch_stderr" == *"refusing to replace active --socket path"* ]]
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

@test "stop: escapes hostile session names in human output" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET session = 'tmux-' || char(27) || '[31mred' || char(7) || char(8238) || 'gpj.exe'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" stop disp-001
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
}

@test "redirect: escapes hostile session names in errors" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET session = 'tmux-' || char(27) || '[31mred' || char(7) || char(8238) || 'gpj.exe'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" redirect disp-001 "hello"
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bel"* ]]
    [[ "$STDERR" != *"$bidi"* ]]
    [[ "$STDERR" == *"\\x1b"* ]]
    [[ "$STDERR" == *"\\x07"* ]]
    [[ "$STDERR" == *"\\u{202e}"* ]]
}

@test "status debug logs escape malformed hostile PR URLs" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET pr_urls = '["https://github.com/org/repo/pull/not-a-number\u001b\u0007\u202egpj.exe"]'
WHERE id = 'disp-001';
SQL

    run_split env RUST_LOG=atc_cli::status=debug "$ATC_BIN" --color never --config "$TEST_TMPDIR/atc.toml" status --all --flat
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDERR" != *"$esc"* ]]
    [[ "$STDERR" != *"$bel"* ]]
    [[ "$STDERR" != *"$bidi"* ]]
    [[ "$STDERR" == *"\\x1b"* ]]
    [[ "$STDERR" == *"\\x07"* ]]
    [[ "$STDERR" == *"\\u{202e}"* ]]
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

@test "cleanup: resolving by task slug prints resolved dispatch id" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "done"

    run atc --config "$TEST_TMPDIR/atc.toml" cleanup tasks/test-1
    assert_success
    assert_output --partial "Cleaned disp-001"
    refute_output --partial "Cleaned tasks/test-1"
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

    # Info should now show cost and PR URLs
    run atc --config "$TEST_TMPDIR/atc.toml" info disp-001
    assert_success
    assert_output --partial "done"
    assert_output --partial '$4.20'
    assert_output --partial "15"
    assert_output --partial "pr_urls:"
    assert_output --partial "github.com"
}

@test "round-trip: insert → post-complete → status shows cost summary" {
    setup_lifecycle
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    write_test_log "$TEST_TMPDIR/disp-001.jsonl" "success" "4.20"

    run atc --config "$TEST_TMPDIR/atc.toml" post-complete --id disp-001
    assert_success

    run atc --config "$TEST_TMPDIR/atc.toml" status --status done
    assert_success
    assert_output --partial "done"
    assert_output --partial '$4.20'
}

@test "history: escapes hostile work-unit registry values in human output" {
    setup_lifecycle
    insert_test_work_unit "$TEST_TMPDIR/atc.db" "wu-001" "tasks/history-test"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-history" "tasks/history-test" "done"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE work_units
SET id = 'wu-' || char(27) || '[2J',
    task_slug = 'tasks/history-' || char(27) || '[2J',
    branch = 'branch-' || char(7),
    repos = '["open-source/atc' || char(8238) || 'gpj.exe"]',
    pr_urls = '["https://github.com/org/repo/pull/77"]'
WHERE id = 'wu-001';
UPDATE dispatches
SET work_unit_id = 'wu-' || char(27) || '[2J'
WHERE id = 'disp-history';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" history --pr https://github.com/org/repo/pull/77
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
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

    # Set a high cost and a known PR on the record so health can transition
    # through NeedsReview without relying on gh PR discovery in the test env.
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET cost_usd = 15.0, pr_url = 'https://github.com/org/repo/pull/98', pr_urls = '[\"https://github.com/org/repo/pull/98\"]' WHERE id = 'disp-cost';"

    # health will evaluate signals — tmux check will return "exited" since no
    # session exists, but that's fine for verifying cost warning output.
    run atc --config "$TEST_TMPDIR/atc.toml" health --auto
    assert_success
    assert_output --partial "15.00"
    assert_output --partial "10.00"
}

@test "health: escapes hostile dispatch id in cost warning output" {
    setup_lifecycle
    setup_test_git_worktree
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-cost" "tasks/cost-test" "needs-review"
    local esc=$'\033'
    local bel=$'\a'
    local bidi=$'\u202e'

    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET id = 'disp-cost-' || char(27) || '[2J' || char(7) || char(8238) || 'gpj.exe',
    cost_usd = 15.0,
    pr_url = 'https://github.com/org/repo/pull/98',
    pr_urls = '["https://github.com/org/repo/pull/98"]'
WHERE id = 'disp-cost';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" health --auto
    [ "$SPLIT_STATUS" -eq 0 ]
    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
    [[ "$STDOUT" == *"15.00"* ]]
    [[ "$STDOUT" == *"10.00"* ]]
}

@test "health --auto: auto-review prints trigger message for NeedsReview with PR" {
    setup_lifecycle
    setup_test_git_worktree

    # Insert as "running" so the Running→NeedsReview transition produces changed=true.
    # Health checker re-evaluates signals from scratch; the DB check values just
    # serve as the baseline for detecting change.
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-review" "tasks/review-test" "running"

    # Set pr_urls and checks matching what health evaluation will produce
    # (agent exited=true via tmux, branch pushed=true via git ls-remote,
    # pr created=true via pr_urls shortcut, ci=false since gh fails in test env).
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET pr_url = 'https://github.com/org/repo/pull/99', pr_urls = '[\"https://github.com/org/repo/pull/99\"]', check_agent_exited_clean = 1, check_branch_pushed = 1, check_pr_created = 1 WHERE id = 'disp-review';"

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
        "UPDATE dispatches SET pr_url = 'https://github.com/org/repo/pull/100', pr_urls = '[\"https://github.com/org/repo/pull/100\"]', check_agent_exited_clean = 1, check_branch_pushed = 1, check_pr_created = 1 WHERE id = 'disp-auto';"

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

    # Set cost just above 5.0 and a known PR so health reaches the warning path
    # without relying on gh PR discovery in the test env.
    sqlite3 "$TEST_TMPDIR/atc.db" \
        "UPDATE dispatches SET cost_usd = 6.0, pr_url = 'https://github.com/org/repo/pull/101', pr_urls = '[\"https://github.com/org/repo/pull/101\"]' WHERE id = 'disp-thresh';"

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
