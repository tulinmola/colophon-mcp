#!/usr/bin/env node
// The server built for this platform, given the package's firmware.
// Arguments on the command line replace that rather than add to it: --roms
// is the server's one option, so they can only name another directory.
import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// Every platform package.json's os and cpu admit, by Node's names, and the
// target its server was built for, which names its archive and its folder
// in servers/.
const targets = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
  "win32-arm64": "aarch64-pc-windows-msvc",
};

const here = dirname(fileURLToPath(import.meta.url));
const platform = `${process.platform}-${process.arch}`;
const target = targets[platform];
if (!target) {
  console.error(
    `colophon-mcp: no server for ${platform}. The package carries servers for ${Object.keys(targets).join(", ")} only. On another platform, build the server from its source: https://github.com/tulinmola/colophon-mcp#building`,
  );
  process.exit(1);
}
const executable =
  process.platform === "win32" ? "colophon-mcp.exe" : "colophon-mcp";
const path = join(here, "servers", target, executable);
const args =
  process.argv.length > 2
    ? process.argv.slice(2)
    : ["--roms", join(here, "roms")];

const server = spawn(path, args, { stdio: "inherit" });
// A client stops the server by closing its input and, if it does not exit,
// by SIGTERM (the protocol's lifecycle, "Shutdown",
// https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
// and the process a client started is this one.
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => server.kill(signal));
}
server.on("error", (error) => {
  console.error(
    `colophon-mcp: cannot start ${path} (${error.code}). The package is incomplete or damaged: under npx, delete the _npx folder in npm's cache (npm config get cache) and start it again; otherwise install the package again.`,
  );
  process.exit(1);
});
server.on("exit", (code) => {
  process.exitCode = code ?? 1;
});
