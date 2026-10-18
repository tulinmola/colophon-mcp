EMULATOR_DIR ?= ../colophon-emulator

# A client this repository did not write, pinned: the protocol's own
# Inspector, https://github.com/modelcontextprotocol/inspector, run through
# npx, which wants Node 22.19 or later.
INSPECTOR = @modelcontextprotocol/inspector@2.11.0

CLANG_FORMAT ?= $(shell command -v clang-format 2>/dev/null || echo xcrun clang-format)
CLANG_TIDY ?= $(shell command -v clang-tidy 2>/dev/null || command -v /opt/homebrew/opt/llvm/bin/clang-tidy 2>/dev/null || echo clang-tidy)

# Taken by wildcard so a file cannot be added and left out of the formatter
# or the linter.
BRIDGE_SOURCES = $(wildcard emulator/*.c)
BRIDGE_HEADERS = $(wildcard emulator/*.h)

all:
	cargo build --release

test:
	cargo test

test-inspector:
	cargo build
	sh tests/inspector.sh "$${CARGO_TARGET_DIR:-target}/debug/colophon-mcp" $(INSPECTOR)

test-all: test test-inspector

check: format-check lint test

format:
	cargo fmt
	$(CLANG_FORMAT) -i $(BRIDGE_SOURCES) $(BRIDGE_HEADERS)

format-check:
	cargo fmt --check
	$(CLANG_FORMAT) --dry-run --Werror $(BRIDGE_SOURCES) $(BRIDGE_HEADERS)

lint:
	cargo clippy --all-targets -- --deny warnings
	$(CLANG_TIDY) $(BRIDGE_SOURCES) -- -std=c99 -Wall -Wextra -I$(EMULATOR_DIR)/src

.PHONY: all test test-inspector test-all check format format-check lint
