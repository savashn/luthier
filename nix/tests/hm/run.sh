#!/usr/bin/env bash
# Tries programs.luthier against real Home Manager: builds the configurations
# in flake.nix and runs what they generate — the service's runner, the script
# a declaration is applied with, the activation block — with HOME in a
# scratch directory. It never activates anything, which would touch the real
# profile.
#
# Needs the Nix daemon and the network: `luthier update` reads the OAS index
# and matsuri is downloaded from GitHub, so it cannot run in the Nix sandbox
# or in CI. Run it whenever nix/hm-module.nix changes:
#
#   nix/tests/hm/run.sh
#
# The scratch directory is kept when a check fails, to look at.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# The `scratch` of flake.nix.
T=/tmp/luthier-hm-test
state=$T/home/.local/state/luthier/home-manager
bash=$(command -v bash)

failures=0
# A file that exists and does not hold the pattern.
lacks() { [ -f "$2" ] && ! grep -q "$1" "$2"; }
check() {
  local what=$1
  shift
  if "$@"; then
    printf 'ok    %s\n' "$what"
  else
    printf 'FAIL  %s\n' "$what"
    failures=$((failures + 1))
  fi
}

# The configurations' outputs, built.
build() {
  nix build "$here#homeConfigurations.$1.activationPackage" --out-link "$T/result-$1"
  echo "$T/result-$1"
}

# The script a configuration applies its declaration with.
apply_script() {
  grep -o -m1 '/nix/store/[^ ]*-luthier-apply' "$1/activate"
}

unit_dir() {
  echo "$1/home-files/.config/systemd/user"
}

# As the user manager would start it: an environment with none of the
# login shell's variables. Prints its status; the output goes to $2.
run_service() {
  local status=0
  env -i HOME="$T/home" "$1" >"$2" 2>&1 || status=$?
  echo "$status"
}

# luthier as a login shell under this configuration would run it.
shell_luthier() {
  env HOME="$T/home" XDG_CONFIG_HOME="$T/home/.config" XDG_DATA_HOME="$T/home/data" \
    XDG_CACHE_HOME="$T/home/.cache" "$bg/home-path/bin/luthier" "$@"
}
lists_matsuri() { shell_luthier list | grep -qi matsuri; }
libraries_in() { shell_luthier location | grep -q "$1\$"; }

rm -rf "$T"
mkdir -p "$T/home" "$T/log"

echo "== Building"
bg=$(build background)
ml=$(build missing-location)
act=$(build activation)
plain=$(build no-xdg)

echo "== Units"
units=$(unit_dir "$bg")
check "luthier.service is never restarted by a switch" \
  grep -qx 'X-SwitchMethod=keep-old' "$units/luthier.service"
check "luthier.service is wanted by no target" \
  lacks '^WantedBy=' "$units/luthier.service"
check "luthier-resume.service starts at login" \
  grep -qx 'WantedBy=default.target' "$units/luthier-resume.service"

echo "== XDG directories"
apply=$(apply_script "$bg")
check "with xdg.enable, luthier is given Home Manager's XDG_DATA_HOME" \
  grep -q "XDG_DATA_HOME=$T/home/data" "$apply"
check "without it, luthier is given none" \
  lacks XDG_DATA_HOME "$(apply_script "$plain")"

echo "== Applying in the background"
runner=$(sed -n 's/^ExecStart=//p' "$units/luthier.service")
mkdir -p "$state"
ln -sfn "$apply" "$state/wanted"
status=$(run_service "$runner" "$T/log/first")
check "the first run succeeds" test "$status" = 0
check "it says the packages are installed" \
  grep -q 'The declared packages are installed.' "$T/log/first"
check "it is recorded as applied" test "$(cat "$state/applied" 2>/dev/null)" = "$apply"
check "the plugin is installed" test -f "$T/home/.clap/matsuri-v2-ubuntu.clap"
check "the state is under Home Manager's XDG_DATA_HOME" test -d "$T/home/data/luthier/state"
check "nothing is under the default one" test ! -e "$T/home/.local/share/luthier"
check "a login shell's luthier lists the package" lists_matsuri

status=$(run_service "$runner" "$T/log/again")
check "a second run succeeds" test "$status" = 0
check "and does nothing" test ! -s "$T/log/again"

echo "== A location that cannot be set"
apply=$(apply_script "$ml")
runner=$(sed -n 's/^ExecStart=//p' "$(unit_dir "$ml")/luthier.service")
ln -sfn "$apply" "$state/wanted"
status=$(run_service "$runner" "$T/log/missing")
check "the run fails" test "$status" = 1
check "it says nothing is imported" grep -q 'nothing is imported' "$T/log/missing"
check "and imports nothing" lacks 'Nothing to do' "$T/log/missing"
check "it is not recorded as applied" test ! -e "$state/applied"

mkdir "$T/samples"
status=$(run_service "$runner" "$T/log/mounted")
check "once the directory exists, the run succeeds" test "$status" = 0
check "it is recorded as applied" test "$(cat "$state/applied" 2>/dev/null)" = "$apply"
check "the location is set" libraries_in "$T/samples"

echo "== Applying during activation"
# The block Home Manager's activation script runs, with the two helpers it
# relies on, under the options activation sets.
{
  echo 'set -eu -o pipefail'
  echo 'run() { "$@"; }'
  echo 'warnEcho() { echo "warning: $*" >&2; }'
  sed -n '/^luthier() {/,/^fi$/p' "$act/activate"
  echo 'echo "the rest of the switch goes ahead"'
} >"$T/activation-block.sh"
status=0
env -i HOME="$T/home" "$bash" "$T/activation-block.sh" >"$T/log/activation" 2>&1 || status=$?
check "a failed location does not fail the switch" test "$status" = 0
check "it is warned about" grep -q 'warning: luthier location set' "$T/log/activation"
check "nothing is imported" grep -q 'nothing is imported' "$T/log/activation"
check "the rest of the switch runs" \
  grep -q 'the rest of the switch goes ahead' "$T/log/activation"

if [ "$failures" -gt 0 ]; then
  echo
  echo "$failures check(s) failed; the logs are in $T/log"
  exit 1
fi
rm -rf "$T"
echo
echo "All checks passed."
