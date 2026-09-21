#!/usr/bin/env bash
#
# Table-driven check of scripts/release-version.sh: every accepted version normalizes to the printed
# value with exit 0, and every rejected version produces empty stdout with a nonzero exit. The whole
# transcript is composed once and compared once, so a single mismatch shows the full expected-vs-got.
#
# The reject rows are the point of KT-59: +build metadata, prerelease+build, and empty prerelease
# identifiers must all fail, alongside paths, whitespace, slashes and shell metacharacters. Inputs
# are only ever passed as a quoted argument, never evaluated, so the metacharacter rows cannot run.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
helper="$here/../release-version.sh"

# input|expected_exit|expected_stdout  (stdout is empty for every reject)
cases=(
    "0.1.0|0|0.1.0"
    "v0.1.0|0|0.1.0"
    "0.1.0-rc.1|0|0.1.0-rc.1"
    "1.2.3-alpha.1|0|1.2.3-alpha.1"
    "|1|"
    "v|1|"
    "0.1|1|"
    "1.2.3.4|1|"
    "+build|1|"
    "1.2.3+build.5|1|"
    "1.2.3-rc.1+build|1|"
    "../etc/passwd|1|"
    "1.2.3 |1|"
    "0.1.0/x|1|"
    "1.2.3; rm -rf /|1|"
    "\$(touch pwned)|1|"
    "1.2.3-|1|"
    "1.2.3-.|1|"
    "1.2.3-a..b|1|"
)

expected=""
actual=""
for case in "${cases[@]}"; do
    IFS='|' read -r input want_rc want_out <<<"$case"
    got_out="$("$helper" "$input" 2>/dev/null)" && got_rc=0 || got_rc=$?
    expected+="[$input] rc=$want_rc out=$want_out"$'\n'
    actual+="[$input] rc=$got_rc out=$got_out"$'\n'
done

if [ -e pwned ]; then
    printf 'FAIL: a metacharacter input executed (pwned exists)\n' >&2
    exit 1
fi

if [ "$actual" != "$expected" ]; then
    printf 'expected:\n%s\ngot:\n%s\n' "$expected" "$actual" >&2
    printf 'FAIL: release-version transcript mismatch\n' >&2
    exit 1
fi

printf 'all release-version tests passed (%d cases)\n' "${#cases[@]}"
