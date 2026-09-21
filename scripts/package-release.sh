#!/usr/bin/env bash
#
# Assemble the release layout for one target, optionally ad-hoc sign on Darwin, tar it
# deterministically, emit a sha256 manifest, and verify the tar listing matches what was staged.
#
#   scripts/package-release.sh --target <triple> --version <ver> \
#       --bin <ktsense> --engine <dir> --dest <dir> [--sign adhoc|none]
#
# The staged tree is bin/ktsense beside libexec/kmp-lsp so that discovery's
# <exe-dir>/../libexec/kmp-lsp rule resolves the bundled engine after extraction, with no
# KTSENSE_LSP_PATH set. The engine directory is produced by scripts/fetch-upstream-engine.sh; this
# script never touches the network.
#
# Exits non-zero on a missing input, a sidecar whose architecture does not match the target, a
# requested Darwin signature without codesign, or a tar listing that diverges from the staged tree.

set -euo pipefail

die() { printf 'package-release: %s\n' "$*" >&2; exit 1; }
note() { printf ':: %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }

usage() {
    cat << 'USAGE'
package-release.sh --target <triple> --version <ver> --bin <ktsense> --engine <dir> --dest <dir>
                   [--sign adhoc|none]

  --target  Rust target triple the artifact is built for
  --version release version, without the leading v
  --bin     path to the built ktsense executable
  --engine  directory holding kmp-lsp and, when the target keeps it, kmp-jar-indexer
  --dest    output directory for the staged tree, the tarball and the sha256 manifest
  --sign    adhoc ad-hoc signs the executables with codesign (Darwin only); none (default) skips
USAGE
}

target=""
version=""
bin=""
engine=""
dest=""
sign="none"

while [ $# -gt 0 ]; do
    case "$1" in
        --target) [ -n "${2:-}" ] || die "--target needs a value"; target="$2"; shift 2 ;;
        --version) [ -n "${2:-}" ] || die "--version needs a value"; version="$2"; shift 2 ;;
        --bin) [ -n "${2:-}" ] || die "--bin needs a value"; bin="$2"; shift 2 ;;
        --engine) [ -n "${2:-}" ] || die "--engine needs a value"; engine="$2"; shift 2 ;;
        --dest) [ -n "${2:-}" ] || die "--dest needs a value"; dest="$2"; shift 2 ;;
        --sign) [ -n "${2:-}" ] || die "--sign needs a value"; sign="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$target" ] || { usage >&2; die "--target is required"; }
[ -n "$version" ] || { usage >&2; die "--version is required"; }
[ -n "$bin" ] || { usage >&2; die "--bin is required"; }
[ -n "$engine" ] || { usage >&2; die "--engine is required"; }
[ -n "$dest" ] || { usage >&2; die "--dest is required"; }
[ -f "$bin" ] || die "no ktsense binary at $bin"
[ -f "$engine/kmp-lsp" ] || die "no kmp-lsp in engine directory $engine"

case "$sign" in
    adhoc | none) ;;
    *) die "--sign must be adhoc or none, got '$sign'" ;;
esac

name="ktsense-$version-$target"
stage="$dest/$name"
tarball="$dest/$name.tar.gz"
manifest="$dest/$name.tar.gz.sha256"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A fixed timestamp both GNU and BSD touch accept, so the tarball is reproducible across runs.
readonly EPOCH="200001010000"

sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1"
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1"
    else
        die "no sha256 tool found, need sha256sum or shasum"
    fi
}

# The Intel darwin tarball ships an arm64 kmp-jar-indexer upstream (KT-41): there is no working
# x86_64 sidecar, so shipping the arm64 one under an Intel label would silently break Gradle-jar
# indexing on Intel Macs. Every other target keeps a matching-architecture sidecar when present.
stage_sidecar() {
    if [ "$target" = "x86_64-apple-darwin" ]; then
        note "omitting kmp-jar-indexer for $target: upstream ships an arm64 binary in the Intel tarball (KT-41); source queries do not need it"
        return 0
    fi
    local side="$engine/kmp-jar-indexer"
    if [ ! -f "$side" ]; then
        note "no kmp-jar-indexer in $engine; Gradle-jar symbol indexing will be absent for $target"
        return 0
    fi
    assert_sidecar_arch "$side"
    install -m 0755 "$side" "$stage/libexec/kmp-jar-indexer"
    ok "libexec/kmp-jar-indexer"
}

assert_sidecar_arch() {
    local path="$1" desc want
    command -v file > /dev/null 2>&1 || return 0
    desc="$(file -b "$path")"
    case "$desc" in
        *Mach-O* | *ELF*) ;;
        *) return 0 ;;
    esac
    case "$target" in
        aarch64-*) want='arm64|aarch64' ;;
        x86_64-*) want='x86.64|x86_64' ;;
        *) return 0 ;;
    esac
    printf '%s' "$desc" | grep -Eq "$want" \
        || die "kmp-jar-indexer architecture ($desc) does not match target $target; refusing a mislabelled sidecar (KT-41)"
}

sign_adhoc() {
    case "$target" in
        *-apple-darwin) ;;
        *) die "--sign adhoc is only valid for an *-apple-darwin target, got $target" ;;
    esac
    command -v codesign > /dev/null 2>&1 || die "--sign adhoc needs codesign, not found"
    local file
    for file in "$stage/bin/ktsense" "$stage"/libexec/*; do
        [ -f "$file" ] || continue
        codesign --sign - --force --timestamp=none "$file"
        codesign -dv "$file"
        ok "signed $file"
    done
}

make_tarball() {
    find "$stage" -exec touch -t "$EPOCH" {} +
    (cd "$dest" && find "$name" | LC_ALL=C sort > "$work/entries")
    if tar --version 2>/dev/null | grep -q 'GNU tar'; then
        tar --no-recursion --numeric-owner --owner=0 --group=0 \
            --format=ustar -C "$dest" -T "$work/entries" -cf "$work/out.tar"
    else
        tar --no-recursion --numeric-owner --uid 0 --gid 0 --uname '' --gname '' \
            -C "$dest" -T "$work/entries" -cf "$work/out.tar"
    fi
    gzip -n -9 -c "$work/out.tar" > "$tarball"
    ok "$tarball"
}

verify_listing() {
    local expected actual
    expected="$(cd "$dest" && find "$name" | sed 's:/*$::' | LC_ALL=C sort)"
    actual="$(tar tzf "$tarball" | sed 's:/*$::' | LC_ALL=C sort)"
    [ "$expected" = "$actual" ] \
        || die "tar listing does not match the staged layout
staged:
$expected
archived:
$actual"
    ok "tar listing matches the staged layout"
}

rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/libexec"
install -m 0755 "$bin" "$stage/bin/ktsense"
ok "bin/ktsense"
install -m 0755 "$engine/kmp-lsp" "$stage/libexec/kmp-lsp"
ok "libexec/kmp-lsp"
stage_sidecar

if [ "$sign" = "adhoc" ]; then
    sign_adhoc
fi

make_tarball
verify_listing

(cd "$dest" && sha256_of "$name.tar.gz" > "$name.tar.gz.sha256")
ok "$manifest"
note "packaged $name from engine $engine"
