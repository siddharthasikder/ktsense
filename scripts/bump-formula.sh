#!/usr/bin/env bash
#
# Point Formula/ktsense.rb at a published release: fetch the four per-target `.sha256` sidecars for
# <version>, validate every one of them, and only then rewrite the formula's single RELEASE value and
# its four sha256 values, in one atomic replacement.
#
#   scripts/bump-formula.sh <version>        # accepts v0.1.0-rc.1 or 0.1.0-rc.1
#
# The accepted version syntax is not re-implemented here. scripts/release-version.sh owns it (KT-59),
# so a -prerelease suffix is accepted and +build metadata is rejected in exactly one place: Homebrew
# ranks +build tokens above the plain version, which contradicts SemVer, so a formula must never be
# handed one.
#
# The four assets are the ones scripts/package-release.sh produces, named there as
# ktsense-<version>-<target>.tar.gz with a ktsense-<version>-<target>.tar.gz.sha256 manifest beside
# each. The four targets are the release workflow's matrix (KT-42), in formula order.
#
# Nothing is written until all four sidecars are fetched into a temporary directory and each line
# passes validation, and the rewritten formula is verified line by line before it replaces the real
# file. A partial network failure, a malformed hash, a sidecar naming the wrong tarball, or a formula
# whose target blocks are not exactly the four expected ones therefore leaves Formula/ktsense.rb
# byte-identical. Re-running with the same version and the same checksums produces no diff.
#
# Three narrow overrides, all for tests and for an operator fetching from somewhere unusual:
#   KTSENSE_FORMULA_PATH      formula to rewrite; default <repo>/Formula/ktsense.rb
#   KTSENSE_RELEASE_BASE_URL  release download base; default is read out of the formula's own urls so
#                             it follows the formula instead of hardcoding an owner. This script
#                             appends the v<version>/ tag directory, so a server answering
#                             v<version>/<asset>.sha256 from its root is a drop-in for
#                             https://github.com/<owner>/ktsense/releases/download.
#   KTSENSE_DOWNLOADER        auto (default), curl, wget or python3
#
# The downloader is probed, not assumed. curl comes first because it is the one fetcher present on
# both the GitHub macOS and Ubuntu runner images (wget is not installed on the macOS images), and
# only flags that macOS's system curl accepts are used. wget and python3 follow so a host missing
# curl still works. If none of the three is on PATH the script says so, rather than failing as a
# "command not found" from inside a pipeline.

set -euo pipefail

# Formula order, which is also the order the sidecars are reported in.
readonly TARGETS="aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl"

# KT-43 left the owner as a placeholder on purpose: this repository has no remote and no GitHub
# owner, so there is nothing to fetch from. Refusing is honest; guessing an owner would not be.
readonly OWNER_PLACEHOLDER="OWNER"

# A sidecar line is one sha256sum/shasum record: 64 lowercase hex, whitespace, an optional binary
# marker, then the filename. Uppercase hex and a short digest both fail here, before any rewrite.
readonly SIDECAR_RE='^([0-9a-f]{64})[[:space:]]+[*]?([^[:space:]]+)[[:space:]]*$'

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
root="$(CDPATH='' cd -- "$here/.." && pwd)"

die() { printf 'bump-formula: %s\n' "$*" >&2; exit 1; }
note() { printf ':: %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }

usage() {
    cat << 'USAGE'
bump-formula.sh <version>

  <version>  release to point the formula at, as vX.Y.Z[-prerelease] or X.Y.Z[-prerelease]

Fetches the four release .sha256 sidecars, validates them, and rewrites Formula/ktsense.rb's
RELEASE constant and its four sha256 values atomically. Leaves the formula untouched on any failure.

Environment:
  KTSENSE_FORMULA_PATH      formula to rewrite (default <repo>/Formula/ktsense.rb)
  KTSENSE_RELEASE_BASE_URL  release download base (default read from the formula's urls)
  KTSENSE_DOWNLOADER        auto (default), curl, wget or python3
USAGE
}

case "${1:-}" in
    -h | --help) usage; exit 0 ;;
esac

if [ "$#" -ne 1 ]; then
    usage >&2
    exit 2
fi

if ! version="$(bash "$here/release-version.sh" "$1")"; then
    die "refusing to bump the formula to an invalid release version"
fi

formula="${KTSENSE_FORMULA_PATH:-$root/Formula/ktsense.rb}"
[ -f "$formula" ] || die "no formula at $formula"
[ -w "$formula" ] || die "formula at $formula is not writable"

# The rewrite lands by renaming a sibling file over the formula, which would replace a symlink with a
# regular file and quietly detach whatever it pointed at. A tap that mirrors this formula by symlink
# should be told, not surprised.
if [ -L "$formula" ]; then
    die "$formula is a symlink; rewriting it would replace the link with a regular file. Point KTSENSE_FORMULA_PATH at the real file."
fi

formula_dir="$(CDPATH='' cd -- "$(dirname -- "$formula")" && pwd)"
[ -w "$formula_dir" ] || die "cannot stage the rewrite in $formula_dir: not writable"

derive_base_url() {
    local bases count
    bases="$(sed -n 's|^ *url "\(https://[^"]*\)/releases/download/[^"]*"$|\1|p' "$formula" | LC_ALL=C sort -u)"
    [ -n "$bases" ] || die "no release urls in $formula to derive a download base from"
    count="$(printf '%s\n' "$bases" | grep -c . || true)"
    [ "$count" -eq 1 ] || die "the formula's release urls disagree on a download base:
$bases"
    printf '%s/releases/download' "$bases"
}

base_url="${KTSENSE_RELEASE_BASE_URL:-}"
if [ -z "$base_url" ]; then
    base_url="$(derive_base_url)"
    case "$base_url" in
        *"/$OWNER_PLACEHOLDER/"*)
            die "$formula still points at the $OWNER_PLACEHOLDER placeholder ($base_url), so no release exists to fetch. Put the real owner in the formula first, or set KTSENSE_RELEASE_BASE_URL." ;;
    esac
fi
base_url="${base_url%/}"

resolve_downloader() {
    local want="${KTSENSE_DOWNLOADER:-auto}" candidate
    case "$want" in
        auto)
            for candidate in curl wget python3; do
                if command -v "$candidate" > /dev/null 2>&1; then
                    printf '%s' "$candidate"
                    return 0
                fi
            done
            die "no downloader on PATH: need curl, wget or python3" ;;
        curl | wget | python3)
            command -v "$want" > /dev/null 2>&1 || die "KTSENSE_DOWNLOADER=$want is not on PATH"
            printf '%s' "$want" ;;
        *) die "KTSENSE_DOWNLOADER must be auto, curl, wget or python3, got '$want'" ;;
    esac
}

downloader="$(resolve_downloader)"

fetch_to() {
    local url="$1" dest="$2"
    case "$downloader" in
        curl) curl -fsS --retry 2 --max-time 60 -o "$dest" "$url" ;;
        wget) wget -q --tries=2 --timeout=60 -O "$dest" "$url" ;;
        python3) python3 -c '
import sys, urllib.request
url, dest = sys.argv[1], sys.argv[2]
try:
    with urllib.request.urlopen(url, timeout=60) as response:
        body = response.read()
except Exception as error:
    sys.stderr.write("fetch failed: %s\n" % error)
    raise SystemExit(1)
with open(dest, "wb") as handle:
    handle.write(body)
' "$url" "$dest" ;;
    esac
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

note "bumping $formula to $version via $downloader from $base_url/v$version"

map="$work/hashes"
: > "$map"

for target in $TARGETS; do
    asset="ktsense-$version-$target.tar.gz"
    sidecar="$asset.sha256"
    url="$base_url/v$version/$sidecar"
    dest="$work/$sidecar"

    # A failed fetch leaves an empty or absent file behind on some downloaders, so it is removed
    # before the failure is reported: nothing downstream should see a half-written sidecar.
    if ! fetch_to "$url" "$dest"; then
        rm -f "$dest"
        die "could not fetch $url"
    fi
    [ -s "$dest" ] || die "fetched an empty $sidecar from $url"

    lines="$(grep -c '[^[:space:]]' "$dest" || true)"
    [ "$lines" -eq 1 ] || die "$sidecar must hold exactly one checksum line, found $lines"

    line="$(grep '[^[:space:]]' "$dest" | head -1)"
    [[ $line =~ $SIDECAR_RE ]] \
        || die "$sidecar is not 64 lowercase hex characters and a filename: '$line'"
    hash="${BASH_REMATCH[1]}"
    named="${BASH_REMATCH[2]}"
    [ "$named" = "$asset" ] || die "$sidecar names '$named', expected '$asset'"

    printf '%s %s\n' "$target" "$hash" >> "$map"
    ok "$sidecar $hash"
done

expected_count="$(printf '%s\n' $TARGETS | grep -c .)"
fetched="$(grep -c . "$map" || true)"
[ "$fetched" -eq "$expected_count" ] || die "expected $expected_count validated sidecars, have $fetched"

rewritten="$work/formula.rewritten"
sites="$work/sites"
: > "$sites"

# One pass over the formula. The RELEASE value and the four sha256 values are the only lines it
# rewrites, and each sha256 is taken from the url line directly above it, so a hash can never land
# in another target's block. Anything that does not look like exactly four target blocks is fatal
# here, before the real formula is touched.
awk -v version="$version" -v sites="$sites" -v targets="$TARGETS" '
function fail(msg) {
    printf("bump-formula: %s\n", msg) > "/dev/stderr"
    failed = 1
    exit 1
}
FNR == NR { hash[$1] = $2; next }
{
    line = $0
    if (line ~ /^[[:space:]]*RELEASE[[:space:]]*=[[:space:]]*"/) {
        if (++release_count > 1) fail("formula has more than one RELEASE constant")
        sub(/"[^"]*"/, "\"" version "\"", line)
        printf("release %d\n", FNR) >> sites
    } else if (line ~ /^[[:space:]]*url[[:space:]]+"/) {
        hit = ""
        n = split(targets, target, " ")
        for (i = 1; i <= n; i++) {
            if (index(line, "-" target[i] ".tar.gz\"") > 0) { hit = target[i]; break }
        }
        if (hit == "") fail("url on line " FNR " names none of the four release targets: " line)
        if (seen[hit]++) fail("formula has more than one url block for " hit)
        if (pending != "") fail("url block for " pending " has no sha256 line")
        pending = hit
    } else if (line ~ /^[[:space:]]*sha256[[:space:]]+"/) {
        if (pending == "") fail("sha256 on line " FNR " is not inside one of the four target blocks")
        sub(/"[^"]*"/, "\"" hash[pending] "\"", line)
        printf("sha256 %s %d\n", pending, FNR) >> sites
        sha_count++
        pending = ""
    }
    print line
}
END {
    if (failed) exit 1
    if (release_count != 1) fail("formula has no RELEASE constant to bump")
    if (pending != "") fail("url block for " pending " has no sha256 line")
    n = split(targets, target, " ")
    for (i = 1; i <= n; i++) if (!seen[target[i]]) fail("formula has no url block for " target[i])
    if (sha_count != n) fail("expected " n " sha256 values to rewrite, rewrote " sha_count)
}
' "$map" "$formula" > "$rewritten"

before_lines="$(awk 'END { print NR }' "$formula")"
after_lines="$(awk 'END { print NR }' "$rewritten")"
[ "$before_lines" -eq "$after_lines" ] \
    || die "the rewrite changed the formula's line count ($before_lines -> $after_lines)"

awk 'NR == FNR { was[FNR] = $0; next } was[FNR] != $0 { print FNR }' "$formula" "$rewritten" > "$work/changed"
awk '$1 == "release" { print $2 } $1 == "sha256" { print $3 }' "$sites" > "$work/sites.lines"
unexpected="$(awk 'NR == FNR { site[$1] = 1; next } !($1 in site) { print }' "$work/sites.lines" "$work/changed")"
[ -z "$unexpected" ] \
    || die "the rewrite changed lines outside the RELEASE and sha256 sites: $(printf '%s' "$unexpected" | tr '\n' ' ')"

# Post-conditions on the rewritten text itself, so the outcome is asserted rather than inferred from
# the edit having been attempted. These hold on a repeat run too, where nothing changed.
got_release="$(sed -n 's/^[[:space:]]*RELEASE[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$rewritten")"
[ "$got_release" = "$version" ] || die "rewritten formula reports RELEASE '$got_release', expected '$version'"

while read -r target hash; do
    block_hash="$(awk -v needle="-$target.tar.gz\"" '
        index($0, needle) > 0 { want = 1; next }
        want && /^[[:space:]]*sha256[[:space:]]+"/ {
            if (match($0, /"[0-9a-f]*"/)) print substr($0, RSTART + 1, RLENGTH - 2)
            want = 0
        }' "$rewritten")"
    [ "$block_hash" = "$hash" ] \
        || die "rewritten $target block carries '$block_hash', expected '$hash'"
done < "$map"

distinct="$(awk '{ print $2 }' "$map" | LC_ALL=C sort -u | grep -c . || true)"

# Same-directory staging, so the replacement is a rename within one filesystem and a reader never
# sees a partial formula. cp -p creates the staged file with the formula's own mode, then the content
# is written into it by truncation, which keeps that mode.
staged="$formula_dir/.ktsense-formula.$$"
trap 'rm -rf "$work"; rm -f "$staged"' EXIT
rm -f "$staged"
cp -p "$formula" "$staged"
cat "$rewritten" > "$staged"
mv -f "$staged" "$formula"
trap 'rm -rf "$work"' EXIT

changed_count="$(grep -c . "$work/changed" || true)"
ok "$formula now at $version with $distinct distinct target hashes"
note "$changed_count line(s) changed: $(tr '\n' ' ' < "$work/changed")"
if [ "$changed_count" -eq 0 ]; then
    note "no diff: the formula already carried this version and these checksums"
fi
