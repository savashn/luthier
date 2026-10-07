#!/bin/sh
# Prints the version every crate in the workspace carries, `[workspace.package]`
# in the root Cargo.toml, for the workflows. Run from the repository root.
#
# The release compares a tag with it as the version `luthier --version`
# reports, which holds only while luthier-cli takes the workspace's version,
# so that is checked too.
set -eu

version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml)
if [ -z "$version" ] || [ "$(printf '%s\n' "$version" | wc -l)" -ne 1 ]; then
  echo "Cargo.toml has no single workspace version" >&2
  exit 1
fi
grep -qx 'version.workspace = true' crates/luthier-cli/Cargo.toml || {
  echo "luthier-cli does not take the workspace version, so it is not this" >&2
  exit 1
}
printf '%s\n' "$version"
