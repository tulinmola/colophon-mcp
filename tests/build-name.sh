#!/bin/sh
# A server held to the name of the build it was meant to be, which it gives
# in its answer to initialize. The release workflow runs it, against the
# server it builds and the package it packs.
#
#   tests/build-name.sh NAME COMMAND...
#
# COMMAND starts the server, with whatever arguments it needs.
set -eu

expected=$1
shift

initialize='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"release","version":"0"}}}'
answer=$(echo "$initialize" | "$@") || {
  status=$?
  echo "the server exited with status $status" >&2
  exit 1
}
named=$(echo "$answer" | grep -o '"version":"[^"]*"' || true)
if [ "$named" != "\"version\":\"$expected\"" ]; then
  echo "expected \"version\":\"$expected\" in the answer to initialize, got ${named:-no answer naming a version}" >&2
  exit 1
fi
