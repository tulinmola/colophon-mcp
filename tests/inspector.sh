#!/bin/sh
# The server graded by a client this repository did not write: the Model
# Context Protocol's own Inspector,
# https://github.com/modelcontextprotocol/inspector, from its command line,
# in each era it speaks. Its legacy era opens with initialize in 2025-11-25,
# and its modern one with server/discover in 2026-07-28. Exit status 5 is
# its own for a tool that answered with isError, and 6 for a tool schema it
# finds would not carry to other clients.
#
#   tests/inspector.sh SERVER INSPECTOR
#
# SERVER is the built binary, and INSPECTOR the npm package, at its version.
set -eu

server=$1
inspector=$2

roms=$(mktemp -d)
trap 'rm -rf "$roms"' EXIT
for firmware in cpc6128 cpc664 cpc464; do
  head -c 32768 /dev/zero >"$roms/$firmware.rom"
done
head -c 16384 /dev/zero >"$roms/amsdos.rom"
head -c 16384 /dev/zero >"$roms/spectrum48.rom"

fail() {
  echo "inspector: $*" >&2
  exit 1
}

inspect() {
  npx --yes "$inspector" --cli --quiet --format json --protocol-era "$era" \
    "$server" --roms "$roms" "$@"
}

for era in legacy modern; do
  tools=$(inspect --method tools/list --strict) || fail "$era: tools/list exited $?"
  for tool in machine_close machine_list machine_open; do
    case $tools in
    *"\"name\":\"$tool\""*) ;;
    *) fail "$era: $tool is not listed: $tools" ;;
    esac
  done

  opened=$(inspect --method tools/call --tool-name machine_open --tool-arg machine=spectrum48) ||
    fail "$era: machine_open exited $?"
  case $opened in
  *'"structuredContent":{"handle":"spectrum48-1","machine":"spectrum48"}'*) ;;
  *) fail "$era: machine_open answered $opened" ;;
  esac

  status=0
  inspect --method tools/call --tool-name machine_close --tool-arg handle=spectrum48-1 \
    >/dev/null 2>&1 || status=$?
  # Each run is a server of its own, so the handle is one it never gave.
  [ "$status" -eq 5 ] || fail "$era: closing a handle never given exited $status"
done
echo "inspector: both eras pass"
