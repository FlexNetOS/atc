# BATS Integration Tests for ATC
# ===============================
#
# Usage:
#   make test          — run all tests (clones BATS libs on first run)
#   make setup         — clone BATS libraries only
#   make clean         — remove BATS libraries
#   FILE=lifecycle     — run only matching test files (A-Z, a-z, 0-9, ., _, -)
#   JOBS=8             — parallel jobs (default: 4)
#   DEBUG=1            — show atc commands being executed

.PHONY: all test setup clean preflight

SHELL := /bin/bash

BATS_CORE_REPO   := https://github.com/bats-core/bats-core.git
BATS_SUPPORT_REPO := https://github.com/bats-core/bats-support.git
BATS_ASSERT_REPO  := https://github.com/bats-core/bats-assert.git
BATS_FILE_REPO    := https://github.com/bats-core/bats-file.git

JOBS ?= 4
FILE ?=
SETUP_LOCK_TIMEOUT_SECONDS ?= 60

unexport BATS_CORE_REPO
unexport BATS_SUPPORT_REPO
unexport BATS_ASSERT_REPO
unexport BATS_FILE_REPO
unexport JOBS
unexport FILE
unexport SETUP_LOCK_TIMEOUT_SECONDS

export ATC_BATS_CORE_REPO := $(value BATS_CORE_REPO)
export ATC_BATS_SUPPORT_REPO := $(value BATS_SUPPORT_REPO)
export ATC_BATS_ASSERT_REPO := $(value BATS_ASSERT_REPO)
export ATC_BATS_FILE_REPO := $(value BATS_FILE_REPO)
export ATC_BATS_FILE_FILTER := $(value FILE)
export ATC_BATS_JOBS := $(value JOBS)
export ATC_BATS_SETUP_LOCK_TIMEOUT_SECONDS := $(value SETUP_LOCK_TIMEOUT_SECONDS)

all: test

preflight:
	@set -euo pipefail; \
	timeout="$${ATC_BATS_SETUP_LOCK_TIMEOUT_SECONDS:-60}"; \
	jobs="$${ATC_BATS_JOBS:-4}"; \
	file_filter="$${ATC_BATS_FILE_FILTER:-}"; \
	if [[ ! "$$timeout" =~ ^[0-9]+$$ ]]; then \
		echo "error: SETUP_LOCK_TIMEOUT_SECONDS must be an integer" >&2; \
		exit 2; \
	fi; \
	if (( $${#timeout} > 4 || 10#$$timeout > 3600 )); then \
		echo "error: SETUP_LOCK_TIMEOUT_SECONDS must be <= 3600" >&2; \
		exit 2; \
	fi; \
	if [[ ! "$$jobs" =~ ^[1-9][0-9]*$$ ]]; then \
		echo "error: JOBS must be a positive integer" >&2; \
		exit 2; \
	fi; \
	if (( $${#jobs} > 3 || 10#$$jobs > 128 )); then \
		echo "error: JOBS must be <= 128" >&2; \
		exit 2; \
	fi; \
	if [[ ! "$$file_filter" =~ ^[A-Za-z0-9_.-]*$$ ]]; then \
		echo "error: FILE may contain only letters, numbers, '.', '_', and '-'" >&2; \
		exit 2; \
	fi

setup: preflight
	@set -euo pipefail; \
	mkdir -p bats; \
	lock="bats/.setup.lock"; \
	timeout="$${ATC_BATS_SETUP_LOCK_TIMEOUT_SECONDS:-60}"; \
	deadline=$$((SECONDS + 10#$$timeout)); \
	while ! mkdir "$$lock" 2>/dev/null; do \
		if [[ ! -f "$$lock/pid" ]] || ! kill -0 "$$(cat "$$lock/pid" 2>/dev/null)" 2>/dev/null; then \
			rm -rf "$$lock"; \
		elif (( SECONDS >= deadline )); then \
			echo "error: timed out waiting for BATS setup lock at $$lock" >&2; \
			exit 1; \
		else \
			sleep 0.1; \
		fi; \
	done; \
	echo "$$$$" > "$$lock/pid"; \
	trap 'rm -rf "$$lock"' EXIT; \
	clone_dep() { \
		local repo="$$1" dir="$$2" tmp; \
		if [[ ! -d "$$dir/.git" ]]; then \
			tmp="$$dir.tmp.$$$$"; \
			rm -rf "$$tmp"; \
			git clone --depth 1 -- "$$repo" "$$tmp"; \
			rm -rf "$$dir"; \
			mv "$$tmp" "$$dir"; \
		fi; \
	}; \
	clone_dep "$$ATC_BATS_CORE_REPO" bats/bats-core; \
	clone_dep "$$ATC_BATS_SUPPORT_REPO" bats/bats-support; \
	clone_dep "$$ATC_BATS_ASSERT_REPO" bats/bats-assert; \
	clone_dep "$$ATC_BATS_FILE_REPO" bats/bats-file

test: setup
	@set -euo pipefail; \
	jobs="$${ATC_BATS_JOBS:-4}"; \
	file_filter="$${ATC_BATS_FILE_FILTER:-}"; \
	test_files=(); \
	while IFS= read -r test_file; do \
		test_files+=("$$test_file"); \
	done < <(find . -maxdepth 1 -name "*$$file_filter*.bats" | sort); \
	if (( $${#test_files[@]} == 0 )); then \
		echo "error: No test files matching '*$$file_filter*.bats' found" >&2; \
		exit 2; \
	fi; \
	./bats/bats-core/bin/bats --jobs "$$jobs" "$${test_files[@]}"

clean:
	rm -rf bats/
