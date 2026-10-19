# Colophon MCP

## Prologue

A book could pass through several hands before it was finished. The text was copied, the rubrics and the pictures were set in, and the copy was often corrected, sometimes against another — by the scribe who made it, or by an older monk.

The [emulator](https://github.com/tulinmola/colophon-emulator) runs the machines of the 8-bit era from the inside, the [player](https://github.com/tulinmola/colophon-player) is where a person watches one run, and the [archive](https://github.com/tulinmola/colophon-archive) is where what was found is written down. This is where another reader is let in: an agent, which has no eyes on the glass and no fingers on the keys, and so asks the machine for all of it — to run, to stop, to type, to show its picture and what it holds. It asks through the [Model Context Protocol](https://modelcontextprotocol.io), and it may stand up as many machines at once as its question needs, so that a game's releases for two machines can be read side by side.

What it finds is not the record. A finding is to reach the archive as something the machine can be asked again — the copy it was read from, named by its hash, the build it ran on, and what was done to it and at which tick — and since nothing in the machine consults a clock, the same questions will get the same answers for whoever asks them next.

The protocol will change and the machines will not. That is why it is spoken here and not in the emulator.

## Building

Building it takes Rust through rustup, which fetches the toolchain `rust-toolchain.toml` pins on first use, a C compiler, libclang for bindgen to read the emulator's headers with, and a checkout of [the emulator](https://github.com/tulinmola/colophon-emulator) standing beside this one; `EMULATOR_DIR` names another.

```sh
make                  # build
make test             # the tests
make check            # formatting, linting and the tests
make test-sanitized   # the tests under AddressSanitizer and UBSan
make test-firmware    # the firmware, against the emulator's command line
make test-inspector   # the server against the protocol's own Inspector
```

`make check` also wants clang-format and clang-tidy. The check workflow pins the release it runs, because another may format differently or find other things. `make test-sanitized` wants a C compiler with AddressSanitizer and UBSan. `make test-firmware` has the emulator fetch its firmware, by the hashes it pins, and build its command line. `make test-inspector` fetches the Inspector through npx, which wants Node 22.19 or later.

## Where it stands

An agent can run the machines, stop them and watch them, and read them, but cannot yet see them or reach them with anything but a snapshot. The server answers it over standard input and output, in the protocol's revisions 2026-07-28, 2025-11-25 and 2025-06-18, and names the build in its answers about itself and its tools. It stands up the machines the emulator's command line names, from power-on or from a snapshot, and names every file it built one from by its hash; it runs them by the frame or the tick and steps them by the instruction, saying how far each request took them, stops them where it is told and gathers what they do on the way, and calls their routines; it reads and writes whatever they hold by its path, refusing what the emulator could not run from, and reads and writes their memory as their processors would. [The server](docs/index.en.md) is the full account. The tools still to come will show a machine's picture and read its memory as one, disassemble it, type at it, give it its discs and its tapes, and keep it as it stands to be taken up again. The rest are to be learned from the colophons that ask for them.

## Sources

Every source is cited where the code uses it; this list is the other view, and what each one gives us.

- [Z80 CPU User Manual](https://www.zilog.com/docs/z80/um0080.pdf) (Zilog UM0080) — how long an instruction takes where nothing holds the processor's clock, how a halted processor idles and what answering an interrupt costs, which grade where a run stopped by its limit ends and what a step runs.
- ["Timings"](https://cpctech.cpcwiki.de/docs/instrtim.html) (Kevin Thacker's cpctech) — every instruction's measured length on a CPC, in whole microseconds, which grades a CPC's steps.
- ["Snapshot (.SNA) file format"](https://cpctech.cpcwiki.de/docs/snapshot.html) (Kevin Thacker's cpctech) — the CPC snapshot's header, field by field, by which the tests write snapshots and read the command line's.
- ["Ram management"](https://cpctech.cpcwiki.de/docs/rampage.html) (Kevin Thacker's cpctech) — the RAM configurations of a 6128, which grade a write to the register that selects them.
- ["Emulator file formats"](https://worldofspectrum.org/faq/reference/formats.htm) (World of Spectrum FAQ) — the 48K .sna, its header and its program counter on the stack, by which the tests write a Spectrum's snapshots and read the command line's.
- [ZX Spectrum BASIC programming, chapter 25](https://worldofspectrum.org/ZXBasicManual/zxmanchap25.html) (Sinclair Research) — the system variables P-RAMT and FRAMES, which grade a Spectrum started on its own firmware.
- ["The Gate Array"](https://web.archive.org/web/20260710174425/https://www.grimware.org/doku.php/documentations/devices/gatearray) (Grim) — a CPC's ROMs answering reads while writes fall through to the RAM beneath them, and the register that pages them and sets the mode, which grade memory read and written as the processor would and the byte a snapshot keeps that register in.
- ["The Undocumented Z80 Documented"](http://www.z80.info/zip/z80-documented.pdf) (Sean Young) — AF and SP at FFFFh after a reset, which grades a machine read as it stands up.
- [The protocol's TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk/blob/v2.3.1/packages/core-internal/src/shared/protocol.ts#L103) — the 60 seconds its clients wait for an answer by default, which bound how far one request may run a machine.
- [Claude Code's MCP documentation](https://code.claude.com/docs/en/mcp) — the 10000 tokens of an answer at which Claude Code warns, which bound how much memory one peek reads and how many matches one run gathers.
- [The Complete Spectrum ROM Disassembly](https://skoolkid.github.io/rom/) (Ian Logan and Frank O'Hara, as SkoolKit gives it) — where the 48K ROM's interrupt handler writes the frame count, and what PIXEL-ADD takes and returns, which grade a write named for its instruction and a routine called.
- ["16K/48K reference"](https://worldofspectrum.org/faq/reference/48kreference.htm) (comp.sys.sinclair FAQ) — every even port addressing the ULA, and the first byte of the screen at 16384, which grade a port matched on its decoding and a routine called.
- ["The CRTC"](https://web.archive.org/web/20240924035112/https://www.grimware.org/doku.php/documentations/devices/crtc) (Grim) — the 6845 answering A14 low, with A8 its RS and A9 its R/W, and its register index five bits wide, which grade the writes gathered from its ports.
- [NIST's SHA-256 examples](https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Standards-and-Guidelines/documents/examples/SHA256.pdf) — the digests of two messages worked through, which fix what the sha2 crate gives a file's hash.
- ["The Amstrad CPC CRTC Compendium" v1.11](https://shaker.logonsystem.eu/ACCC1.11-EN.pdf) (Longshot / Logon System) — the standard frame of 312 lines of 64µs, the 4MHz the Gate Array clocks the processor at, the address of the firmware's table the frame is programmed from, and the Gate Array's interrupt every 52 lines, which together grade a CPC run by the frame, a CPC started on its own firmware, and the interrupts a frame holds. Technical information sourced from the "Amstrad CPC CRTC Compendium" by Longshot (CC BY-NC-ND).
- [libspectrum's `timings.c`](https://sourceforge.net/p/fuse-emulator/libspectrum/ci/master/tree/timings.c) (Philip Kendall, Fuse) — the 48K frame, 224 T-states to a line and 312 lines, which grades a Spectrum run by the frame.
- [The Model Context Protocol specification](https://modelcontextprotocol.io/specification/2026-07-28), revisions 2026-07-28, 2025-11-25 and 2025-06-18 — what a server must answer and how: the revisions and their refusal, the server's name, the shape of a tool's answer and of its errors, and what may reach standard output, quoted in the tests that grade each.
- [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) — build metadata, set after a plus and left out of a version's precedence, which carries the commits a build is named for on the package's own version.
- [The MCP Inspector](https://github.com/modelcontextprotocol/inspector) 2.11.0 — the protocol's own client, which the Inspector tier runs against the server in both eras, so that a client this repository did not write agrees with its tests.
- ["Contended memory"](https://sinclair.wiki.zxnet.co.uk/wiki/Contended_memory) (Sinclair Wiki) — the IR pair standing on the bus through an instruction's internal T-states, and charged there, which lets a test make the processor finish before the machine does.

## License

MIT, like the rest of Colophon.
