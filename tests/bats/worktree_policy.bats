#!/usr/bin/env bats
# Tests for worktree routing policy (worktree: field in template frontmatter).
# No external dependencies (claude, git-kb, tmux, meta) required —
# these test dry-run output to verify policy routing behavior.

load helpers/common

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Create a minimal .atc/ directory with templates that declare worktree policies.
setup_atc_dir() {
    mkdir -p "$TEST_TMPDIR/.atc/templates" "$TEST_TMPDIR/.atc/components" "$TEST_TMPDIR/.atc/partials"

    # swot: worktree: none
    cat > "$TEST_TMPDIR/.atc/templates/swot.md" <<'EOF'
---
description: Deep SWOT analysis
directive: research
worktree: none
required_params: [competitor, name]
---
SWOT for {{competitor}} ({{name}}).
EOF

    # close: worktree: document
    cat > "$TEST_TMPDIR/.atc/templates/close.md" <<'EOF'
---
description: Close a task
directive: close
worktree: document
required_params: [task]
---
Close {{task}}.
EOF

    # branch-review: worktree: current
    cat > "$TEST_TMPDIR/.atc/templates/branch-review.md" <<'EOF'
---
description: Local review
directive: review-fix
worktree: current
---
Review branch.
EOF

    # pr-review: worktree: branch (explicit)
    cat > "$TEST_TMPDIR/.atc/templates/pr-review.md" <<'EOF'
---
description: PR review
directive: review-fix
worktree: branch
required_params: [pr]
---
Review {{pr}}.

{{prefetch}}
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
# worktree: none — swot should show no worktree creation
# ---------------------------------------------------------------------------

@test "atc run swot --dry-run shows worktree: none" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    run atc run swot --param competitor=Acme --param name=Test --dry-run
    assert_success
    assert_output --partial "Worktree:    none"
    assert_output --partial "tpl--none--swot"
}

@test "atc run swot --dry-run branch is stable (no synthetic timestamp)" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    run atc run swot --param competitor=Acme --param name=Test --dry-run
    assert_success
    # Branch should be tpl--none--swot, not tpl--swot-<timestamp>
    assert_output --partial "Branch:      tpl--none--swot"
}

# ---------------------------------------------------------------------------
# worktree: document — close should show document routing
# ---------------------------------------------------------------------------

@test "atc run close --dry-run shows worktree: document" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    run atc run close --param task=tasks/harmony-350 --dry-run
    assert_success
    assert_output --partial "Worktree:    document"
}

# ---------------------------------------------------------------------------
# worktree: current — branch-review uses current branch
# ---------------------------------------------------------------------------

@test "atc run push-branch --dry-run shows worktree: current" {
    setup_atc_dir
    # Also write push-branch template with worktree: current
    cat > "$TEST_TMPDIR/.atc/templates/push-branch.md" <<'EOF'
---
description: Push branch
directive: implement
worktree: current
---
Push branch.
EOF
    cd "$TEST_TMPDIR"
    # Init a git repo so current branch detection works
    git init --quiet "$TEST_TMPDIR"
    git -C "$TEST_TMPDIR" config user.email "test@test.com"
    git -C "$TEST_TMPDIR" config user.name "Test"
    git -C "$TEST_TMPDIR" checkout -b feature-branch --quiet 2>/dev/null || true
    echo "x" > "$TEST_TMPDIR/f.txt"
    git -C "$TEST_TMPDIR" add f.txt
    git -C "$TEST_TMPDIR" commit -m "init" --quiet

    run atc run push-branch --dry-run
    assert_success
    assert_output --partial "Worktree:    current"
    # Should use the actual branch, not synthetic
    assert_output --partial "Branch:      feature-branch"
}

@test "atc run push-branch --dry-run does not create synthetic branch" {
    setup_atc_dir
    cat > "$TEST_TMPDIR/.atc/templates/push-branch.md" <<'EOF'
---
description: Push branch
directive: implement
worktree: current
---
Push branch.
EOF
    cd "$TEST_TMPDIR"
    git init --quiet "$TEST_TMPDIR"
    git -C "$TEST_TMPDIR" config user.email "test@test.com"
    git -C "$TEST_TMPDIR" config user.name "Test"
    git -C "$TEST_TMPDIR" checkout -b my-branch --quiet 2>/dev/null || true
    echo "x" > "$TEST_TMPDIR/f.txt"
    git -C "$TEST_TMPDIR" add f.txt
    git -C "$TEST_TMPDIR" commit -m "init" --quiet

    run atc run push-branch --dry-run
    assert_success
    # Must NOT contain tpl-- prefix
    refute_output --partial "tpl--"
}

# ---------------------------------------------------------------------------
# worktree: branch — pr-review preserves current behavior
# ---------------------------------------------------------------------------

@test "atc run pr-review --dry-run shows worktree: branch" {
    setup_atc_dir
    cd "$TEST_TMPDIR"
    # pr-review requires pr param — use a dummy URL (dry-run won't call gh)
    run atc run pr-review --param pr=https://github.com/org/repo/pull/42 --dry-run
    assert_success
    assert_output --partial "Worktree:    branch"
}
