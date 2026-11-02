#!/bin/sh
# The npm package laid out from a release's archives: each platform's server
# in a folder of servers/ named for its target, as its archive is, and the
# firmware once, in roms/, which every archive must carry alike.
#
#   npm/assemble.sh VERSION ARCHIVES PACKAGE
#
# VERSION is the package's, the build name up to its +; ARCHIVES the
# directory holding all six archives; and PACKAGE the directory the package
# is laid out in, which must not yet exist.
set -eu

version=$1
archives=$2
package=$3
source=$(dirname "$0")

unpacked=$(mktemp -d)
trap 'rm -rf "$unpacked"' EXIT
mkdir "$package"
cp "$source/package.json" "$source/colophon-mcp.js" "$source/README.md" \
  "$source/../LICENSE" "$source/../NOTICE" "$package/"
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
  x86_64-apple-darwin aarch64-apple-darwin; do
  tar -xzf "$archives/colophon-mcp-$target.tar.gz" -C "$unpacked"
done
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
  unzip -q "$archives/colophon-mcp-$target.zip" -d "$unpacked"
done
for folder in "$unpacked"/colophon-mcp-*; do
  target=${folder##*/colophon-mcp-}
  mkdir -p "$package/servers/$target"
  mv "$folder"/colophon-mcp* "$package/servers/$target/"
  if [ -d "$package/roms" ]; then
    diff -r "$package/roms" "$folder/roms"
  else
    cp -R "$folder/roms" "$package/roms"
  fi
done
cd "$package" && npm pkg set version="$version"
