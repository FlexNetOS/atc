#!/usr/bin/env bash
# Common helpers for ATC BATS tests.
#
# BATS_TEST_DIRNAME = directory of the .bats file = tests/bats/
# Repo root is two levels up from there.

_repo_root() {
    cd "$BATS_TEST_DIRNAME/../.." && pwd
}

# Build the atc binary once per test file (not per test).
setup_file() {
    local root
    root="$(_repo_root)"
    export ATC_BIN="$root/target/debug/atc"
    cargo build --manifest-path "$root/Cargo.toml" --quiet
}

# Per-test setup: create a temp directory for config/state.
setup() {
    TEST_TMPDIR="$(mktemp -d)"
    export TEST_TMPDIR
    export ATC_BIN="${ATC_BIN:-$(_repo_root)/target/debug/atc}"
}

# Per-test teardown: clean up temp directory.
teardown() {
    if [[ -d "$TEST_TMPDIR" ]]; then
        rm -rf "$TEST_TMPDIR"
    fi
}

# Write a minimal valid config file to $1.
write_test_config() {
    local config_file="$1"
    local db_path="${2:-$TEST_TMPDIR/atc.db}"
    cat > "$config_file" <<EOF
[dispatch]
repo = "core"
meta_workspace_root = "$TEST_TMPDIR/workspace"

[registry]
path = "$db_path"
EOF
}
