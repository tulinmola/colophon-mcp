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

# The tests where the emulator's own test-sanitized looks: its C built with
# AddressSanitizer and UBSan, and the sweep of every value `set` could write run
# among them. rustc links without the C compiler's default libraries, which
# are where the sanitizers' runtimes come from, so it is asked to keep them.
# The tests are built for the host named outright, so that the build script
# and the proc macros are not sanitized with them. A trailing array is bounds
# checked like any other only under -fstrict-flex-arrays=3, and the disc
# controller ends with one.
SANITIZERS = -fsanitize=address,undefined -fno-sanitize-recover=all -fstrict-flex-arrays=3

test-sanitized:
	CC="$(CC)" CFLAGS="$(SANITIZERS)" \
	  RUSTFLAGS="$(RUSTFLAGS) -C default-linker-libraries=yes -C link-arg=-fsanitize=address,undefined" \
	  cargo test --target "$$(rustc -vV | sed -n 's/host: //p')" \
	  --target-dir "$${CARGO_TARGET_DIR:-target}/sanitized" --lib -- --include-ignored

# The firmware tier: the server running the firmware the emulator fetches,
# held against the emulator's own command line, both made there if they are
# not yet.
test-firmware:
	$(MAKE) -C $(EMULATOR_DIR) roms build/emulator
	cargo test --test protocol -- --ignored

test-all: test test-sanitized test-firmware test-inspector

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

.PHONY: all test test-inspector test-sanitized test-firmware test-all check format format-check lint
