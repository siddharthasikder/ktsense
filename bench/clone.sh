#!/usr/bin/env bash
#
# clone.sh - fetch external Kotlin corpora for benchmarking into bench/repos/.
#
# Usage:
#   bench/clone.sh                     Clone the pinned default corpora.
#   bench/clone.sh <url> <ref> <dest>  Clone one repo at an explicit pinned ref.
#
# Default corpora (pinned tags; pass explicit args to override or add others):
#   https://github.com/Kotlin/kotlinx.coroutines  1.9.0   kotlinx.coroutines
#   https://github.com/ktorio/ktor                3.0.1   ktor
#
# Network access is required for THIS script only. compress.sh runs entirely
# offline on the committed fixtures, so the fixture benchmark never needs a
# clone. Destinations live under bench/repos/, which .gitignore excludes.
#
# Clones are shallow (--depth 1) at a pinned tag and idempotent: an existing
# destination is left untouched so re-runs and CI caches are cheap. Runs
# non-interactively (GIT_TERMINAL_PROMPT=0), so a missing credential or a bad
# ref fails fast instead of prompting.

set -euo pipefail

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

export GIT_TERMINAL_PROMPT=0

script_dir=$(cd "$(dirname "$0")" && pwd)
repos_dir="$script_dir/repos"

clone_one() {
    url=$1
    ref=$2
    name=$3
    dest="$repos_dir/$name"
    if [ -e "$dest" ]; then
        echo "clone.sh: $dest already exists, skipping" >&2
        return 0
    fi
    mkdir -p "$repos_dir"
    echo "clone.sh: cloning $url @ $ref -> $dest" >&2
    git clone --depth 1 --single-branch --branch "$ref" -- "$url" "$dest"
}

case "${1:-}" in
    -h|--help) usage 0 ;;
esac

if [ "$#" -eq 0 ]; then
    clone_one "https://github.com/Kotlin/kotlinx.coroutines" "1.9.0" "kotlinx.coroutines"
    clone_one "https://github.com/ktorio/ktor" "3.0.1" "ktor"
elif [ "$#" -eq 3 ]; then
    clone_one "$1" "$2" "$3"
else
    echo "clone.sh: expected no arguments or exactly <url> <ref> <dest>" >&2
    usage 2
fi
