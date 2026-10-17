# Colophon MCP

## Prologue

A book could pass through several hands before it was finished. The text was copied, the rubrics and the pictures were set in, and the copy was often corrected, sometimes against another — by the scribe who made it, or by an older monk.

The [emulator](https://github.com/tulinmola/colophon-emulator) runs the machines of the 8-bit era from the inside, the [player](https://github.com/tulinmola/colophon-player) is where a person watches one run, and the [archive](https://github.com/tulinmola/colophon-archive) is where what was found is written down. This is where another reader is let in: an agent, which has no eyes on the glass and no fingers on the keys, and so asks the machine for all of it — to run, to stop, to type, to show its picture and what it holds. It asks through the [Model Context Protocol](https://modelcontextprotocol.io), and it may stand up as many machines at once as its question needs, so that a game's releases for two machines can be read side by side.

What it finds is not the record. A finding is to reach the archive as something the machine can be asked again — the copy it was read from, named by its hash, the build it ran on, and what was done to it and at which tick — and since nothing in the machine consults a clock, the same questions will get the same answers for whoever asks them next.

The protocol will change and the machines will not. That is why it is spoken here and not in the emulator.

## Building

Building it takes Rust through rustup, which fetches the toolchain `rust-toolchain.toml` pins on first use, a C compiler, libclang for bindgen to read the emulator's headers with, and a checkout of [the emulator](https://github.com/tulinmola/colophon-emulator) standing beside this one; `EMULATOR_DIR` names another.

```sh
make         # build
make test    # the tests
make check   # formatting, linting and the tests
```

`make check` also wants clang-format and clang-tidy. The check workflow pins the release it runs, because another may format differently or find other things.

## Where it stands

No agent can reach a machine yet. A CPC and a Spectrum are built here from the emulator's own sources, and the tests run them by the frame and to a limit in ticks; that is all. The first tools will stand machines up, list them and put them away, run them by the frame and step them by the instruction, stop them where they are told, type at them, read and set anything they hold by its path, read and write their memory and disassemble it, show their picture, and keep them as they stand to be taken up again. The rest are to be learned from the colophons that ask for them.

## Sources

Every source is cited where the code uses it; this list is the other view, and what each one gives us.

- [Z80 CPU User Manual](https://www.zilog.com/docs/z80/um0080.pdf) (Zilog UM0080) — how long an instruction takes where nothing holds the processor's clock, which grades where a run stopped by its limit ends.
- ["The Amstrad CPC CRTC Compendium" v1.11](https://shaker.logonsystem.eu/ACCC1.11-EN.pdf) (Longshot / Logon System) — the standard frame of 312 lines of 64µs, the 4MHz the Gate Array clocks the processor at, and the address of the firmware's table the frame is programmed from, which together grade a CPC run by the frame. Technical information sourced from the "Amstrad CPC CRTC Compendium" by Longshot (CC BY-NC-ND).
- [libspectrum's `timings.c`](https://sourceforge.net/p/fuse-emulator/libspectrum/ci/master/tree/timings.c) (Philip Kendall, Fuse) — the 48K frame, 224 T-states to a line and 312 lines, which grades a Spectrum run by the frame.
- ["Contended memory"](https://sinclair.wiki.zxnet.co.uk/wiki/Contended_memory) (Sinclair Wiki) — the IR pair standing on the bus through an instruction's internal T-states, and charged there, which lets a test make the processor finish before the machine does.

## License

MIT, like the rest of Colophon.
