---
title: Installing the server
description: The server as each release carries it, built and tested on six platforms with the firmware it reads beside it, and the line each client is given to start it.
order: 1
---

A release is the server bound for travel: built on each platform it is for, tested there, and carried with the firmware it reads, so that a client need only be told where it lies.

## A release

Each release on the repository's [releases page](https://github.com/tulinmola/colophon-mcp/releases) carries an archive for each of six platforms, built and tested on a machine of that platform:

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

`latest/download` takes the newest release; `download/v0.1.0`, or another release's tag, takes that one.

## Starting it from a client

Every client is given the same command line, the server and the folder its firmware is read from, both by their absolute paths. Below, the archive for Apple silicon was unpacked in `/path/to`; another platform's folder takes its place, and on Windows the server is `colophon-mcp.exe`.

### Claude Code

```sh
claude mcp add colophon -- /path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp --roms /path/to/colophon-mcp-aarch64-apple-darwin/roms
```

The server is then started in the project the command was given in, and with `--scope user` before its name, in every project ([Claude Code's MCP documentation](https://code.claude.com/docs/en/mcp)).

### Claude Desktop

The server is named in `claude_desktop_config.json`, which is `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on Windows and `~/.config/Claude/claude_desktop_config.json` on Linux ([Connect to local MCP servers](https://modelcontextprotocol.io/docs/develop/connect-local-servers)):

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

A Windows path is written with its backslashes doubled, as JSON escapes them.

### Codex

```sh
codex mcp add colophon -- /path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp --roms /path/to/colophon-mcp-aarch64-apple-darwin/roms
```

The command writes the server into `~/.codex/config.toml`, under `[mcp_servers.colophon]` ([Codex's MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)).

### VS Code

```sh
code --add-mcp '{"name":"colophon","type":"stdio","command":"/path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp","args":["--roms","/path/to/colophon-mcp-aarch64-apple-darwin/roms"]}'
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
      "command": "/path/to/colophon-mcp-aarch64-apple-darwin/colophon-mcp",
      "args": ["--roms", "/path/to/colophon-mcp-aarch64-apple-darwin/roms"]
    }
  }
}
```

### A cloud session

Claude Code on the web runs an environment's setup script as root on Ubuntu the first time a session starts there, and later sessions start from what it left, until the script changes or roughly seven days pass; its default network reaches GitHub, and a session on one repository starts the servers that repository's `.mcp.json` names ([Claude Code's cloud environments](https://code.claude.com/docs/en/cloud-environments)). So the script names the release it takes, and a new release is taken by changing the tag in it. A setup script that fetches the server for the session's processor:

```sh
#!/bin/bash
mkdir -p /opt/colophon-mcp
curl -fsSL "https://github.com/tulinmola/colophon-mcp/releases/download/v0.1.0/colophon-mcp-$(uname -m)-unknown-linux-musl.tar.gz" | tar -xz -C /opt/colophon-mcp --strip-components 1
```

and the repository's `.mcp.json`:

```json
{
  "mcpServers": {
    "colophon": {
      "command": "/opt/colophon-mcp/colophon-mcp",
      "args": ["--roms", "/opt/colophon-mcp/roms"]
    }
  }
}
```

## What it lacks

No package manager carries the server yet, neither npm nor Homebrew, and neither the protocol's registry nor a client's own catalogue lists it: each client is given its path by hand, and a new release is fetched as the first was.

No release is notarized by Apple or signed with a certificate Windows trusts. An archive fetched through a browser on macOS is marked as downloaded, and macOS may refuse to run the server unpacked from it, since Apple has not notarized it; `xattr -d com.apple.quarantine colophon-mcp`, in the server's folder, lifts the mark. An archive fetched with `curl`, as above, is not marked.

No cloud session has fetched a release yet. Claude Code on the web passes GitHub's traffic through a proxy that refuses a source archive of a repository not attached to the session, even a public one, and its page says nothing of a release's archives, so the setup script above is untried.

Any other platform builds the server from its source, against a checkout of the emulator beside it, as the [README](https://github.com/tulinmola/colophon-mcp#building) says; a build from files with uncommitted changes is marked so in its name.
