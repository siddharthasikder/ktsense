#!/usr/bin/env bash
#
# Proves the KTSENSE_BUILD_VERSION compile-time contract end to end, the only way a compile-time env
# can be checked: by building the binary and reading what it reports.
#
#   - a development build (no override) reports the workspace manifest version, 0.0.0
#   - a release build (KTSENSE_BUILD_VERSION set) reports that exact version
#   - reusing the same target directory, changing the override forces a recompile and a new report,
#     which is the cache-correctness claim Cargo's option_env! dep-info tracking makes
#
# Everything builds into an isolated CARGO_TARGET_DIR so the repository's own target/ is never
# touched and no override leaks into a later build. No source or Cargo.toml is mutated. The output is
# pinned exactly, clap's "ktsense " program-name prefix included.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH='' cd -- "$here/../.." && pwd)"
target_dir="$(mktemp -d)"
trap 'rm -rf "$target_dir"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

bin="$target_dir/debug/ktsense"

build() {
    ( cd "$repo_root" && CARGO_TARGET_DIR="$target_dir" "$@" \
        cargo build --locked -p ktsense-cli ) > /dev/null
}

reported() {
    "$bin" --version
}

build
got="$(reported)"
[ "$got" = "ktsense 0.0.0" ] || fail "dev build reported '$got', expected 'ktsense 0.0.0'"
pass "development build reports the manifest version"

build env KTSENSE_BUILD_VERSION=0.1.0-rc.1
got="$(reported)"
[ "$got" = "ktsense 0.1.0-rc.1" ] \
    || fail "override build reported '$got', expected 'ktsense 0.1.0-rc.1'"
pass "override build reports the injected version and recompiled from the same target dir"

build env KTSENSE_BUILD_VERSION=0.2.0
got="$(reported)"
[ "$got" = "ktsense 0.2.0" ] \
    || fail "changed override reported '$got', expected 'ktsense 0.2.0'"
pass "changing the override in place recompiles rather than serving a cached version"

printf 'all build-version tests passed\n'
