#!/usr/bin/env bash
#
# latency.sh - per-command latency of ktsense over a Kotlin source tree, as CSV.
#
# Usage:
#   bench/latency.sh <root> [options]
#
#   --reps N          Timed repetitions per row (default 5). One extra untimed
#                     warm-up invocation runs first and is discarded.
#   --file PATH       File for `outline`, relative to <root>. Default: the
#                     largest .kt file under <root>, so the reported outline
#                     latency is that corpus's worst case.
#   --symbol NAME     Symbol for `symbols` and `trace`. Default: the first
#                     top-level declaration of --file, so the default is derived
#                     from the corpus and is certain to exist.
#   --settle SECS     Seconds to let a daemon this script started finish indexing
#                     before the daemon rows are timed (default 5). `daemon
#                     start` returns as soon as the socket answers, which is
#                     before the engine's index is ready.
#   --gate CMD=MS     Repeatable. Require CMD's DAEMON-path median under MS ms.
#                     A command with no daemon path reports BLOCKED and exits 1;
#                     a gate that cannot be evaluated has not been met.
#   --keep-daemon     Leave a daemon this script started running.
#
# Output          CSV on stdout, one row per command and path:
#
#   command,path,reps,best_ms,median_ms,worst_ms,exit,out_bytes,load1,note
#
#                 `median_ms` is the statistic the gates read; best and worst
#                 bound it. `load1` is the 1-minute load average sampled when
#                 the row was taken, because a number from a saturated host is
#                 not a measurement. `out_bytes` is the last repetition's stdout
#                 size, which makes an answer that grew pathologically (see
#                 `trace` in AGENTS.md) visible next to its timing.
#
#                 Everything that is not CSV - the corpus report, the gate
#                 verdicts, warnings - goes to stderr, so
#                 `bench/latency.sh <root> > latency.csv` yields a clean file.
#
# Paths           `in_process` forces the in-process path (KTSENSE_NO_DAEMON=1):
#                 the card's "cold" column. It is not a cold INDEX; the engine's
#                 on-disk cache is whatever the host already had, and genuinely
#                 cold index time is bench/cold-index.py (KT-53).
#                 `daemon` requires a live daemon to answer
#                 (KTSENSE_REQUIRE_DAEMON=1), so a silent fallback can never be
#                 reported as a daemon number.
#
#                 Only `outline` and `deps` have a daemon path at all. `symbols`
#                 is exempt by design: its backend is the engine's command-mode
#                 `find`, and `workspace/symbol` is fuzzy and not root-scoped on
#                 this engine, so a warm session has nothing better to offer.
#                 `trace` and `map` are simply not routed yet. Their daemon rows
#                 report `na` with the reason in `note` rather than repeating the
#                 in-process number under a second name.
#
# Corpus integrity
#                 Running kmp-lsp over a Gradle project triggers an Eclipse
#                 Buildship import that writes bin/main/ containing byte-identical
#                 copies of the .kt sources. ktsense's own traversal does not
#                 prune `bin`, so on an imported corpus `deps` and `map` walk
#                 every file twice and their latency is inflated. This script
#                 prunes `bin` in its own file discovery and reports the
#                 duplicate count; rows whose command walks the whole tree carry
#                 the `bin_duplicates` note when the corpus is affected (KT-55).
#
# The ktsense binary is located, in order, from:
#   $KTSENSE_BIN, <repo>/target/release/ktsense, <repo>/target/debug/ktsense,
#   then `ktsense` on PATH. This script never builds; build ktsense-cli first
#   (for example: cargo build --release -p ktsense-cli) or set KTSENSE_BIN.
#
# Exit status: 0 on a report, or when every requested gate passes; 1 when a gate
# fails or is blocked, or when nothing could be measured; 2 on a usage error.

set -euo pipefail

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

reps=5
outline_file=""
symbol=""
settle=5
keep_daemon=0
root=""
gates=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --reps)
            [ "$#" -ge 2 ] || { echo "latency.sh: --reps needs a value" >&2; exit 2; }
            reps=$2; shift 2 ;;
        --file)
            [ "$#" -ge 2 ] || { echo "latency.sh: --file needs a value" >&2; exit 2; }
            outline_file=$2; shift 2 ;;
        --symbol)
            [ "$#" -ge 2 ] || { echo "latency.sh: --symbol needs a value" >&2; exit 2; }
            symbol=$2; shift 2 ;;
        --settle)
            [ "$#" -ge 2 ] || { echo "latency.sh: --settle needs a value" >&2; exit 2; }
            settle=$2; shift 2 ;;
        --gate)
            [ "$#" -ge 2 ] || { echo "latency.sh: --gate needs CMD=MS" >&2; exit 2; }
            case "$2" in
                *=''|*=*[!0-9]*|=*) echo "latency.sh: --gate wants CMD=MS, got: $2" >&2; exit 2 ;;
                *=*) gates="$gates $2" ;;
                *) echo "latency.sh: --gate wants CMD=MS, got: $2" >&2; exit 2 ;;
            esac
            shift 2 ;;
        --keep-daemon) keep_daemon=1; shift ;;
        --) shift; break ;;
        -*) echo "latency.sh: unknown option: $1" >&2; usage 2 ;;
        *)
            [ -z "$root" ] || { echo "latency.sh: unexpected argument: $1" >&2; usage 2; }
            root=$1; shift ;;
    esac
done

[ -n "$root" ] || { echo "latency.sh: missing <root>" >&2; usage 2; }
[ -d "$root" ] || { echo "latency.sh: not a directory: $root" >&2; exit 2; }
case "$reps" in
    ''|*[!0-9]*) echo "latency.sh: --reps wants a whole number, got: $reps" >&2; exit 2 ;;
esac
[ "$reps" -ge 1 ] || { echo "latency.sh: --reps must be at least 1" >&2; exit 2; }
case "$settle" in
    ''|*[!0-9]*) echo "latency.sh: --settle wants whole seconds, got: $settle" >&2; exit 2 ;;
esac

resolve_bin() {
    if [ -n "${KTSENSE_BIN:-}" ]; then
        command -v -- "$KTSENSE_BIN" && return 0
        echo "latency.sh: KTSENSE_BIN is set but not executable: $KTSENSE_BIN" >&2
        return 1
    fi
    script_dir=$(cd "$(dirname "$0")" && pwd)
    repo_root=$(cd "$script_dir/.." && pwd)
    for candidate in "$repo_root/target/release/ktsense" "$repo_root/target/debug/ktsense"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    command -v ktsense && return 0
    return 1
}

if ! bin=$(resolve_bin); then
    echo "latency.sh: no ktsense binary found. Build ktsense-cli or set KTSENSE_BIN." >&2
    exit 2
fi

root_abs=$(cd "$root" && pwd)

out_tmp=$(mktemp)
err_tmp=$(mktemp)
rows_tmp=$(mktemp)
samples_tmp=$(mktemp)
started_daemon=0

cleanup() {
    if [ "$started_daemon" -eq 1 ] && [ "$keep_daemon" -eq 0 ]; then
        "$bin" --root "$root_abs" daemon stop >/dev/null 2>&1 || true
    fi
    rm -f "$out_tmp" "$err_tmp" "$rows_tmp" "$samples_tmp"
}
trap cleanup EXIT INT TERM

load1() {
    cut -d' ' -f1 < /proc/loadavg 2>/dev/null || echo na
}

# Prunes `bin` as well as the generated and tooling trees, so a Buildship import
# cannot make the same source file look like two candidates (KT-55).
kotlin_files() {
    (cd "$root_abs" && find . \
        \( -type d \( -name .git -o -name .gradle -o -name .idea -o -name .settings \
            -o -name build -o -name bin -o -name target -o -name out \
            -o -name node_modules \) -prune \) \
        -o \( -type f -name '*.kt' -printf "$1" \))
}

duplicate_count=$(find "$root_abs" -type f -name '*.kt' -path '*/bin/*' | wc -l)
measured_count=$(kotlin_files '%p\0' | tr -dc '\0' | wc -c)
corpus_note=""
if [ "$duplicate_count" -gt 0 ]; then
    corpus_note="bin_duplicates"
fi

if [ -z "$outline_file" ]; then
    outline_file=$(kotlin_files '%s\t%p\n' | awk -F'\t' '
        $1 > max || ($1 == max && $2 < path) { max = $1; path = $2 }
        END { print path }')
    outline_file=${outline_file#./}
    [ -n "$outline_file" ] || { echo "latency.sh: no .kt files under $root" >&2; exit 1; }
fi
[ -f "$root_abs/$outline_file" ] || {
    echo "latency.sh: no such file under $root: $outline_file" >&2
    exit 2
}

if [ -z "$symbol" ]; then
    symbol=$(KTSENSE_NO_DAEMON=1 "$bin" --root "$root_abs" --format json outline "$outline_file" \
        | python3 -c 'import json,sys
declarations = json.load(sys.stdin).get("declarations", [])
print(declarations[0]["name"] if declarations else "")')
    [ -n "$symbol" ] || {
        echo "latency.sh: $outline_file declares nothing to trace; pass --symbol" >&2
        exit 2
    }
fi

if ! KTSENSE_NO_DAEMON=1 "$bin" --root "$root_abs" symbols "$symbol" >/dev/null 2>"$err_tmp"; then
    echo "latency.sh: '$symbol' does not resolve to one declaration; pass --symbol" >&2
    sed 's/^/  /' "$err_tmp" >&2
    exit 2
fi

# Matches on the captured text rather than piping into grep: `grep -q` closes the
# pipe on its first match, which under `pipefail` turns a live daemon's own
# SIGPIPE into a report that no daemon is running.
daemon_live() {
    case $("$bin" --root "$root_abs" daemon status 2>/dev/null || true) in
        "ktsense: daemon running"*) return 0 ;;
    esac
    return 1
}

engine_version=$(kmp-lsp --version 2>/dev/null | head -1 || echo 'not on PATH')

{
    printf 'latency.sh: root %s\n' "$root_abs"
    printf 'latency.sh: binary %s (%s)\n' "$bin" "$("$bin" --version | head -1)"
    printf 'latency.sh: engine %s\n' "$engine_version"
    printf 'latency.sh: corpus %d .kt files measured, %d duplicated under bin/\n' \
        "$measured_count" "$duplicate_count"
    if [ "$duplicate_count" -gt 0 ]; then
        printf 'latency.sh: WARNING the corpus carries %d Buildship source copies under bin/.\n' \
            "$duplicate_count"
        printf '  ktsense does not prune bin, so deps, map and trace see a doubled tree and\n'
        printf '  their latency is inflated. Those rows are noted bin_duplicates (KT-55).\n'
    fi
    printf 'latency.sh: outline file %s (%s bytes)\n' \
        "$outline_file" "$(wc -c < "$root_abs/$outline_file")"
    printf 'latency.sh: symbol %s\n' "$symbol"
    printf 'latency.sh: %d timed repetitions per row, after one discarded warm-up\n' "$reps"
} >&2

if daemon_live; then
    echo "latency.sh: a daemon was already running for this root; leaving it alone" >&2
else
    echo "latency.sh: starting a daemon for the daemon rows" >&2
    "$bin" --root "$root_abs" daemon start >&2
    started_daemon=1
    if [ "$settle" != "0" ]; then
        printf 'latency.sh: letting the daemon index for %ss before timing it\n' "$settle" >&2
        sleep "$settle"
    fi
fi

# `time` is a bash keyword, so it measures the invocation itself with no
# command-substitution fork inside the measured span, unlike a `date` pair.
TIMEFORMAT='%3R'

run_once() {
    "$bin" --root "$root_abs" "$@" > "$out_tmp" 2> "$err_tmp"
}

measure() {
    command_name=$1
    path=$2
    note=$3
    shift 3

    if [ "$path" = daemon ]; then
        export KTSENSE_REQUIRE_DAEMON=1
        unset KTSENSE_NO_DAEMON
    else
        export KTSENSE_NO_DAEMON=1
        unset KTSENSE_REQUIRE_DAEMON
    fi

    load=$(load1)
    "$bin" --root "$root_abs" "$@" >/dev/null 2>&1 || true

    : > "$samples_tmp"
    status=0
    index=0
    while [ "$index" -lt "$reps" ]; do
        elapsed=$( { time run_once "$@"; } 2>&1 ) || status=$?
        printf '%s\n' "$elapsed" >> "$samples_tmp"
        index=$((index + 1))
    done

    bytes=$(wc -c < "$out_tmp")
    read -r best median worst <<EOF
$(sort -n "$samples_tmp" | awk '{ v[NR] = $1 } END {
    mid = int((NR + 1) / 2);
    printf "%.0f %.0f %.0f", v[1] * 1000, v[mid] * 1000, v[NR] * 1000;
}')
EOF
    if [ "$status" -ne 0 ]; then
        note="exit_nonzero${note:+;$note}"
    fi
    printf '%s,%s,%d,%s,%s,%s,%d,%d,%s,%s\n' \
        "$command_name" "$path" "$reps" "$best" "$median" "$worst" \
        "$status" "$bytes" "$load" "$note" >> "$rows_tmp"
    if [ "$status" -ne 0 ]; then
        printf 'latency.sh: WARNING %s on the %s path exited %d; its timing is a time to fail\n' \
            "$command_name" "$path" "$status" >&2
        sed 's/^/  /' "$err_tmp" >&2
    fi
    unset KTSENSE_NO_DAEMON KTSENSE_REQUIRE_DAEMON
}

absent() {
    printf '%s,daemon,0,na,na,na,na,na,%s,%s\n' "$1" "$(load1)" "$2" >> "$rows_tmp"
}

measure outline in_process "" outline "$outline_file"
measure outline daemon "" outline "$outline_file"
measure symbols in_process "$corpus_note" symbols "$symbol"
absent symbols exempt_fork_a
measure trace in_process "$corpus_note" trace "$symbol"
absent trace not_routed
measure map in_process "$corpus_note" map
absent map not_routed
measure deps in_process "$corpus_note" deps
measure deps daemon "$corpus_note" deps

echo 'command,path,reps,best_ms,median_ms,worst_ms,exit,out_bytes,load1,note'
cat "$rows_tmp"

gate_status=0
for gate in $gates; do
    gate_command=${gate%%=*}
    gate_ms=${gate#*=}
    row=$(awk -F, -v c="$gate_command" '$1 == c && $2 == "daemon" { print; exit }' "$rows_tmp")
    if [ -z "$row" ]; then
        printf 'latency.sh: gate BLOCKED %s has no row to gate\n' "$gate_command" >&2
        gate_status=1
        continue
    fi
    median=$(printf '%s' "$row" | cut -d, -f5)
    row_exit=$(printf '%s' "$row" | cut -d, -f7)
    note=$(printf '%s' "$row" | cut -d, -f10)
    if [ "$median" = na ]; then
        printf 'latency.sh: gate BLOCKED %s under %s ms: no daemon path for %s (%s)\n' \
            "$gate_command" "$gate_ms" "$gate_command" "$note" >&2
        gate_status=1
    elif [ "$row_exit" != 0 ]; then
        printf 'latency.sh: gate BLOCKED %s under %s ms: it exited %s, so %s ms is a failure\n' \
            "$gate_command" "$gate_ms" "$row_exit" "$median" >&2
        gate_status=1
    elif [ "$median" -lt "$gate_ms" ]; then
        printf 'latency.sh: gate PASS %s daemon median %s ms under %s ms\n' \
            "$gate_command" "$median" "$gate_ms" >&2
    else
        printf 'latency.sh: gate FAIL %s daemon median %s ms not under %s ms\n' \
            "$gate_command" "$median" "$gate_ms" >&2
        gate_status=1
    fi
done

exit "$gate_status"
