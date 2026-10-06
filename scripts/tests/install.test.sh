#!/usr/bin/env bash
#
# Checks the parts of scripts/install.sh that can be checked without touching the machine: dotted
# version comparison, option parsing, and the two safety properties the script promises.
#
# The version rows are the point. The glibc floor is 2.28, and a string compare ranks "2.9" above it,
# so a naive test would pass a host whose engine cannot start. The rows below pin the numeric
# behaviour, including the multi-digit component that breaks lexical ordering.
#
# The safety rows pin what must never regress: --yes implies both consents, and neither trust nor
# Homebrew bootstrap is enabled by default. A future edit that made either default would grant trust
# on a user's machine without asking, which the README forbids.
#
# The script is sourced with KTSENSE_INSTALL_SOURCE_ONLY=1 so main never runs here.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
script="$here/../install.sh"

[[ -x $script ]] || { printf 'not executable: %s\n' "$script" >&2; exit 1; }

# shellcheck disable=SC1090  # the path is computed, and sourcing is the point of this test
KTSENSE_INSTALL_SOURCE_ONLY=1 source "$script"

expected=""
actual=""

# have|want|expected_exit  (0 means have >= want)
version_cases=(
    "2.28|2.28|0"
    "2.29|2.28|0"
    "2.34|2.28|0"
    "3.0|2.28|0"
    "2.26|2.28|1"
    "2.17|2.28|1"
    "2.9|2.28|1"
    "2.5|2.28|1"
    "1.99|2.28|1"
    "2.28.1|2.28|0"
    "2.27.9|2.28|1"
    "2|2.28|1"
    "2.28-stable|2.28|0"
    "2.36-9+deb12u4|2.28|0"
    "2.08|2.28|1"
    "2.030|2.28|0"
)

for row in "${version_cases[@]}"; do
    IFS='|' read -r have want want_exit <<<"$row"
    got_exit=0
    version_ge "$have" "$want" || got_exit=1
    expected+="version_ge $have $want -> $want_exit"$'\n'
    actual+="version_ge $have $want -> $got_exit"$'\n'
done

# args|bootstrap_brew|trust_tap|assume_yes|skip_ripgrep|expected_parse_exit
arg_cases=(
    "|0|0|0|0|0"
    "--trust-tap|0|1|0|0|0"
    "--bootstrap-brew|1|0|0|0|0"
    "--skip-ripgrep|0|0|0|1|0"
    "--yes|1|1|1|0|0"
    "-y|1|1|1|0|0"
    "--bootstrap-brew --trust-tap|1|1|0|0|0"
    "--yes --skip-ripgrep|1|1|1|1|0"
    "--nope|0|0|0|0|1"
)

for row in "${arg_cases[@]}"; do
    IFS='|' read -r args w_boot w_trust w_yes w_skip w_exit <<<"$row"
    bootstrap_brew=0; trust_tap=0; assume_yes=0; skip_ripgrep=0
    got_exit=0
    # shellcheck disable=SC2086
    parse_args $args >/dev/null 2>&1 || got_exit=$?
    expected+="parse [$args] -> exit $w_exit boot $w_boot trust $w_trust yes $w_yes skip $w_skip"$'\n'
    actual+="parse [$args] -> exit $got_exit boot $bootstrap_brew trust $trust_tap yes $assume_yes skip $skip_ripgrep"$'\n'
done

bootstrap_brew=0; trust_tap=0; assume_yes=0; skip_ripgrep=0
parse_args >/dev/null 2>&1
expected+="default grants no trust -> yes"$'\n'
actual+="default grants no trust -> $( ((trust_tap == 0)) && echo yes || echo no )"$'\n'
expected+="default bootstraps nothing -> yes"$'\n'
actual+="default bootstraps nothing -> $( ((bootstrap_brew == 0)) && echo yes || echo no )"$'\n'

parse_args --help >/dev/null 2>&1 && help_exit=0 || help_exit=$?
expected+="--help -> 10"$'\n'
actual+="--help -> $help_exit"$'\n'

expected+="script declines to auto-trust in its own text -> yes"$'\n'
actual+="script declines to auto-trust in its own text -> $(grep -q 'Trust is never granted silently' "$script" && echo yes || echo no)"$'\n'

if [[ $expected == "$actual" ]]; then
    printf 'install.sh helpers: %d version rows, %d arg rows, all as expected\n' \
        "${#version_cases[@]}" "${#arg_cases[@]}"
    exit 0
fi

printf 'install.sh helper check FAILED\n\n--- expected\n%s\n--- actual\n%s\n' "$expected" "$actual" >&2
exit 1
