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

# nfpm's own release build, pinned by version and SHA-256 here alone, so CI
# checks the packaging with the nfpm the release uses. An nfpm already on PATH
# is used only if it is that version. The machine this runs on picks the
# archive, not <target>.
nfpm_version=2.47.0
# `go install` builds report it with a leading v; the release build without.
nfpm_version_re=$(printf '%s' "$nfpm_version" | sed 's/\./\\./g')
if ! nfpm --version 2> /dev/null | grep -qE "^GitVersion: +v?$nfpm_version_re\$"; then
  case "$(uname -m)" in
  x86_64)
    nfpm_archive="nfpm_${nfpm_version}_Linux_x86_64.tar.gz"
    nfpm_sha256=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
    ;;
  aarch64)
    nfpm_archive="nfpm_${nfpm_version}_Linux_arm64.tar.gz"
    nfpm_sha256=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c
    ;;
  *)
    echo "no nfpm build is pinned for $(uname -m)" >&2
    exit 1
    ;;
  esac
  nfpm_dir=$(mktemp -d)
  trap 'rm -rf "$nfpm_dir"' EXIT
  curl -fsSL --retry 3 -o "$nfpm_dir/$nfpm_archive" \
    "https://github.com/goreleaser/nfpm/releases/download/v$nfpm_version/$nfpm_archive"
  printf '%s  %s\n' "$nfpm_sha256" "$nfpm_dir/$nfpm_archive" | sha256sum -c - > /dev/null 2>&1 || {
    echo "$nfpm_archive is not the file its pinned SHA-256 names" >&2
    exit 1
  }
  tar xzf "$nfpm_dir/$nfpm_archive" -C "$nfpm_dir" nfpm
  PATH="$nfpm_dir:$PATH"
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
