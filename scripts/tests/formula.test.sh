#!/usr/bin/env bash
#
# Tests for Formula/ktsense.rb that need neither a published release nor Homebrew:
#   - the four release URLs name exactly the assets scripts/package-release.sh produces for the
#     four targets the release workflow builds
#   - every path the formula's install block touches exists in a real archive, for every target
#   - the libexec install is a glob, never a named sidecar (KT-41: the Intel darwin archive has no
#     kmp-jar-indexer, so naming it would break that target's install)
#   - the rehearsed install preserves bin/../libexec/kmp-lsp, the archive's self-location contract
#   - the sha256 values are the visible placeholder, not something that could pass for a real hash
#
# It builds archives with a stand-in binary and sidecar, so it runs anywhere tar and bash do. It
# rehearses the install by hand: brew install against a release URL stays a release gate.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
root="$(CDPATH='' cd -- "$here/../.." && pwd)"
formula="$root/Formula/ktsense.rb"
packager="$root/scripts/package-release.sh"
workflow="$root/.github/workflows/release.yml"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

[ -f "$formula" ] || fail "no formula at $formula"

readonly PLACEHOLDER_SHA256="deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"

version="$(sed -n 's/^  RELEASE = "\([^"]*\)".*/\1/p' "$formula")"
[ -n "$version" ] || fail "formula has no RELEASE constant"
pass "formula release version is $version"

# The workflow matrix is the definition of which targets a release publishes.
targets="$(awk '$1 == "-" && $2 == "target:" { print $3 }' "$workflow")"
count="$(printf '%s\n' "$targets" | grep -c .)"
[ "$count" -eq 4 ] || fail "expected 4 release targets in the workflow, found $count: $targets"

# The release path each url resolves to, owner-agnostic so replacing the owner placeholder is free.
formula_paths="$(sed -n 's|^      url "https://[^/]*/[^/]*/[^/]*/releases/download/\(.*\)"$|\1|p' "$formula" \
    | sed "s/#{RELEASE}/$version/g" | LC_ALL=C sort)"

engine="$work/engine"
mkdir -p "$engine"
printf '#!/bin/sh\necho stub\n' > "$engine/kmp-lsp"
printf '#!/bin/sh\necho stub\n' > "$engine/kmp-jar-indexer"
chmod +x "$engine/kmp-lsp" "$engine/kmp-jar-indexer"
bin="$work/ktsense"
printf '#!/bin/sh\necho stub\n' > "$bin"
chmod +x "$bin"
printf 'stub skill\n' > "$work/SKILL.md"
printf 'stub project license\n' > "$work/LICENSE"
printf 'stub upstream license\n' > "$work/LICENSE.kmp-lsp"

# Mirrors the formula's install block. A change there that this does not follow fails below.
rehearse_install() {
    local stage="$1" prefix="$2"
    mkdir -p "$prefix/bin" "$prefix/libexec" "$prefix/share/ktsense"
    mv "$stage/bin/ktsense" "$prefix/bin/ktsense"
    mv "$stage"/libexec/* "$prefix/libexec/"
    mv "$stage/SKILL.md" "$prefix/share/ktsense/SKILL.md"
}

built_paths=""
for target in $targets; do
    asset="ktsense-$version-$target.tar.gz"
    dest="$work/dest-$target"
    bash "$packager" --target "$target" --version "$version" \
        --bin "$bin" --engine "$engine" \
        --skill "$work/SKILL.md" --license "$work/LICENSE" \
        --license-upstream "$work/LICENSE.kmp-lsp" \
        --dest "$dest" > /dev/null
    [ -f "$dest/$asset" ] || fail "package-release.sh did not produce $asset for $target"
    built_paths="$built_paths
v$version/$asset"

    stage="$work/extract-$target"
    mkdir -p "$stage"
    tar xzf "$dest/$asset" -C "$stage" --strip-components 1
    for path in bin/ktsense SKILL.md LICENSE LICENSE.kmp-lsp; do
        [ -e "$stage/$path" ] || fail "$asset has no $path for the formula to install"
    done
    [ -n "$(ls -A "$stage/libexec")" ] || fail "$asset ships an empty libexec/"

    prefix="$work/prefix-$target"
    rehearse_install "$stage" "$prefix"
    [ -x "$prefix/bin/../libexec/kmp-lsp" ] \
        || fail "rehearsed install for $target breaks bin/../libexec/kmp-lsp discovery"
    [ -f "$prefix/share/ktsense/SKILL.md" ] || fail "rehearsed install for $target has no pkgshare skill"
    pass "formula installs $asset and keeps engine discovery intact"
done

built_paths="$(printf '%s\n' "$built_paths" | grep -v '^$' | LC_ALL=C sort)"
if [ "$formula_paths" != "$built_paths" ]; then
    printf 'formula urls:\n%s\npackaged assets:\n%s\n' "$formula_paths" "$built_paths" >&2
    fail "the formula's release urls do not match the assets the release actually publishes"
fi
pass "all four urls name assets package-release.sh produces for the workflow's targets"

# KT-41: the Intel darwin archive carries no kmp-jar-indexer, so the formula must never name it.
# Comments are stripped first: the constraint itself is worth documenting in the formula.
code="$(grep -v '^ *#' "$formula")"
printf '%s' "$code" | grep -Fq 'kmp-jar-indexer' \
    && fail "the formula names kmp-jar-indexer; the x86_64-apple-darwin archive does not ship one (KT-41)"
printf '%s' "$code" | grep -Fq 'libexec.install Dir["libexec/*"]' \
    || fail "the formula must install whatever the archive ships under libexec/"
pass "libexec is installed as a glob, not a named sidecar"

placeholders="$(grep -c "sha256 \"$PLACEHOLDER_SHA256\"" "$formula" || true)"
[ "$placeholders" -eq 4 ] \
    || fail "expected 4 placeholder sha256 values until a release exists, found $placeholders"
pass "all four sha256 values are the visible placeholder"

grep -Fq 'assert_match version.to_s' "$formula" \
    || fail "the test block must assert the binary reports the release version (KT-59)"
grep -Fq 'assert_match "fun greet(): String"' "$formula" \
    || fail "the test block must assert the outline signature"
pass "the test block checks both the outline and the reported version"

printf 'all formula tests passed\n'
