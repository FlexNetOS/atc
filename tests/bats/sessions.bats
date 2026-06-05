#!/usr/bin/env bats
# Tests for `atc sessions` / `atc tui` non-interactive surfaces.

load helpers/common

setup_sessions_data() {
    setup_lifecycle
    insert_test_work_unit "$TEST_TMPDIR/atc.db" "wu-001" "tasks/test-1"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-001" "tasks/test-1" "running"
    touch "$TEST_TMPDIR/disp-001.jsonl"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET work_unit_id = 'wu-001',
    cost_usd = 1.25,
    num_turns = 7,
    duration_ms = 65000,
    pr_url = 'https://github.com/org/repo/pull/42',
    pr_urls = '["https://github.com/org/repo/pull/42","https://github.com/org/api/pull/7"]',
    agent_session_id = '00000000-0000-4000-8000-000000000794',
    agent_transcript_cwd = '$TEST_TMPDIR/worktree',
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true,"supports_tmux_attach":true,"supports_tmux_redirect":true,"supports_stream_json_output":true,"supports_cost_and_turn_reporting":true}',
    terminal_locator_json = '{"backend":"tmux","version":1,"session":"disp-001","cwd":"$TEST_TMPDIR/worktree","detected_at":"2026-06-05T00:00:00Z","source":"atc-dispatch","confidence":"exact"}'
WHERE id = 'disp-001';
SQL
}

@test "sessions --help documents alias and non-interactive modes" {
    run atc sessions --help
    assert_success
    assert_output --partial "--once"
    assert_output --partial "--json"
    assert_output --partial "--group"

    run atc tui --help
    assert_success
    assert_output --partial "Browse and switch"
    assert_output --partial "--once"
}

@test "open-session --help documents non-attaching json preview" {
    run atc open-session --help
    assert_success
    assert_output --partial "atc://session"
    assert_output --partial "--json"
    assert_output --partial "without attaching"
}

@test "sessions --json emits session rows and capability action state" {
    require_jq
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].uri == "atc://session/disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].task_slug == "tasks/test-1"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].work_unit_id == "wu-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].provider == "claude"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].provider_session_id == "00000000-0000-4000-8000-000000000794"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].pr_urls == ["https://github.com/org/repo/pull/42","https://github.com/org/api/pull/7"]' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].terminal_locator.backend == "tmux"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].terminal_locator.session == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].terminal_status.state | type == "string"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].open_shell.action == "open-session"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.attach.enabled == .rows[0].open_shell.enabled' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.redirect.enabled == true' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.resume.enabled == false' >/dev/null
    echo "$STDOUT" | jq -e '.work_units[0].id == "wu-001"' >/dev/null
}

@test "sessions backfills legacy Claude capabilities for action state" {
    require_jq
    setup_sessions_data
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET agent_capabilities_json = NULL
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.rows[0].terminal_locator.backend == "tmux"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.attach.enabled == .rows[0].open_shell.enabled' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.redirect.enabled == true' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.resume.enabled == false' >/dev/null
}

@test "open-session --json resolves session URI without attaching" {
    require_jq
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" open-session atc://session/disp-001 --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.kind == "open-session"' >/dev/null
    echo "$STDOUT" | jq -e '.data.dispatch_id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.data.uri == "atc://session/disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.data.session == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.data.terminal_locator.backend == "tmux"' >/dev/null
    echo "$STDOUT" | jq -e '.data.terminal_status.state | type == "string"' >/dev/null
    echo "$STDOUT" | jq -e '.data.open_shell.action == "open-session"' >/dev/null
}

@test "open-session rejects ambiguous active task slug with candidates" {
    setup_sessions_data
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-ambiguous" "tasks/test-1" "running"

    run_split atc --config "$TEST_TMPDIR/atc.toml" open-session tasks/test-1 --json
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"multiple active dispatches"* ]]
    [[ "$STDERR" == *"disp-001"* ]]
    [[ "$STDERR" == *"disp-ambiguous"* ]]
}

@test "tui --json is the same sessions command surface" {
    require_jq
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]
    local sessions_json="$STDOUT"

    run_split atc --config "$TEST_TMPDIR/atc.toml" tui --json
    [ "$SPLIT_STATUS" -eq 0 ]

    diff <(echo "$sessions_json" | jq -S .) <(echo "$STDOUT" | jq -S .)
}

@test "sessions --once and tui --once render the human table" {
    setup_sessions_data

    run atc --config "$TEST_TMPDIR/atc.toml" sessions --once
    assert_success
    assert_output --partial "ATC Sessions"
    assert_output --partial "tasks/test-1"
    assert_output --partial "claude"

    run atc --config "$TEST_TMPDIR/atc.toml" tui --once
    assert_success
    assert_output --partial "ATC Sessions"
    assert_output --partial "tasks/test-1"
}

@test "sessions filters by task provider status and group" {
    require_jq
    setup_sessions_data
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-002" "tasks/test-2" "done"

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions \
        --json --task tasks/test-1 --provider claude --status running --group work-unit
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.group == "work-unit"' >/dev/null
    echo "$STDOUT" | jq -e '.rows | length == 1' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].group_key == "wu-001"' >/dev/null
}

@test "sessions task and search filters use linked work-unit task fallback" {
    require_jq
    setup_sessions_data
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET task_slug = NULL
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --task tasks/test-1
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.rows | length == 1' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].task_slug == "tasks/test-1"' >/dev/null

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --search tasks/test-1
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.rows | length == 1' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
}

@test "sessions default includes active and recent terminal records only" {
    require_jq
    setup_sessions_data
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-recent-done" "tasks/test-recent" "done"
    insert_test_dispatch "$TEST_TMPDIR/atc.db" "disp-old-done" "tasks/test-old" "done"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET updated_at = strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now', '-25 hours')
WHERE id = 'disp-old-done';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.rows | map(.id) | index("disp-001") != null' >/dev/null
    echo "$STDOUT" | jq -e '.rows | map(.id) | index("disp-recent-done") != null' >/dev/null
    echo "$STDOUT" | jq -e '.rows | map(.id) | index("disp-old-done") == null' >/dev/null

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --all
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.rows | map(.id) | index("disp-old-done") != null' >/dev/null
}

@test "sessions --json reflects external sqlite updates on subsequent reads" {
    require_jq
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.rows[0].status == "running"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].cost_usd == 1.25' >/dev/null

    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET status = 'needs-review',
    cost_usd = 9.75,
    updated_at = strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].status == "needs-review"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].cost_usd == 9.75' >/dev/null
    echo "$STDOUT" | jq -e '.summary.needs_review == 1' >/dev/null
}

@test "sessions rejects zero poll interval before entering tui" {
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --poll-interval 0s
    [ "$SPLIT_STATUS" -ne 0 ]
    [[ "$STDERR" == *"--poll-interval must be at least 250ms"* ]]
}

@test "sessions renders hostile registry values as inert text" {
    setup_sessions_data
    local sentinel="$TEST_TMPDIR/sessions-pwned"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET branch = 'branch; touch $sentinel',
    session = 'tmux; touch $sentinel',
    agent_provider = 'claude; touch $sentinel'
WHERE id = 'disp-001';
SQL

    run atc --config "$TEST_TMPDIR/atc.toml" sessions --once --all
    assert_success
    assert_output --partial "ATC Sessions"
    assert_output --partial "tmux; touch"
    assert_file_not_exists "$sentinel"
}

@test "sessions --json emits hostile registry values as inert JSON strings" {
    require_jq
    setup_sessions_data
    local sentinel="$TEST_TMPDIR/sessions-json-pwned"
    local command_payload="\$(touch $sentinel)"
    local semicolon_payload="value; touch $sentinel"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET branch = 'branch-$command_payload',
    session = 'tmux-$semicolon_payload',
    worktree_path = '$TEST_TMPDIR/worktree-$command_payload',
    log_file = '$TEST_TMPDIR/log-$command_payload.jsonl',
    agent_provider = 'claude-$semicolon_payload',
    agent_transcript_cwd = '$TEST_TMPDIR/transcript-$command_payload',
    resume_of_dispatch_id = 'source-$command_payload',
    pr_urls = '["https://example.invalid/pr?x=$command_payload","https://example.invalid/$semicolon_payload"]'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --all
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].branch | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$semicolon_payload" '.rows[0].session | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].worktree_path | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].log_file | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$semicolon_payload" '.rows[0].provider | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].transcript_cwd | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].resume_of_dispatch_id | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$command_payload" '.rows[0].pr_urls[0] | contains($payload)' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$semicolon_payload" '.rows[0].pr_urls[1] | contains($payload)' >/dev/null
    assert_file_not_exists "$sentinel"
}

@test "sessions and open-session preview treat hostile terminal locator as inert data" {
    require_jq
    setup_sessions_data
    local sentinel="$TEST_TMPDIR/terminal-locator-pwned"
    local hostile_session="\$(touch $sentinel)"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET terminal_locator_json = '{"backend":"tmux","version":1,"session":"$hostile_session","cwd":"$TEST_TMPDIR/worktree","detected_at":"2026-06-05T00:00:00Z","source":"atc-dispatch","confidence":"exact"}'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --all
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e --arg payload "$hostile_session" '.rows[0].terminal_locator.session == $payload' >/dev/null
    assert_file_not_exists "$sentinel"

    run_split atc --config "$TEST_TMPDIR/atc.toml" open-session disp-001 --json
    [ "$SPLIT_STATUS" -eq 0 ]
    echo "$STDOUT" | jq -e --arg payload "$hostile_session" '.data.session == $payload' >/dev/null
    echo "$STDOUT" | jq -e --arg payload "$hostile_session" '.data.terminal_locator.session == $payload' >/dev/null
    assert_file_not_exists "$sentinel"
}

@test "sessions --json escapes Unicode format controls in encoded bytes while preserving decoded values" {
    require_jq
    setup_sessions_data
    local bidi=$'\u202e'
    local line_sep=$'\u2028'
    local paragraph_sep=$'\u2029'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET branch = 'branch-' || char(8238) || 'gpj' || char(8232) || 'line' || char(8233) || 'para.exe',
    session = 'tmux-' || char(8238) || 'gpj' || char(8232) || 'line',
    agent_provider = 'claude-' || char(8238) || 'gpj' || char(8233) || 'para.exe'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json --all
    [ "$SPLIT_STATUS" -eq 0 ]

    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\u202e"* ]]
    [[ "$STDOUT" == *"\\u2028"* ]]
    [[ "$STDOUT" == *"\\u2029"* ]]
    local decoded_branch
    decoded_branch="$(echo "$STDOUT" | jq -r '.rows[0].branch')"
    [[ "$decoded_branch" == *"$bidi"* ]]
    [[ "$decoded_branch" == *"$line_sep"* ]]
    [[ "$decoded_branch" == *"$paragraph_sep"* ]]
}

@test "sessions --once escapes terminal control sequences in human output" {
    setup_sessions_data
    local esc=$'\033'
    local bel=$'\a'
    local osc_payload="${esc}]52;c;SGVsbG8=${bel}"
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET id = 'disp-control',
    task_slug = 'tasks/control-${esc}[2J',
    branch = 'branch-${esc}[31mred${esc}[0m',
    session = 'tmux-${esc}[31mred${esc}[0m-${osc_payload}',
    agent_provider = 'claude-${esc}[2J',
    resume_of_dispatch_id = 'source-${osc_payload}'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --once --all
    [ "$SPLIT_STATUS" -eq 0 ]

    [[ "$STDOUT" != *"$esc"* ]]
    [[ "$STDOUT" != *"$bel"* ]]
    [[ "$STDOUT" == *"\\x1b"* ]]
    [[ "$STDOUT" == *"\\x07"* ]]
}

@test "sessions --once escapes Unicode format controls in human output" {
    setup_sessions_data
    local bidi=$'\u202e'
    local line_sep=$'\u2028'
    local paragraph_sep=$'\u2029'
    sqlite3 "$TEST_TMPDIR/atc.db" <<SQL
UPDATE dispatches
SET task_slug = 'tasks/bidi-' || char(8238) || 'gpj' || char(8232) || 'line' || char(8233) || 'para.exe',
    branch = 'branch-' || char(8238) || 'gpj' || char(8232) || 'line',
    session = 'tmux-' || char(8238) || 'gpj' || char(8233) || 'para.exe',
    agent_provider = 'claude-' || char(8238) || 'gpj' || char(8232) || 'line' || char(8233) || 'para.exe'
WHERE id = 'disp-001';
SQL

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --once --all
    [ "$SPLIT_STATUS" -eq 0 ]

    [[ "$STDOUT" != *"$bidi"* ]]
    [[ "$STDOUT" != *"$line_sep"* ]]
    [[ "$STDOUT" != *"$paragraph_sep"* ]]
    [[ "$STDOUT" == *"\\u{202e}"* ]]
    [[ "$STDOUT" == *"\\u{2028}"* ]]
    [[ "$STDOUT" == *"\\u{2029}"* ]]
}
