# Colophon MCP

An agent has no eyes on the glass and no fingers on the keys, and what it learns of a machine is what it asks for. This server is where it asks: it stands up an Amstrad CPC 464, 664 or 6128 or a ZX Spectrum 48K, runs it, stops it, types at it and reads and shows what it holds, over the [Model Context Protocol](https://modelcontextprotocol.io).

The package carries the server built for six platforms, Linux, macOS and Windows each on x86-64 and on 64-bit ARM, with the firmware it reads, and starts the server built for the platform it runs on. Claude Code and Codex are given it with:

```sh
claude mcp add colophon -- npx -y colophon-mcp
codex mcp add colophon -- npx -y colophon-mcp
```

Any other client is given the command `npx` with the arguments `-y colophon-mcp`. [Installing the server](https://github.com/tulinmola/colophon-mcp/blob/main/docs/install.en.md) gives the line for each client, and [the server's documentation](https://github.com/tulinmola/colophon-mcp/blob/main/docs/index.en.md) is the full account of it.

The server is under the MIT licence. The firmware is not: it is the machines' own, carried under the permissions Amstrad and Locomotive Software gave emulators in 1999, which `NOTICE` quotes.
