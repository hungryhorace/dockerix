#!/usr/bin/env bash
# Build the release binary and pack it as the release tarball for one
# platform. The Jenkins release job (Jenkinsfile.release) runs it once per
# agent with the agent's own target triple; `just package` runs it here.
#
#   scripts/package.sh <rust-target-triple>      from the repository root
#
# Leaves in dist/, which is emptied first (a stale tarball from an earlier
# run would otherwise be uploaded alongside):
#
#   dockerix-v<ver>-<target>.tar.gz        dockerix, README.md and LICENSE in
#                                          a directory of the same name
#   dockerix-v<ver>-<target>.tar.gz.sha256 its checksum, `shasum -a 256` form
#
# The version is cli/Cargo.toml's. The build is `cargo install` into a
# directory of our own, so the binary lands where this script says whatever
# CARGO_TARGET_DIR is set to (the agents share one). It is the same build
# as `just install`: the image's sources are embedded, so the binary needs
# no checkout.
set -euo pipefail

target=${1:?usage: scripts/package.sh <rust-target-triple>}
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' cli/Cargo.toml | head -1)
[ -n "$ver" ] || { echo "package: no version in cli/Cargo.toml" >&2; exit 1; }
name="dockerix-v$ver-$target"

host=$(rustc -vV | sed -n 's/^host: //p')
if [ "$target" != "$host" ]; then
    echo "package: $target is not this toolchain's host ($host); nothing here cross-compiles" >&2
    exit 1
fi

rm -rf dist
mkdir -p "dist/$name"
cargo install --locked --force --path cli --root dist/.install >&2
cp dist/.install/bin/dockerix README.md LICENSE "dist/$name/"
rm -rf dist/.install
tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "dist/$name"
(cd dist && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")

echo "dist/$name.tar.gz"
cat "dist/$name.tar.gz.sha256" >&2
