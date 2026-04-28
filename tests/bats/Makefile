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
#   ATC_RUN_HARNESS_EVALS=1 FILE=harness_evals
#                      — run opt-in Codex/Claude harness evals

.PHONY: all test setup clean

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
	@[[ -d bats/bats-core ]]    || git clone --depth 1 $(BATS_CORE_REPO)    bats/bats-core
	@[[ -d bats/bats-support ]]  || git clone --depth 1 $(BATS_SUPPORT_REPO)  bats/bats-support
	@[[ -d bats/bats-assert ]]   || git clone --depth 1 $(BATS_ASSERT_REPO)   bats/bats-assert
	@[[ -d bats/bats-file ]]     || git clone --depth 1 $(BATS_FILE_REPO)     bats/bats-file

test: setup
	$(BATS) --jobs $(JOBS) $(TEST_FILES)

clean:
	rm -rf bats/
