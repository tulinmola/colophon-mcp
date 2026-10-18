---
title: The MCP server
description: The machines handed to an agent over the Model Context Protocol — stood up by name, listed, and put away.
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

`machine_open` builds a machine by the name the emulator's command line gives it, and wires it as the command line does: the same RAM, the same firmware in the same sockets, and the disc interface where the machine has one. `machine_list` names the machines standing, and `machine_close` puts one away. Each tool's description is the reference for it; an agent reads them from the server.

## The name of the build

The server names itself and the build in the `_meta` of its answers to `server/discover`, `tools/list` and `tools/call`, as the 2026-07-28 revision asks of every result and the older ones allow, and in the `serverInfo` of its answer to `initialize`. A ping's answer does not, nor an error, nor the empty answers the protocol's library gives for what the server does not offer. The name is the package's own version, then the commit of this repository and the commit of the emulator it was compiled from.

```
0.0.0+mcp.<commit>.dirty.emulator.<commit>
```

`.dirty` follows a commit whose files differ from the build's, an untracked file among them, or an ignored file of a kind the build compiles: here, the sources, the bridge, the build script and the files that pin the crates and the toolchain; in the emulator, its `src/`. A change anywhere else in either repository does not mark the build, because nothing compiled from it changed. Where a repository is not a checkout of its own, the commit is `none`. A finding can be made again only on a build that is unmarked and names commits the published repositories hold; one made on any other build is made again on such a build before it leaves.

## What it cannot do

- Nothing runs a machine yet. One stands where it was built, before its first instruction, and no tool can run it, step it, read it, type at it or show its picture, so a machine can be opened and nothing more can be learned from it.
- No answer names the firmware a machine was built from. Each file is read afresh when a machine is opened and checked only for its size, so a record cannot say what stood in the sockets, and a file changed between two openings gives two machines under one name.
- The machines live only as long as the server. When its input closes, they are put away with it, nothing of them is kept, and the next server gives out the same handles again.
- One client speaks to one server, over its standard input and output and nothing else.
