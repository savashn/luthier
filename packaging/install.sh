#!/bin/sh
# Installs Luthier into your home directory, without root:
#
#     curl -fsSL https://github.com/savashn/luthier/releases/latest/download/install.sh | sh
#
# This file is a template. The release workflow writes the version and the
# tarball's SHA-256 into it when it attaches it to a release, so each copy
# installs the release it came from and refuses any tarball but the one that
# release built.
#
# LUTHIER_PREFIX moves everything (default ~/.local; the binary goes to
# $LUTHIER_PREFIX/bin). Not PREFIX, which build environments export for their own use.
# `sh install.sh --uninstall` removes what it installed.
#
# Everything happens in main(), called on the last line: piped into sh, a
# download cut off halfway defines functions and runs nothing.
set -eu

version='@VERSION@'
sha256='@SHA256@'
asset='luthier-x86_64-linux.tar.gz'

say() { printf '%s\n' "$*"; }
die() {
  printf 'luthier install: %s\n' "$*" >&2
  exit 1
}

main() {
  case "$version" in
  @*) die "this is the template; run the install.sh attached to a release" ;;
  esac

  # Only for a mirror or a test: the SHA-256 above is checked wherever it comes from.
  base="${LUTHIER_RELEASE_URL:-https://github.com/savashn/luthier/releases/download/v$version}"
  prefix="${LUTHIER_PREFIX:-$HOME/.local}"
  bin="$prefix/bin/luthier"
  man="$prefix/share/man/man1/luthier.1"
  bash_completion="$prefix/share/bash-completion/completions/luthier"
  zsh_completion="$prefix/share/zsh/site-functions/_luthier"
  fish_completion="$prefix/share/fish/vendor_completions.d/luthier.fish"

  case "${1:-}" in
  '') install_luthier ;;
  --uninstall) uninstall_luthier ;;
  *) die "unknown argument: $1 (the only one is --uninstall)" ;;
  esac
}

uninstall_luthier() {
  rm -f "$bin" "$man" "$bash_completion" "$zsh_completion" "$fish_completion"
  say "Removed Luthier from $prefix."
  say "The plugins it installed and its own data are still there; run"
  say "\`luthier remove\` for them before uninstalling if you want them gone."
}

install_luthier() {
  [ "$(uname -s)" = Linux ] || die "Luthier runs on Linux only"
  case "$(uname -m)" in
  x86_64 | amd64) ;;
  *) die "Luthier is built for x86_64 only; this machine is $(uname -m)" ;;
  esac

  if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
  elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
  else
    die "needs curl or wget to download"
  fi
  command -v sha256sum > /dev/null 2>&1 || die "needs sha256sum to check the download"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 1' INT TERM

  say "Downloading Luthier $version..."
  fetch "$base/$asset" "$tmp/$asset" || die "could not download $base/$asset"
  printf '%s  %s\n' "$sha256" "$tmp/$asset" | sha256sum -c - > /dev/null 2>&1 ||
    die "the download is not the file release v$version published; nothing was installed"

  tar xzf "$tmp/$asset" -C "$tmp"
  set -- "$tmp"/luthier-*/
  src=$1
  [ -x "$src/luthier" ] || die "the tarball does not have the expected layout"

  install -Dm755 "$src/luthier" "$bin"
  install -Dm644 "$src/luthier.1" "$man"
  install -Dm644 "$src/completions/luthier.bash" "$bash_completion"
  install -Dm644 "$src/completions/luthier.zsh" "$zsh_completion"
  install -Dm644 "$src/completions/luthier.fish" "$fish_completion"

  say "Installed Luthier $version: $bin"
  # A .deb or .rpm copy left beside this one would be shadowed by it, or
  # shadow it, depending on PATH; either way one of them goes stale.
  if [ -e /usr/bin/luthier ] && [ "$bin" != /usr/bin/luthier ]; then
    say ""
    say "/usr/bin/luthier, from a .deb or .rpm, is installed too. Whichever comes"
    say "first on your PATH runs; remove the other so it cannot go out of date."
  fi
  if [ "$prefix" != "$HOME/.local" ]; then
    say ""
    say "The man page and completions are under $prefix/share; man, bash and fish"
    say "find them only if that is a directory they search."
  fi
  case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *)
    say ""
    say "$prefix/bin is not on your PATH. Add it once, to ~/.bashrc or ~/.zshrc"
    say "depending on your shell, and open a new terminal:"
    say ""
    say "    export PATH=\"$prefix/bin:\$PATH\""
    ;;
  esac
  case "${SHELL:-}" in
  */zsh)
    say ""
    say "For zsh's completions, add this to ~/.zshrc before compinit:"
    say ""
    say "    fpath=(\"$prefix/share/zsh/site-functions\" \$fpath)"
    ;;
  esac
}

main "$@"
