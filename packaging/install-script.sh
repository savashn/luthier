#!/bin/sh
# Writes install.sh from the packaging/install.sh template, run from the
# repository root after packaging/package.sh:
#
#     packaging/install-script.sh <version> <arch>...
#
# Fills in the version and, for each architecture named (x86_64, aarch64), the
# SHA-256 of its luthier-<arch>-linux.tar.gz, which must be there. One not
# named keeps its placeholder, and the script then refuses to install on it:
# the release names both, CI only the one it built.
set -eu

version=$1
shift
[ $# -gt 0 ] || {
  echo "name at least one architecture" >&2
  exit 1
}
[ -f packaging/install.sh ] || {
  echo "run this from the repository root" >&2
  exit 1
}

# A failed run leaves no install.sh at all, rather than one from an earlier
# run that could pass for this one.
rm -f install.sh
substitutions="s/@VERSION@/$version/"
for arch in "$@"; do
  # The template holds one `sha256_<arch>='@SHA256_<ARCH>@'` line for each
  # architecture a release can build, so the name says which to fill.
  case "$arch" in
  '' | *[!a-z0-9_]*)
    echo "not an architecture name: $arch" >&2
    exit 1
    ;;
  esac
  placeholder="@SHA256_$(printf '%s' "$arch" | tr '[:lower:]' '[:upper:]')@"
  grep -q "^sha256_$arch='$placeholder'$" packaging/install.sh || {
    echo "packaging/install.sh has no sha256_$arch line, so it cannot pick a $arch build" >&2
    exit 1
  }
  tarball="luthier-$arch-linux.tar.gz"
  [ -f "$tarball" ] || {
    echo "$tarball is not here; run packaging/package.sh for $arch first" >&2
    exit 1
  }
  sha=$(sha256sum "$tarball" | cut -d' ' -f1)
  substitutions="$substitutions;s/$placeholder/$sha/"
done

trap 'rm -f install.sh.new' EXIT
sed "$substitutions" packaging/install.sh > install.sh.new
# A whole SHA-256 must have gone in for each architecture named, or every
# user of the script there would be refused, by a script with a provenance
# attestation.
for arch in "$@"; do
  grep -qE "^sha256_$arch='[0-9a-f]{64}'$" install.sh.new || {
    echo "install.sh did not get the $arch SHA-256" >&2
    exit 1
  }
done
grep -qE "^version='[0-9]+\.[0-9]+\.[0-9]+[^']*'$" install.sh.new || {
  echo "install.sh did not get a version" >&2
  exit 1
}
mv install.sh.new install.sh
sha256sum install.sh
