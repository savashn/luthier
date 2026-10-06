#!/bin/sh
# Makes one architecture's release files from a built binary, run from the
# repository root:
#
#     packaging/package.sh <binary> <version> <target>
#
# <target> is the binary's Rust target triple. Its first part, x86_64 or
# aarch64, names the files: luthier-<arch>-linux.tar.gz, .deb and .rpm, in the
# repository root, where .gitignore keeps them out of commits. The release
# workflow runs it on each architecture's static musl build; CI runs it on
# every change, on a debug build, so a broken nfpm.yaml shows up there rather
# than on a tag. packaging/install-script.sh then writes the install script
# over the tarballs.
set -eu

binary=$1
version=$2
target=$3
[ -f packaging/nfpm.yaml ] || {
  echo "run this from the repository root" >&2
  exit 1
}
arch=${target%%-*}
case "$arch" in
x86_64) nfpm_arch=amd64 ;;
aarch64) nfpm_arch=arm64 ;;
*)
  echo "no packages for $arch" >&2
  exit 1
  ;;
esac
name="luthier-$arch-linux"

# One pin for both workflows, so CI checks the packaging with the nfpm the
# release uses. The Go checksum database keeps the tag's contents fixed.
nfpm_version=v2.47.0
if ! command -v nfpm > /dev/null 2>&1; then
  go install "github.com/goreleaser/nfpm/v2/cmd/nfpm@$nfpm_version"
  PATH="$(go env GOPATH)/bin:$PATH"
fi

# The tarball. The binary generates its own man page and completions, so they
# cannot drift from the arguments it actually accepts.
dir="luthier-$version-$target"
mkdir -p "$dir/completions"
cp "$binary" "$dir/luthier"
"$binary" man > "$dir/luthier.1"
for shell in bash zsh fish; do
  "$binary" completions "$shell" > "$dir/completions/luthier.$shell"
done
cp LICENSE README.md CHANGELOG.md SECURITY.md "$dir/"
tar czf "$name.tar.gz" "$dir"

# The tarball's files, for people who install software as a package: the man
# page and completions then work without any setup, and the package manager
# can remove it all. nfpm.yaml says where each goes; it reads `stage`, as nfpm
# expands no variables in source paths.
rm -rf stage
cp -r "$dir" stage
gzip -9n stage/luthier.1
for packager in deb rpm; do
  VERSION="$version" NFPM_ARCH="$nfpm_arch" nfpm pkg \
    --config packaging/nfpm.yaml --packager "$packager" --target "$name.$packager"
done

sha256sum "$name.tar.gz" "$name.deb" "$name.rpm"
