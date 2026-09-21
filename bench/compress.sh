#!/usr/bin/env bash
#
# compress.sh - report ktsense skeleton compression over a Kotlin source tree.
#
# Usage:
#   bench/compress.sh <root> [--max-percent N]
#
# Aggregated over every .kt source file under <root> (see File selection below),
# prints:
#   raw bytes        total source size on disk
#   skeleton bytes   total size of `ktsense outline` Markdown output
#   estimated tokens ceil(skeleton bytes / 3.6): the product's ByteRatioEstimator.
#                    This is an ESTIMATE, not an exact tokenizer count.
#   skeleton percent 100 * skeleton / raw (how much of the source is retained)
#   reduction        100 - skeleton percent (how much is removed)
#
# --max-percent N   Optional CI gate: exit 1 if the skeleton is N% or more of raw.
#                   Omit it to report only. The publishable "under 30% of raw"
#                   bar applies to real, body-heavy corpora (plan Gate 2 / KT-25),
#                   e.g. --max-percent 30 on bench/repos/kotlinx.coroutines. The
#                   signature-dense fixtures/tiny-app is grammar/render coverage,
#                   not a corpus: it honestly measures ~46.5% of raw and carries
#                   no <30% gate.
#
# File selection    Measures .kt Kotlin source only. .kts Gradle/Kotlin build
#                   scripts are deliberately excluded: they are configuration
#                   DSL with no declarations, so `ktsense outline` renders them
#                   "No public declarations". Counting their raw bytes would pad
#                   the denominator and inflate the reduction, so they are left
#                   out of both raw and skeleton totals (symmetric treatment).
#                   Generated and tooling trees (build, bin, target, .gradle,
#                   .git, .idea, .settings, out, node_modules) are pruned so a
#                   stray generated copy of a source file cannot double-count.
#
# The ktsense binary is located, in order, from:
#   $KTSENSE_BIN, <repo>/target/release/ktsense, <repo>/target/debug/ktsense,
#   then `ktsense` on PATH. This script never builds; build ktsense-cli first
#   (for example: cargo build -p ktsense-cli) or set KTSENSE_BIN.
#
# Deterministic and offline: it reads only local files and outline output, and
# the reported totals do not depend on file iteration order. Outline runs with
# paths relative to <root> so the report is identical on any machine.
#
# Rejected files      A file `ktsense outline` refuses (a syntax error the parser
#                   cannot recover any declaration from) is excluded from BOTH
#                   totals, the same symmetric treatment .kts files get, and every
#                   such file is listed in the report with the reason, so nothing
#                   is dropped silently and the percent is honest about what it
#                   covers. Rejections do not fail the run: the exit status is
#                   reserved for the --max-percent gate, so the gate can be
#                   demonstrated on a corpus with a handful of files the grammar
#                   cannot yet parse (tracked as KT-52).
#
# Recovered files     A file with a LOCALIZED parse error is no longer rejected:
#                   `ktsense outline` recovers the declarations that parsed around
#                   the error and marks the output partial (KT-52a). Such a file
#                   exits 0, so it enters BOTH the raw and skeleton totals like any
#                   measured file, and is additionally tallied and listed as
#                   "recovered (partial)" with its raw byte count, so the report
#                   states how many of the measured bytes came from partial
#                   recovery rather than a complete parse.
#
# Exit status: 0 on a report, or a gate that passes; 1 when the gate fails or no
# file could be measured; 2 on a usage error.

set -euo pipefail

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

root=""
max_percent=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --max-percent)
            [ "$#" -ge 2 ] || { echo "compress.sh: --max-percent needs a value" >&2; exit 2; }
            max_percent=$2
            shift 2
            ;;
        --) shift; break ;;
        -*) echo "compress.sh: unknown option: $1" >&2; usage 2 ;;
        *)
            [ -z "$root" ] || { echo "compress.sh: unexpected argument: $1" >&2; usage 2; }
            root=$1
            shift
            ;;
    esac
done

[ -n "$root" ] || { echo "compress.sh: missing <root>" >&2; usage 2; }
[ -d "$root" ] || { echo "compress.sh: not a directory: $root" >&2; exit 2; }

resolve_bin() {
    if [ -n "${KTSENSE_BIN:-}" ]; then
        command -v -- "$KTSENSE_BIN" && return 0
        echo "compress.sh: KTSENSE_BIN is set but not executable: $KTSENSE_BIN" >&2
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
    echo "compress.sh: no ktsense binary found. Build ktsense-cli or set KTSENSE_BIN." >&2
    exit 2
fi

root_abs=$(cd "$root" && pwd)

raw_total=0
skel_total=0
file_count=0
rejected_count=0
rejected_raw=0
recovered_count=0
recovered_raw=0
skel_tmp=$(mktemp)
err_tmp=$(mktemp)
rejected_tmp=$(mktemp)
recovered_tmp=$(mktemp)
trap 'rm -f "$skel_tmp" "$err_tmp" "$rejected_tmp" "$recovered_tmp"' EXIT

cd "$root_abs"
while IFS= read -r -d '' file; do
    file=${file#./}
    raw=$(wc -c < "$file")
    if ! "$bin" outline "$file" > "$skel_tmp" 2> "$err_tmp"; then
        reason=$(head -n 1 "$err_tmp" | sed -e 's/^ktsense: //' -e "s|^$file ||")
        printf '  %s: %s\n' "$file" "${reason:-outline exited non-zero}" >> "$rejected_tmp"
        rejected_count=$((rejected_count + 1))
        rejected_raw=$((rejected_raw + raw))
        continue
    fi
    skel=$(wc -c < "$skel_tmp")
    raw_total=$((raw_total + raw))
    skel_total=$((skel_total + skel))
    file_count=$((file_count + 1))
    if grep -q '^// partial:' "$skel_tmp"; then
        printf '  %s: %d raw bytes\n' "$file" "$raw" >> "$recovered_tmp"
        recovered_count=$((recovered_count + 1))
        recovered_raw=$((recovered_raw + raw))
    fi
done < <(find . \
    \( -type d \( -name .git -o -name .gradle -o -name .idea -o -name .settings \
        -o -name build -o -name bin -o -name target -o -name out \
        -o -name node_modules \) -prune \) \
    -o \( -type f -name '*.kt' -print0 \))

if [ "$file_count" -eq 0 ]; then
    if [ "$rejected_count" -gt 0 ]; then
        echo "compress.sh: every .kt file under $root was rejected by outline:" >&2
        cat "$rejected_tmp" >&2
    else
        echo "compress.sh: no .kt files under $root" >&2
    fi
    exit 1
fi

read -r est_tokens skel_pct reduction_pct <<EOF
$(awk -v raw="$raw_total" -v skel="$skel_total" 'BEGIN {
    t = skel / 3.6; ct = int(t); if (ct < t) ct++;
    pct = (raw > 0) ? 100 * skel / raw : 0;
    printf "%d %.2f %.2f", ct, pct, 100 - pct;
}')
EOF

printf 'ktsense compression report\n'
printf 'root:              %s\n' "$root"
printf 'files (.kt):       %d measured (%d partial), %d rejected\n' \
    "$file_count" "$recovered_count" "$rejected_count"
printf 'raw bytes:         %d  (measured files only)\n' "$raw_total"
printf 'skeleton bytes:    %d\n' "$skel_total"
printf 'estimated tokens:  %d  (skeleton, ceil(bytes/3.6), estimate)\n' "$est_tokens"
printf 'skeleton is %s%% of raw source\n' "$skel_pct"
printf 'reduction:         %s%%\n' "$reduction_pct"
if [ "$recovered_count" -gt 0 ]; then
    printf 'recovered (partial, %d files, %d raw bytes, counted in both totals):\n' \
        "$recovered_count" "$recovered_raw"
    cat "$recovered_tmp"
fi
if [ "$rejected_count" -gt 0 ]; then
    printf 'rejected by outline (%d files, %d raw bytes, excluded from both totals):\n' \
        "$rejected_count" "$rejected_raw"
    cat "$rejected_tmp"
fi

if [ -n "$max_percent" ]; then
    if awk -v p="$skel_pct" -v m="$max_percent" 'BEGIN { exit !(p >= m) }'; then
        echo "compress.sh: FAIL skeleton is ${skel_pct}% of raw, not under ${max_percent}%" >&2
        exit 1
    fi
    printf 'gate: PASS skeleton under %s%% of raw\n' "$max_percent"
fi
