#!/usr/bin/env bats

load helpers/common

@test "atc init scaffolds Codex skill folders" {
    cd "$TEST_TMPDIR"

    run atc init --no-interactive
    assert_success

    assert_file_exist ".atc/skills/atc-reference.md"
    assert_file_exist ".atc/skills/dispatch.md"
    assert_file_exist ".atc/skills/monitor.md"
    assert_file_exist ".atc/skills/dispatch/SKILL.md"
    assert_file_exist ".atc/skills/dispatch/agents/openai.yaml"
    assert_file_exist ".atc/skills/monitor/SKILL.md"
    assert_file_exist ".atc/skills/monitor/agents/openai.yaml"
}

@test "atc init agents creates a symlink to nested skills" {
    cd "$TEST_TMPDIR"
    mkdir -p .agents/skills

    run atc init --no-interactive
    assert_success

    run atc init agents
    assert_success

    assert_file_exist ".agents/skills/atc/dispatch/SKILL.md"
    [ -L ".agents/skills/atc" ]
    [ "$(readlink .agents/skills/atc)" = "../../.atc/skills" ]
}

@test "atc init agents --copy preserves nested skills" {
    cd "$TEST_TMPDIR"
    mkdir -p .agents/skills

    run atc init --no-interactive
    assert_success

    run atc init agents --copy
    assert_success

    assert_dir_exist ".agents/skills/atc"
    assert_file_exist ".agents/skills/atc/dispatch/SKILL.md"
    assert_file_exist ".agents/skills/atc/dispatch/agents/openai.yaml"
    assert_file_exist ".agents/skills/atc/monitor/SKILL.md"
    assert_file_exist ".agents/skills/atc/monitor/agents/openai.yaml"
}
