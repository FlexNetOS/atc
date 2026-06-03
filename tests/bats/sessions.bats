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
    agent_capabilities_json = '{"supports_resume_by_session_id":true,"supports_explicit_session_id_on_start":true,"supports_tmux_attach":true,"supports_tmux_redirect":true,"supports_stream_json_output":true,"supports_cost_and_turn_reporting":true}'
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

@test "sessions --json emits session rows and capability action state" {
    require_jq
    setup_sessions_data

    run_split atc --config "$TEST_TMPDIR/atc.toml" sessions --json
    [ "$SPLIT_STATUS" -eq 0 ]

    echo "$STDOUT" | jq -e '.schema_version == 1' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].id == "disp-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].task_slug == "tasks/test-1"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].work_unit_id == "wu-001"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].provider == "claude"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].provider_session_id == "00000000-0000-4000-8000-000000000794"' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].pr_urls == ["https://github.com/org/repo/pull/42","https://github.com/org/api/pull/7"]' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.redirect.enabled == true' >/dev/null
    echo "$STDOUT" | jq -e '.rows[0].actions.resume.enabled == false' >/dev/null
    echo "$STDOUT" | jq -e '.work_units[0].id == "wu-001"' >/dev/null
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
