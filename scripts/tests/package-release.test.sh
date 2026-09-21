#!/usr/bin/env bash
#
# Tests for scripts/package-release.sh that do not need a network or a real engine:
#   - the staged layout and tar listing for every target triple, including the Intel-darwin
#     sidecar exclusion required by KT-41
#   - byte-for-byte reproducibility of the tarball across two runs
#
# It fabricates a stand-in binary and sidecar, so it runs anywhere tar and bash do. Darwin signing
# is not exercised here: --sign adhoc needs codesign, which only the macOS release runners carry.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
script="$here/../package-release.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

engine="$work/engine"
mkdir -p "$engine"
printf '#!/bin/sh\necho stub\n' > "$engine/kmp-lsp"
printf '#!/bin/sh\necho stub\n' > "$engine/kmp-jar-indexer"
chmod +x "$engine/kmp-lsp" "$engine/kmp-jar-indexer"

bin="$work/ktsense"
printf '#!/bin/sh\necho stub\n' > "$bin"
chmod +x "$bin"

version="9.9.9"

stage_and_list() {
    local target="$1" dest
    dest="$work/dest-$target"
    rm -rf "$dest"
    bash "$script" --target "$target" --version "$version" \
        --bin "$bin" --engine "$engine" --dest "$dest" > /dev/null
    tar tzf "$dest/ktsense-$version-$target.tar.gz" | sed 's:/*$::' | LC_ALL=C sort | grep -v '^$'
}

expected_with_sidecar() {
    local target="$1" name="ktsense-$version-$1"
    printf '%s\n' "$name" "$name/bin" "$name/bin/ktsense" \
        "$name/libexec" "$name/libexec/kmp-jar-indexer" "$name/libexec/kmp-lsp" \
        | LC_ALL=C sort
}

expected_without_sidecar() {
    local target="$1" name="ktsense-$version-$1"
    printf '%s\n' "$name" "$name/bin" "$name/bin/ktsense" \
        "$name/libexec" "$name/libexec/kmp-lsp" \
        | LC_ALL=C sort
}

check_layout() {
    local target="$1" want="$2" got
    got="$(stage_and_list "$target")"
    if [ "$got" != "$want" ]; then
        printf 'expected:\n%s\ngot:\n%s\n' "$want" "$got" >&2
        fail "layout for $target"
    fi
    pass "layout for $target"
}

# Linux and aarch64-darwin carry the sidecar; only x86_64-apple-darwin drops it (KT-41).
check_layout x86_64-unknown-linux-musl "$(expected_with_sidecar x86_64-unknown-linux-musl)"
check_layout aarch64-unknown-linux-musl "$(expected_with_sidecar aarch64-unknown-linux-musl)"
check_layout aarch64-apple-darwin "$(expected_with_sidecar aarch64-apple-darwin)"
check_layout x86_64-apple-darwin "$(expected_without_sidecar x86_64-apple-darwin)"

# Reproducibility: two independent runs into different destinations produce identical archives.
target="x86_64-unknown-linux-musl"
name="ktsense-$version-$target"
first="$work/repro-a"
second="$work/repro-b"
bash "$script" --target "$target" --version "$version" \
    --bin "$bin" --engine "$engine" --dest "$first" > /dev/null
bash "$script" --target "$target" --version "$version" \
    --bin "$bin" --engine "$engine" --dest "$second" > /dev/null
sum_a="$(sha256sum < "$first/$name.tar.gz")"
sum_b="$(sha256sum < "$second/$name.tar.gz")"
[ "$sum_a" = "$sum_b" ] || fail "tarball is not reproducible: $sum_a vs $sum_b"
pass "tarball is reproducible"

# The manifest names the asset and records its checksum.
grep -Fq "$name.tar.gz" "$first/$name.tar.gz.sha256" || fail "manifest does not name the asset"
pass "manifest names the asset"

printf 'all package-release tests passed\n'
