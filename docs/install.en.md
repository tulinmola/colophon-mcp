---
title: Installing the server
description: The server as npm and each release carry it, built and tested on six platforms with the firmware it reads beside it, and the line each client is given to start it.
order: 1
---

A release is the server bound for travel: built on each platform it is for, tested there, and carried with the firmware it reads, so that a client need only be told what to start.

## From npm

The npm package `colophon-mcp` carries the server built for six platforms, Linux, macOS and Windows each on x86-64 and on 64-bit ARM, with the firmware it reads. It starts the server built for the platform it runs on and gives it the package's firmware, unless its command line names another directory with `--roms`. It is started by `npx`, which comes with Node.js and fetches the package the first time:

```sh
npx -y colophon-mcp
```

That is the command each client below is given, and `npx -y colophon-mcp@0.4.0` holds a client to that version.

A version on npm carries the servers of the release of the same version, so it answers with the same build name, and a version the release workflow publishes carries npm's record of where it was built ([trusted publishing](https://docs.npmjs.com/trusted-publishers)).

## Starting it from a client

### Claude Code

```sh
claude mcp add colophon -- npx -y colophon-mcp
```

The server is then started in the project the command was given in, and with `--scope user` before its name, in every project ([Claude Code's MCP documentation](https://code.claude.com/docs/en/mcp)).

### Claude Desktop

The server is named in `claude_desktop_config.json`, which is `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on Windows and `~/.config/Claude/claude_desktop_config.json` on Linux ([Connect to local MCP servers](https://modelcontextprotocol.io/docs/develop/connect-local-servers)):

```json
{
  "mcpServers": {
    "colophon": {
      "command": "npx",
      "args": ["-y", "colophon-mcp"]
    }
  }
}
```

### Codex

```sh
codex mcp add colophon -- npx -y colophon-mcp
```

The command writes the server into `~/.codex/config.toml`, under `[mcp_servers.colophon]` ([Codex's MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)).

### VS Code

```sh
code --add-mcp '{"name":"colophon","type":"stdio","command":"npx","args":["-y","colophon-mcp"]}'
```

The command writes the server into the user's profile; a workspace's `.vscode/mcp.json` names it as a key under `servers`, with the same `type`, `command` and `args` ([VS Code's MCP configuration reference](https://code.visualstudio.com/docs/agents/reference/mcp-configuration)).

### Cursor

The server is named under `mcpServers` in `~/.cursor/mcp.json` for every project, or in a project's `.cursor/mcp.json` for that one, in the shape Claude Desktop reads, with `"type": "stdio"` beside `command` ([Cursor's MCP documentation](https://cursor.com/docs/context/mcp)).

### Zed

The server is named under `context_servers` in Zed's settings, which `zed: open settings file` opens ([Zed's MCP documentation](https://zed.dev/docs/ai/mcp)):

```json
{
  "context_servers": {
    "colophon": {
      "command": "npx",
      "args": ["-y", "colophon-mcp"]
    }
  }
}
```

### A cloud session

Claude Code on the web starts the servers a repository's `.mcp.json` names in a session on that one repository, and its default network, Trusted, reaches `registry.npmjs.org` ([Claude Code's cloud environments](https://code.claude.com/docs/en/cloud-environments)), so the repository's `.mcp.json` names it as Claude Desktop's configuration does:

```json
{
  "mcpServers": {
    "colophon": {
      "command": "npx",
      "args": ["-y", "colophon-mcp"]
    }
  }
}
```

## From a release

Each release on the repository's [releases page](https://github.com/tulinmola/colophon-mcp/releases) carries an archive for each of the six platforms, built and tested on a machine of that platform:

| Platform | Archive |
| --- | --- |
| Linux, x86-64 | `colophon-mcp-x86_64-unknown-linux-musl.tar.gz` |
| Linux, 64-bit ARM | `colophon-mcp-aarch64-unknown-linux-musl.tar.gz` |
| macOS, Apple silicon | `colophon-mcp-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `colophon-mcp-x86_64-apple-darwin.tar.gz` |
| Windows, x86-64 | `colophon-mcp-x86_64-pc-windows-msvc.zip` |
| Windows, 64-bit ARM | `colophon-mcp-aarch64-pc-windows-msvc.zip` |

An archive holds one folder of its own name, and in it the server, `colophon-mcp`, or `colophon-mcp.exe` on Windows; `roms`, the firmware it reads, as the emulator fetches it; the licence; and `NOTICE`. The firmware is not under the licence: it is the machines' own, carried under the permissions Amstrad and Locomotive Software gave emulators in 1999, which `NOTICE` quotes. A Linux server is linked statically and runs on a Linux of any distribution, whatever its C library; a Windows server runs without Microsoft's Visual C++ runtime installed.

The server names its build in its answers: its version, and the commits of this repository and of the emulator it was built from, as a release's notes name it too. A release is built from commits the published repositories hold, with no change of its own, so a finding made on one need not be made again before it leaves.

`SHA256SUMS`, beside the archives, holds their hashes, and GitHub's command line checks that an archive was built by this repository's release workflow:

```sh
gh attestation verify colophon-mcp-aarch64-apple-darwin.tar.gz --repo tulinmola/colophon-mcp
```

On macOS or Linux an archive is fetched and unpacked with:

```sh
curl -fsSLO https://github.com/tulinmola/colophon-mcp/releases/latest/download/colophon-mcp-aarch64-apple-darwin.tar.gz
tar -xzf colophon-mcp-aarch64-apple-darwin.tar.gz
```

and on Windows, in PowerShell, with:

```powershell
Invoke-WebRequest "https://github.com/tulinmola/colophon-mcp/releases/latest/download/colophon-mcp-x86_64-pc-windows-msvc.zip" -OutFile "colophon-mcp-x86_64-pc-windows-msvc.zip"
Expand-Archive "colophon-mcp-x86_64-pc-windows-msvc.zip" -DestinationPath .
```

`latest/download` takes the newest release; `download/v0.4.0`, or another release's tag, takes that one.

A client is given a server unpacked from an archive in place of `npx -y colophon-mcp`: the server's absolute path as the command, and as its arguments `--roms` and the absolute path of the `roms` folder beside it. For the archive for Apple silicon unpacked in `/path/to`, Claude Code's line is

```sh
claude mcp add colophon -- /path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp --roms /path/to/colophon-mcp-aarch64-apple-darwin/roms
```

and Claude Desktop's configuration names it as:

```json
{
  "mcpServers": {
    "colophon": {
      "command": "/path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp",
      "args": ["--roms", "/path/to/colophon-mcp-aarch64-apple-darwin/roms"]
    }
  }
}
```

In JSON a Windows path is written with its backslashes doubled.

## What it lacks

No package manager but npm carries the server, and neither the protocol's registry nor a client's own catalogue lists it: each client is given its command by hand. The package wants Node.js, for `npx`; without it, a release's archive serves.

The package carries every platform's server where a client needs one, so the first `npx -y colophon-mcp` fetches about 12 MB, which unpack to about 36 MB, and a client that waits little for a server to start may give up on it the first time.

0.1.0 carries no record of where it was built. npm trusts a workflow to publish a package only once the package exists ([`npm trust`](https://docs.npmjs.com/cli/v12/commands/npm-trust)), so 0.1.0 was published by hand, from the release's archives, whose attestations vouch for the archives its servers were taken from.

Claude Code on Linux x86-64 is the one client yet seen to start the package and use it: given `npx -y colophon-mcp`, it fetched 0.1.0 from npm and ran a CPC 6128. The release workflow starts a package through `npx` on each of the six platforms before it publishes it, as a client would, and holds it to its build's name; 0.1.0, published by hand, was started so on Linux x86-64 alone, and its launcher on the other five in a package of the pull request's own servers. None of the other clients above has been tried with it, nor any client on macOS or Windows.

No cloud session has started the server yet. The Trusted network reaches `registry.npmjs.org`, so `npx` should fetch the package there; that has not been tried.

No release is notarized by Apple or signed with a certificate Windows trusts. An archive fetched through a browser on macOS is marked as downloaded, and macOS may refuse to run the server unpacked from it, since Apple has not notarized it; `xattr -d com.apple.quarantine colophon-mcp`, in the server's folder, lifts the mark. An archive fetched with `curl`, as above, is not marked.

Any other platform builds the server from its source, against a checkout of the emulator beside it, as the [README](https://github.com/tulinmola/colophon-mcp#building) says; a build from files with uncommitted changes is marked so in its name.
