#!/bin/sh
# Makes a release's files from a built binary, run from the repository root:
#
#     packaging/package.sh <binary> <version> <target>
#
# The release workflow runs it on the static musl build; CI runs it on every
# change, on a debug build, so a broken nfpm.yaml or install.sh shows up there
# rather than on a tag. The file names come from the environment (ASSET,
# DEB_ASSET, RPM_ASSET, SCRIPT_ASSET), where the workflows set them; the files
# land in the repository root, where .gitignore keeps them out of commits.
set -eu

binary=$1
version=$2
target=$3
: "${ASSET:?}" "${DEB_ASSET:?}" "${RPM_ASSET:?}" "${SCRIPT_ASSET:?}"
[ -f packaging/nfpm.yaml ] || {
  echo "run this from the repository root" >&2
  exit 1
}

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
tar czf "$ASSET" "$dir"

# The tarball's files, for people who install software as a package: the man
# page and completions then work without any setup, and the package manager
# can remove it all. nfpm.yaml says where each goes; it reads `stage`, as nfpm
# expands no variables in source paths.
rm -rf stage
cp -r "$dir" stage
gzip -9n stage/luthier.1
VERSION="$version" nfpm pkg --config packaging/nfpm.yaml --packager deb --target "$DEB_ASSET"
VERSION="$version" nfpm pkg --config packaging/nfpm.yaml --packager rpm --target "$RPM_ASSET"

# The script installs the tarball of the release it is attached to, and
# refuses one whose SHA-256 is not the one written into it here.
sha=$(sha256sum "$ASSET" | cut -d' ' -f1)
sed -e "s/@VERSION@/$version/" -e "s/@SHA256@/$sha/" \
  packaging/install.sh > "$SCRIPT_ASSET"
# What went in must be a version and a whole SHA-256, or every user of the
# script would be refused, by a script with a provenance attestation.
if ! grep -qE "^version='[0-9]+\.[0-9]+\.[0-9]+[^']*'$" "$SCRIPT_ASSET" ||
  ! grep -qE "^sha256='[0-9a-f]{64}'$" "$SCRIPT_ASSET"; then
  echo "$SCRIPT_ASSET did not get a version and SHA-256" >&2
  exit 1
fi

sha256sum "$ASSET" "$DEB_ASSET" "$RPM_ASSET" "$SCRIPT_ASSET"
