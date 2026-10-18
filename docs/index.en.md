---
title: The MCP server
description: The machines handed to an agent over the Model Context Protocol — stood up by name or from a snapshot, run, stepped, read and written.
---

An agent has no eyes on the glass and no fingers on the keys, and what it learns of a machine is what it asks for. This server is where it asks. It speaks the [Model Context Protocol](https://modelcontextprotocol.io) over its standard input and output, to one client at a time, and holds as many machines as that client stands up, each behind a handle of its own.

## Starting it

`make` builds `target/release/colophon-mcp`, and the server is started with the directory its firmware is read from:

```sh
colophon-mcp --roms ../colophon-emulator/roms
```

The directory holds the firmware as the emulator's `make roms` fetches it: `cpc6128.rom`, `cpc664.rom` and `cpc464.rom`, each the operating system followed by BASIC; `amsdos.rom`, for the machines with a disc interface; and `spectrum48.rom`. A file is read when a machine that needs it is opened, so a missing one is reported then, by name, and nothing else is refused for it. A file must be the size of the firmware it is named for, where the emulator's command line takes the first bytes of a longer one.

A client is given the same command line, which it starts and speaks to. With Claude Code:

```sh
claude mcp add colophon -- /path/to/colophon-mcp/target/release/colophon-mcp --roms /path/to/colophon-emulator/roms
```

## What it answers

A request that names its revision in its `_meta`, as the 2026-07-28 revision has every request do, is answered in the revision it names, `server/discover` among them. A client that opens with `initialize` instead is answered in 2025-11-25 or 2025-06-18, as it asks, and in 2025-11-25 if it asks for any other. A request in a revision the server does not speak is refused with the ones it does.

`machine_open` builds a machine by the name the emulator's command line gives it, and wires it as the command line does: the same RAM, the same firmware in the same sockets, and the disc interface where the machine has one. Given a snapshot, it loads it as the command line does, into the machine named, once the firmware is fitted. `machine_list` names the machines standing, and `machine_close` puts one away once what it was asked before is done. `run` and `step` move a machine, `inspect` and `set` read and write what it holds, and `peek` and `poke` its memory. Each tool's description is the reference for it; an agent reads them from the server.

## Time

Nothing but `run` and `step` moves a machine, and each answers with the ticks it took, the frames the monitor retraced in them, and the machine's tick since it was stood up: from power-on, or from its snapshot. The same calls on the same machine give the same answers.

A run goes by frames or by ticks. A frame is the monitor's and ends as its retrace sends the beam to the top-left corner, so a run by frames leaves a whole picture on the monitor; a CPC's frame is whatever its 6845 is programmed to make and may never come, so such a run is held to twice a standard frame's ticks for each frame asked. A run by ticks runs that many, which is how the emulator's command line counts its frames: 79872 ticks to a CPC's, 69888 to a Spectrum's. A step runs instructions, each to its end and through the response to an interrupt taken there, so a step can land in a handler. A run or a step ends between instructions, a few ticks past what was asked if the instruction standing there runs on, unless it ends inside a chain of prefixes, which is no instruction until it is done; the answer then says it stopped unsettled.

One call runs at most 1500 frames, or the ticks of 1500 standard frames, which is half a minute of either machine's time, or a minute of a CPC's whose frames do not come; or 100000 instructions, under a second. On the machine the server was written on, 1500 of a CPC's frames run in about six seconds, well inside the minute a client of the protocol's TypeScript SDK waits for an answer.

## What a machine holds

`inspect` reads the emulator's own struct for the machine, `cpc_t` or `spectrum_t`, by the path a C expression gives a field of it: `cpu.pc`, `crtc.registers[12]`, `fdc.units[0].head`. Its fields keep their headers' names, and a C enum's value is its enumerator's name; pointers are not followed, and `peek` reads memory instead. Some fields are the chips' registers and counters; others are the emulator's machinery for stepping them a tick at a time, which no silicon holds as such, and the headers say which.

`set` writes by the same paths, every value given or none. The core indexes and sizes by some of its fields without checking them, because nothing it does takes them out of range: the size of the RAM, the RAM configuration of a 64K machine, the pen that picks one of the Gate Array's seventeen inks, the sound chip's selected register, the disc controller's counts of the bytes it has taken and given and the drive its command chose, the monitor's size, and the processor's count of the cycles under way. A value past those is refused, with the range that would do, and so is a WAIT written on a Spectrum's bus, which nothing there drives and which would hold its processor for good; the machine stands as it was. A CPC's memory map is worked out again after every write, and a Spectrum's ULA stood at the tick of the frame its `frame_tick` names, from which its beam is reckoned.

`peek` and `poke` reach memory as the processor would: through a CPC's banking and its ROMs, so that a write under a ROM lands in the RAM beneath it, and into a Spectrum's ROM not at all.

## The files a machine is built from

`machine_open` answers with every file the machine was built from, its firmware and its snapshot, each by its path and the SHA-256 of what it held. A finding names its copy by that hash, and a file changed between two openings gives two hashes. A snapshot's path is read on the file system the server runs on, from its working directory if it is relative. Anything there but a file is refused unread, firmware or snapshot, and so is a snapshot longer than 49179 bytes for a Spectrum, the one length its snapshot has, or than 8MB for a CPC, whose version 3 may carry chunks after its memory.

## The name of the build

The server names itself and the build in the `_meta` of its answers to `server/discover`, `tools/list` and `tools/call`, as the 2026-07-28 revision asks of every result and the older ones allow, and in the `serverInfo` of its answer to `initialize`. A ping's answer does not, nor an error, nor the empty answers the protocol's library gives for what the server does not offer. The name is the package's own version, then the commit of this repository and the commit of the emulator it was compiled from.

```
0.0.0+mcp.<commit>.dirty.emulator.<commit>
```

`.dirty` follows a commit whose files differ from the build's, an untracked file among them, or an ignored file of a kind the build compiles: here, the sources, the bridge, the build script and the files that pin the crates and the toolchain; in the emulator, its `src/`. A change anywhere else in either repository does not mark the build, because nothing compiled from it changed. Where a repository is not a checkout of its own, the commit is `none`. A finding can be made again only on a build that is unmarked and names commits the published repositories hold; one made on any other build is made again on such a build before it leaves.

## What it cannot do

- A machine can be run and read, but not yet seen, typed at, or given a disc or a tape. No tool shows its picture or presses its keys, so a game is reached only through a snapshot taken elsewhere.
- A snapshot carries less than the machine. A CPC's is read for the fields of version 1 alone: the 6845's counters a version 3 carries are not read, so the machine begins its frame again from the top, and no version keeps the disc controller, so one taken inside a disc access does not go on with it. A Spectrum's has no room for the beam. Nothing a version 3 carries in chunks after its memory dump is read, so one that keeps its memory in chunks rather than in the dump is refused. No machine is saved as a snapshot yet.
- Nothing stops a machine where it is asked to: there are no breakpoints, and no machine calls a routine of its own on request. A question about a moment is answered only by running to it and reading.
- A call cannot be taken back. A client that stops waiting for a run leaves the machine to run the whole of what it asked, which the machine's tick in its next answer shows; the limit on a call keeps that to half a minute of the machine's time, or a minute of a CPC's whose frames do not come.
- `set` refuses only what the emulator could not run from, so a machine written through its machinery can stand where no silicon could, and run on from there.
- The machines live only as long as the server. When its input closes, they are put away with it, nothing of them is kept, and the next server gives out the same handles again.
- One client speaks to one server, over its standard input and output and nothing else.
