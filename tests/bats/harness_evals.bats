#!/usr/bin/env bats

load helpers/common

# Opt-in external harness evals.
# These use real Codex / Claude CLIs with a fake `atc` shim first on PATH and
# score behavior by the observed `atc` command, not by natural-language output.

@test "codex explicit atc-dispatch skill runs the correct PR review dry-run" {
    skip_unless_harness_evals_enabled
    require_codex_auth
    setup_harness_eval_workspace "agents"
    install_fake_atc

    run codex exec \
        --skip-git-repo-check \
        --dangerously-bypass-approvals-and-sandbox \
        --ignore-user-config \
        --ignore-rules \
        --ephemeral \
        'Use $atc-dispatch to dry-run the correct ATC command for PR review of https://github.com/acme/widgets/pull/123, then stop.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    grep -Eq '^run pr-review --param pr=https://github\.com/acme/widgets/pull/123 --dry-run ?$' "$ATC_EVAL_LOG"
}

@test "codex explicit atc-monitor skill reads logs for a dispatch" {
    skip_unless_harness_evals_enabled
    require_codex_auth
    setup_harness_eval_workspace "agents"
    install_fake_atc

    run codex exec \
        --skip-git-repo-check \
        --dangerously-bypass-approvals-and-sandbox \
        --ignore-user-config \
        --ignore-rules \
        --ephemeral \
        'Use $atc-monitor to inspect dispatch disp-123 by reading its logs, then stop.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    grep -Eq '^logs disp-123 ?$' "$ATC_EVAL_LOG"
}

@test "codex implicit dispatch request uses the PR review template shape" {
    skip_unless_harness_evals_enabled
    require_codex_auth
    setup_harness_eval_workspace "agents"
    install_fake_atc

    run codex exec \
        --skip-git-repo-check \
        --dangerously-bypass-approvals-and-sandbox \
        --ignore-user-config \
        --ignore-rules \
        --ephemeral \
        'Please dry-run the correct ATC command to review PR https://github.com/acme/widgets/pull/123, then stop.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    grep -Eq '^run pr-review --param pr=https://github\.com/acme/widgets/pull/123 --dry-run ?$' "$ATC_EVAL_LOG"
}

@test "codex reference-only request does not invoke atc" {
    skip_unless_harness_evals_enabled
    require_codex_auth
    setup_harness_eval_workspace "agents"
    install_fake_atc

    run codex exec \
        --skip-git-repo-check \
        --dangerously-bypass-approvals-and-sandbox \
        --ignore-user-config \
        --ignore-rules \
        --ephemeral \
        'What ATC command should I use for PR review of https://github.com/acme/widgets/pull/123? Do not run anything.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    [ ! -s "$ATC_EVAL_LOG" ]
}

@test "claude explicit atc-monitor skill reads logs for a dispatch" {
    skip_unless_harness_evals_enabled
    require_claude_auth
    setup_harness_eval_workspace "claude"
    install_fake_atc

    run claude -p --bare --dangerously-skip-permissions \
        'Use /atc-monitor to inspect dispatch disp-123 by reading its logs, then stop.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    grep -Eq '^logs disp-123 ?$' "$ATC_EVAL_LOG"
}

@test "claude explicit atc-dispatch skill runs the correct PR review dry-run" {
    skip_unless_harness_evals_enabled
    require_claude_auth
    setup_harness_eval_workspace "claude"
    install_fake_atc

    run claude -p --bare --dangerously-skip-permissions \
        'Use /atc-dispatch to dry-run the correct ATC command for PR review of https://github.com/acme/widgets/pull/123, then stop.'

    assert_success
    assert_file_exist "$ATC_EVAL_LOG"
    grep -Eq '^run pr-review --param pr=https://github\.com/acme/widgets/pull/123 --dry-run ?$' "$ATC_EVAL_LOG"
}
