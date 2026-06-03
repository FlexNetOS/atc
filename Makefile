# ATC Makefile
# ============
# Common commands for Rust development

.PHONY: all help build release test test-bats check clippy fmt fmt-check clean watch watch-test doc doc-open install uninstall install-hooks ci

# Default: alias 'all' to 'build'
all: build

# Default target
help:
	@echo "ATC Development Commands"
	@echo "========================"
	@echo ""
	@echo "Development:"
	@echo "  make build      - Build all crates"
	@echo "  make test       - Run Rust tests"
	@echo "  make test-bats  - Run BATS integration tests"
	@echo "  make check      - Run cargo check"
	@echo "  make clippy     - Run clippy lints"
	@echo "  make fmt        - Format code"
	@echo "  make fmt-check  - Check formatting without modifying"
	@echo "  make watch      - Watch and rebuild on changes"
	@echo ""
	@echo "Installation:"
	@echo "  make install       - Install atc to ~/.cargo/bin"
	@echo "  make uninstall     - Remove atc from ~/.cargo/bin"
	@echo "  make install-hooks - Install commit-msg, pre-commit, and pre-push hooks"
	@echo ""
	@echo "Documentation:"
	@echo "  make doc        - Generate documentation"
	@echo "  make doc-open   - Generate and open documentation"
	@echo ""
	@echo "CI:"
	@echo "  make ci         - Run all CI checks"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean      - Remove build artifacts"

# =============================================================================
# Development
# =============================================================================

# Build all workspace members
build:
	cargo build --workspace

# Build in release mode
release:
	cargo build --workspace --release

# Run tests (Rust unit + integration)
test:
	cargo test --workspace

# Run BATS integration tests (auto-clones bats-core on first run)
test-bats: build
	$(MAKE) -C tests/bats test

# Run cargo check
check:
	cargo check --workspace --all-targets

# Run clippy
clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting without modifying
fmt-check:
	cargo fmt --all -- --check

# Watch and rebuild on changes
watch:
	cargo watch -x "build --workspace"

# Watch and run tests on changes
watch-test:
	cargo watch -x "test --workspace"

# =============================================================================
# Documentation
# =============================================================================

# Generate documentation
doc:
	cargo doc --workspace --no-deps

# Generate and open documentation
doc-open:
	cargo doc --workspace --no-deps --open

# =============================================================================
# CI
# =============================================================================

# Run all CI checks (same as GitHub Actions)
ci: fmt-check clippy check test
	@echo "All CI checks passed!"

# =============================================================================
# Installation
# =============================================================================

# Install atc binary to ~/.cargo/bin
install:
	cargo install --path crates/atc-cli
	@echo ""
	@echo "atc installed! Make sure ~/.cargo/bin is in your PATH."
	@echo "Try: atc --help"

# Uninstall atc binary
uninstall:
	cargo uninstall atc-cli 2>/dev/null || true
	@echo "atc uninstalled."

# Install git hooks (commit-msg validates release-safe subjects; pre-push runs CI checks locally)
# Uses git rev-parse to handle worktrees and submodules correctly
install-hooks:
	@echo "Installing git hooks..."
	@if command -v npm >/dev/null 2>&1; then \
		echo "Installing pinned hook dependencies..."; \
		npm ci --ignore-scripts; \
	else \
		echo "Warning: npm not found; commit-msg hook will skip local commitlint until dependencies are installed."; \
	fi
	@chmod +x .githooks/commit-msg
	@chmod +x scripts/hooks/pre-push
	@chmod +x .githooks/pre-commit
	@mkdir -p "$$(git rev-parse --git-path hooks)"
	@ln -sf "$$(pwd)/.githooks/commit-msg" "$$(git rev-parse --git-path hooks)/commit-msg"
	@ln -sf "$$(pwd)/scripts/hooks/pre-push" "$$(git rev-parse --git-path hooks)/pre-push"
	@ln -sf "$$(pwd)/.githooks/pre-commit" "$$(git rev-parse --git-path hooks)/pre-commit"
	@echo "Commit-msg, pre-commit, and pre-push hooks installed. Commit subjects and CI checks will run locally."

# =============================================================================
# Cleanup
# =============================================================================

# Remove build artifacts
clean:
	cargo clean
