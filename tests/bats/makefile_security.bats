#!/usr/bin/env bats
# Regression tests for tests/bats/Makefile variable handling.

load "${BATS_TEST_DIRNAME}/bats/bats-support/load"
load "${BATS_TEST_DIRNAME}/bats/bats-assert/load"
load "${BATS_TEST_DIRNAME}/bats/bats-file/load"

setup() {
    TEST_TMPDIR="$(mktemp -d -t atc-bats-makefile-XXXXXX)"
    export TEST_TMPDIR
}

teardown() {
    if [[ -d "${TEST_TMPDIR:-}" ]]; then
        rm -rf "$TEST_TMPDIR"
    fi
}

@test "make setup rejects shell metacharacters in setup lock timeout" {
    local sentinel="$TEST_TMPDIR/setup-timeout-pwned"

    run make -C "$BATS_TEST_DIRNAME" setup \
        SETUP_LOCK_TIMEOUT_SECONDS="0)); touch $sentinel; exit 97; #"

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make setup treats make functions in setup lock timeout as data" {
    local sentinel="$TEST_TMPDIR/setup-timeout-make-pwned"
    local payload="\$(shell touch $sentinel)"

    run make -C "$BATS_TEST_DIRNAME" setup \
        SETUP_LOCK_TIMEOUT_SECONDS="$payload"

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make test treats make functions in JOBS as data" {
    local sentinel="$TEST_TMPDIR/jobs-make-pwned"
    local payload="\$(shell touch $sentinel)"

    run make -C "$BATS_TEST_DIRNAME" test FILE=quick JOBS="$payload"

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make test rejects shell metacharacters in JOBS" {
    local sentinel="$TEST_TMPDIR/jobs-shell-pwned"

    run make -C "$BATS_TEST_DIRNAME" test FILE=quick \
        JOBS="1; touch $sentinel; #"

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make test validates JOBS before setup" {
    local sandbox="$TEST_TMPDIR/make-sandbox-jobs"
    mkdir -p "$sandbox"

    run make -f "$BATS_TEST_DIRNAME/Makefile" -C "$sandbox" test \
        FILE=quick JOBS="1; exit 97; #"

    assert_failure
    assert_file_not_exists "$sandbox/bats"
}

@test "make test treats make functions in FILE as data" {
    local sentinel="$TEST_TMPDIR/file-make-pwned"
    local payload="\$(shell touch $sentinel)"

    run make -C "$BATS_TEST_DIRNAME" test FILE="$payload" JOBS=1

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make test rejects shell metacharacters in FILE" {
    local sentinel="$TEST_TMPDIR/file-shell-pwned"

    run make -C "$BATS_TEST_DIRNAME" test \
        FILE="quick; touch $sentinel; #" JOBS=1

    assert_failure
    assert_file_not_exists "$sentinel"
}

@test "make test validates FILE before setup" {
    local sandbox="$TEST_TMPDIR/make-sandbox-file"
    local sentinel="$TEST_TMPDIR/file-preflight-pwned"
    mkdir -p "$sandbox"

    run make -f "$BATS_TEST_DIRNAME/Makefile" -C "$sandbox" test \
        FILE="quick; touch $sentinel; #" JOBS=1

    assert_failure
    assert_file_not_exists "$sentinel"
    assert_file_not_exists "$sandbox/bats"
}

@test "make setup treats make functions in dependency repo variables as data" {
    local sentinel="$TEST_TMPDIR/repo-make-pwned"
    local payload="\$(shell touch $sentinel)"

    run make -C "$BATS_TEST_DIRNAME" setup BATS_CORE_REPO="$payload"

    assert_success
    assert_file_not_exists "$sentinel"
}

@test "make setup treats shell metacharacters in missing dependency repo as data" {
    local sandbox="$TEST_TMPDIR/make-sandbox-repo"
    local sentinel="$TEST_TMPDIR/repo-shell-pwned"
    mkdir -p \
        "$sandbox/bats/bats-support/.git" \
        "$sandbox/bats/bats-assert/.git" \
        "$sandbox/bats/bats-file/.git"

    run make -f "$BATS_TEST_DIRNAME/Makefile" -C "$sandbox" setup \
        BATS_CORE_REPO="$TEST_TMPDIR/missing; touch $sentinel; #"

    assert_failure
    assert_file_not_exists "$sentinel"
}
