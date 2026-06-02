# BATS Integration Tests for ATC
# ===============================
#
# Usage:
#   make test          — run all tests (clones BATS libs on first run)
#   make setup         — clone BATS libraries only
#   make clean         — remove BATS libraries
#   FILE=lifecycle     — run only matching test files
#   JOBS=8             — parallel jobs (default: 4)
#   DEBUG=1            — show atc commands being executed

.PHONY: all test setup clean

SHELL := /bin/bash

BATS_CORE_REPO   := https://github.com/bats-core/bats-core.git
BATS_SUPPORT_REPO := https://github.com/bats-core/bats-support.git
BATS_ASSERT_REPO  := https://github.com/bats-core/bats-assert.git
BATS_FILE_REPO    := https://github.com/bats-core/bats-file.git

BATS := ./bats/bats-core/bin/bats
JOBS ?= 4
FILE ?=

# Find test files, optionally filtered by FILE pattern
TEST_FILES := $(shell find . -maxdepth 1 -name '*$(FILE)*.bats' | sort)

ifeq ($(TEST_FILES),)
$(error No test files matching '*$(FILE)*.bats' found)
endif

all: test

setup:
	@mkdir -p bats
	@lock="bats/.setup.lock"; \
	while ! mkdir "$$lock" 2>/dev/null; do \
		if [[ ! -f "$$lock/pid" ]] || ! kill -0 "$$(cat "$$lock/pid" 2>/dev/null)" 2>/dev/null; then \
			rm -rf "$$lock"; \
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
			git clone --depth 1 "$$repo" "$$tmp"; \
			rm -rf "$$dir"; \
			mv "$$tmp" "$$dir"; \
		fi; \
	}; \
	clone_dep "$(BATS_CORE_REPO)" bats/bats-core; \
	clone_dep "$(BATS_SUPPORT_REPO)" bats/bats-support; \
	clone_dep "$(BATS_ASSERT_REPO)" bats/bats-assert; \
	clone_dep "$(BATS_FILE_REPO)" bats/bats-file

test: setup
	$(BATS) --jobs $(JOBS) $(TEST_FILES)

clean:
	rm -rf bats/
