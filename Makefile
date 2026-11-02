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

# The disassembly tier's evidence, which no test here wrote, each file checked
# by its hash before it is used: SkoolKit, https://skoolkit.ca, whose
# disassembler is pure Python and runs from its source, and The Complete
# Spectrum ROM Disassembly as SkoolKit's author keeps it,
# https://github.com/skoolkid/rom, each fetched at a version of its own; and
# SOFT 968, The Amstrad 6128 Firmware Manual, as the text archive.org read
# from its scan,
# https://archive.org/details/SOFT968TheAmstrad6128FirmwareManual, a reading
# archive.org may make afresh, and its hash refuses any other. The 48K's
# firmware is the emulator's.
SKOOLKIT = https://files.pythonhosted.org/packages/source/s/skoolkit/skoolkit-10.1.tar.gz
SKOOLKIT_SHA256 = e71f0bac2bd38de1eacbb58650e95d43082351a1e9ecb8edecb11da112d02383
ROM_SKOOL = https://raw.githubusercontent.com/skoolkid/rom/20260409/sources/rom.skool
ROM_SKOOL_SHA256 = 07c1eeeeb471016a4e2090110b5717b45f8310a806f61bd60986d22f1a3a2696
SOFT968 = https://archive.org/download/SOFT968TheAmstrad6128FirmwareManual/SOFT968%20-%20The%20Amstrad%206128%20Firmware%20manual_djvu.txt
SOFT968_SHA256 = 9d295b2800abe5974617ebb3f51fb5a9866a6a30aaafe6c7e012aeb0e0efce78
SHA256SUM ?= $(shell command -v sha256sum 2>/dev/null || echo shasum -a 256)
EVIDENCE = $${CARGO_TARGET_DIR:-target}/disassembly

test-disassembly:
	$(MAKE) -C $(EMULATOR_DIR) roms
	mkdir -p "$(EVIDENCE)"
	curl -sSfL -o "$(EVIDENCE)/skoolkit.tar.gz" $(SKOOLKIT)
	curl -sSfL -o "$(EVIDENCE)/rom.skool" $(ROM_SKOOL)
	curl -sSfL -o "$(EVIDENCE)/soft968.txt" "$(SOFT968)"
	cd "$(EVIDENCE)" && printf '%s  skoolkit.tar.gz\n%s  rom.skool\n%s  soft968.txt\n' \
	  $(SKOOLKIT_SHA256) $(ROM_SKOOL_SHA256) $(SOFT968_SHA256) | $(SHA256SUM) -c
	rm -rf "$(EVIDENCE)/skoolkit" && mkdir "$(EVIDENCE)/skoolkit"
	tar -xzf "$(EVIDENCE)/skoolkit.tar.gz" -C "$(EVIDENCE)/skoolkit" --strip-components 1
	COLOPHON_MCP_DISASSEMBLY_EVIDENCE="$(EVIDENCE)" cargo test --test disassembly -- --ignored

test-all: test test-sanitized test-firmware test-disassembly test-inspector

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

# A version cut as the player's `npm version` cuts one: VERSION, past the one
# Cargo.toml gives, written into Cargo.toml and Cargo.lock and into the two pins
# the install page gives, committed under its own name and tagged, on a tree
# with nothing uncommitted. The push is git push --follow-tags.
version:
	@test -z "$$(git status --porcelain)" || { echo "the tree has changes; commit them first" >&2; exit 1; }
	@echo "$(VERSION)" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' || { echo "VERSION is '$(VERSION)'; give three numbers, as make version VERSION=0.2.0 does" >&2; exit 1; }
	@set -e; \
	old=$$(sed -n 's/^version = "\(.*\)"$$/\1/p' Cargo.toml); \
	if [ "$$old" = "$(VERSION)" ] || [ "$$(printf '%s\n%s\n' "$$old" "$(VERSION)" | sort -V | tail -n 1)" != "$(VERSION)" ]; then \
	  echo "Cargo.toml gives $$old; give a VERSION past it" >&2; exit 1; \
	fi; \
	for pin in "colophon-mcp@$$old" "download/v$$old"; do \
	  grep -qF "$$pin" docs/install.en.md || { echo "docs/install.en.md no longer reads $$pin; pin the version there again first" >&2; exit 1; }; \
	done; \
	pattern=$$(printf '%s' "$$old" | sed 's/\./\\./g'); \
	sed 's/^version = "'"$$pattern"'"$$/version = "$(VERSION)"/' Cargo.toml > Cargo.toml.new && mv Cargo.toml.new Cargo.toml; \
	sed -e 's|colophon-mcp@'"$$pattern"'|colophon-mcp@$(VERSION)|g' -e 's|download/v'"$$pattern"'|download/v$(VERSION)|g' \
	  docs/install.en.md > docs/install.en.md.new && mv docs/install.en.md.new docs/install.en.md; \
	cargo update --workspace --offline; \
	git commit -q -m "$(VERSION)" Cargo.toml Cargo.lock docs/install.en.md; \
	git tag -a "v$(VERSION)" -m "Colophon MCP $(VERSION)"; \
	echo "v$(VERSION) is tagged; git push --follow-tags publishes it"

.PHONY: all test test-inspector test-sanitized test-firmware test-disassembly test-all check format format-check lint version
