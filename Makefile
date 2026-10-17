EMULATOR_DIR ?= ../colophon-emulator

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

.PHONY: all test check format format-check lint
