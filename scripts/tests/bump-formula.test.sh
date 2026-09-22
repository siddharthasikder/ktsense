#!/usr/bin/env bash
#
# Tests scripts/bump-formula.sh end to end without a release, a remote, a tap or Homebrew.
#
# A bounded local HTTP server stands in for https://github.com/<owner>/ktsense/releases/download: it
# serves v<version>/ktsense-<version>-<target>.tar.gz.sha256 for four deterministic fake tarballs,
# and the script is pointed at it with KTSENSE_RELEASE_BASE_URL. The formula is never touched in
# place: every case copies Formula/ktsense.rb into a temporary tree and passes that copy through
# KTSENSE_FORMULA_PATH, so the committed formula stays byte-identical whatever happens here.
#
# One server serves every case. Each failure case gets its own version tag directory, so a malformed
# hash, a sidecar naming another target's tarball and an absent sidecar can all be served at once
# without restarting anything.
#
# Everything the run observes is composed into one transcript and compared against one expected
# transcript, so a single mismatch prints the whole picture rather than the first tripped assertion.
# The expected changed-line numbers and the expected hashes are derived here, from the formula and
# from the fixture content, never hardcoded: a formula edit moves both sides together, and a wrong
# hash cannot agree with itself.
#
# Written for bash 3.2, the version macOS ships, so no associative arrays.

set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
root="$(CDPATH='' cd -- "$here/../.." && pwd)"
script="$root/scripts/bump-formula.sh"
formula_src="$root/Formula/ktsense.rb"

readonly TARGETS="aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl"
readonly GOOD_VERSION="0.1.0-rc.1"
readonly UPPERCASE_VERSION="0.2.0-rc.1"
readonly SHORT_VERSION="0.3.0-rc.1"
readonly WRONGNAME_VERSION="0.4.0-rc.1"
readonly MISSING_VERSION="0.5.0-rc.1"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

[ -f "$script" ] || fail "no script at $script"
[ -f "$formula_src" ] || fail "no formula at $formula_src"
command -v python3 > /dev/null 2>&1 || fail "python3 is needed to serve the local release fixture"

work="$(mktemp -d)"
server_pid=""
cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" 2> /dev/null || true
        wait "$server_pid" 2> /dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        fail "no sha256 tool found, need sha256sum or shasum"
    fi
}

serve_root="$work/serve"
source_before="$(sha256_of "$formula_src")"

# Deterministic stand-in content, distinct per target and per release, so all four hashes differ and
# a hash that lands in the wrong block cannot pass.
fake_tarball() {
    local version="$1" target="$2" dir="$3"
    printf 'fake ktsense release %s for %s\n' "$version" "$target" > "$dir/ktsense-$version-$target.tar.gz"
}

# Writes the four sidecars for one release tag. The mutation argument names the single defect to
# plant, applied to x86_64-apple-darwin so the first target still validates and the failure has to be
# caught mid-flight, after one good sidecar was already fetched.
stage_release() {
    local version="$1" mutation="${2:-none}" dir target asset hash
    dir="$serve_root/v$version"
    mkdir -p "$dir"
    for target in $TARGETS; do
        fake_tarball "$version" "$target" "$dir"
        asset="ktsense-$version-$target.tar.gz"
        hash="$(sha256_of "$dir/$asset")"
        if [ "$target" = "x86_64-apple-darwin" ]; then
            case "$mutation" in
                uppercase)
                    printf '%s  %s\n' "$(printf '%s' "$hash" | tr 'a-f' 'A-F')" "$asset" > "$dir/$asset.sha256"
                    continue ;;
                short)
                    printf '%s  %s\n' "${hash%?}" "$asset" > "$dir/$asset.sha256"
                    continue ;;
                wrongname)
                    printf '%s  %s\n' "$hash" "ktsense-$version-aarch64-apple-darwin.tar.gz" > "$dir/$asset.sha256"
                    continue ;;
                missing)
                    continue ;;
            esac
        fi
        printf '%s  %s\n' "$hash" "$asset" > "$dir/$asset.sha256"
    done
}

stage_release "$GOOD_VERSION"
stage_release "$UPPERCASE_VERSION" uppercase
stage_release "$SHORT_VERSION" short
stage_release "$WRONGNAME_VERSION" wrongname
stage_release "$MISSING_VERSION" missing

# Bound the fixture: bind port 0 so nothing collides, publish the port, answer only on loopback, and
# die with the test through the EXIT trap.
cat > "$work/serve.py" << 'PY'
import http.server
import os
import socketserver
import sys

root, port_file = sys.argv[1], sys.argv[2]
os.chdir(root)


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", 0), QuietHandler) as httpd:
    with open(port_file, "w") as handle:
        handle.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY

python3 "$work/serve.py" "$serve_root" "$work/port" &
server_pid=$!

port=""
waited=0
while [ "$waited" -lt 100 ]; do
    if [ -s "$work/port" ]; then
        port="$(cat "$work/port")"
        break
    fi
    kill -0 "$server_pid" 2> /dev/null || fail "the local release fixture exited before it was ready"
    sleep 0.1
    waited=$((waited + 1))
done
[ -n "$port" ] || fail "the local release fixture did not publish a port within 10s"
base_url="http://127.0.0.1:$port"

expected_hash_of() {
    local version="$1" target="$2"
    sha256_of "$serve_root/v$version/ktsense-$version-$target.tar.gz"
}

# The sha256 that sits under a target's url block, found independently of how the script finds it.
hash_in_block() {
    local file="$1" target="$2"
    awk -v needle="-$target.tar.gz\"" '
        index($0, needle) > 0 { inblock = 1; next }
        inblock && /^[[:space:]]*sha256[[:space:]]+"/ {
            if (match($0, /"[^"]*"/)) print substr($0, RSTART + 1, RLENGTH - 2)
            inblock = 0
        }' "$file"
}

release_in() {
    sed -n 's/^[[:space:]]*RELEASE[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$1"
}

changed_lines() {
    awk 'NR == FNR { was[FNR] = $0; next } was[FNR] != $0 { print FNR }' "$1" "$2" | tr '\n' ' ' | sed 's/ $//'
}

count_lines() {
    printf '%s\n' "$1" | grep -c '[^[:space:]]' || true
}

# A fresh copy of the formula for one case, so no case can observe another's rewrite.
copy_formula() {
    local dest="$1" source="${2:-$formula_src}"
    mkdir -p "$(dirname "$dest")"
    cp "$source" "$dest"
}

run_bump() {
    local copy="$1" version="$2" base="$3" log="$4"
    KTSENSE_FORMULA_PATH="$copy" KTSENSE_RELEASE_BASE_URL="$base" \
        bash "$script" "$version" > "$log" 2>&1
}

expected=""
actual=""

record() {
    expected+="$1"$'\n'
    actual+="$2"$'\n'
}

# A case that must fail and must leave its formula byte-identical. The pre-run hash is taken from the
# copy itself, so a mutated-formula case is compared against its own input rather than the original.
expect_refusal() {
    local label="$1" copy="$2" version="$3" base="$4" before after rc refused unchanged
    before="$(sha256_of "$copy")"
    rc=0
    run_bump "$copy" "$version" "$base" "$work/$label.log" || rc=$?
    after="$(sha256_of "$copy")"
    refused="no"
    if [ "$rc" -ne 0 ]; then refused="yes"; fi
    unchanged="no"
    if [ "$before" = "$after" ]; then unchanged="yes"; fi
    record "$label: refused=yes unchanged=yes" "$label: refused=$refused unchanged=$unchanged"
}

# Refuses the placeholder owner instead of reaching the network, with no base-url override at all.
placeholder_copy="$work/placeholder/ktsense.rb"
copy_formula "$placeholder_copy"
placeholder_before="$(sha256_of "$placeholder_copy")"
placeholder_rc=0
KTSENSE_FORMULA_PATH="$placeholder_copy" bash "$script" "$GOOD_VERSION" > "$work/placeholder.log" 2>&1 \
    || placeholder_rc=$?
placeholder_after="$(sha256_of "$placeholder_copy")"
placeholder_refused="no"
if [ "$placeholder_rc" -ne 0 ]; then placeholder_refused="yes"; fi
placeholder_unchanged="no"
if [ "$placeholder_before" = "$placeholder_after" ]; then placeholder_unchanged="yes"; fi
placeholder_named="no"
if grep -q 'OWNER' "$work/placeholder.log"; then placeholder_named="yes"; fi
record "placeholder-owner: refused=yes unchanged=yes says-owner=yes" \
    "placeholder-owner: refused=$placeholder_refused unchanged=$placeholder_unchanged says-owner=$placeholder_named"

# The version contract is delegated to release-version.sh (KT-59), so its rejects must reject here.
copy_formula "$work/badversion-empty/ktsense.rb"
expect_refusal "version-empty" "$work/badversion-empty/ktsense.rb" "" "$base_url"
copy_formula "$work/badversion-build/ktsense.rb"
expect_refusal "version-build-metadata" "$work/badversion-build/ktsense.rb" "1.2.3-rc.1+build" "$base_url"

# One bad sidecar in a set of four, three ways, each after a good sidecar has already been fetched.
copy_formula "$work/uppercase/ktsense.rb"
expect_refusal "hash-uppercase" "$work/uppercase/ktsense.rb" "$UPPERCASE_VERSION" "$base_url"
copy_formula "$work/short/ktsense.rb"
expect_refusal "hash-too-short" "$work/short/ktsense.rb" "$SHORT_VERSION" "$base_url"
copy_formula "$work/wrongname/ktsense.rb"
expect_refusal "sidecar-names-other-target" "$work/wrongname/ktsense.rb" "$WRONGNAME_VERSION" "$base_url"
copy_formula "$work/missing/ktsense.rb"
expect_refusal "sidecar-missing" "$work/missing/ktsense.rb" "$MISSING_VERSION" "$base_url"

# A formula whose target blocks are not the expected four is a refusal, not a partial rewrite.
duplicate_src="$work/duplicate-src.rb"
sed 's|-x86_64-apple-darwin\.tar\.gz|-aarch64-apple-darwin.tar.gz|' "$formula_src" > "$duplicate_src"
copy_formula "$work/duplicate/ktsense.rb" "$duplicate_src"
expect_refusal "formula-duplicate-target" "$work/duplicate/ktsense.rb" "$GOOD_VERSION" "$base_url"

unknown_src="$work/unknown-src.rb"
sed 's|-x86_64-apple-darwin\.tar\.gz|-x86_64-apple-ios.tar.gz|' "$formula_src" > "$unknown_src"
copy_formula "$work/unknown/ktsense.rb" "$unknown_src"
expect_refusal "formula-unknown-target" "$work/unknown/ktsense.rb" "$GOOD_VERSION" "$base_url"

# The acceptance run: one version, four hashes, nothing else, then the same run again.
bumped="$work/bump/ktsense.rb"
copy_formula "$bumped"
bump_rc=0
run_bump "$bumped" "$GOOD_VERSION" "$base_url" "$work/bump.log" || bump_rc=$?
record "bump: rc=0" "bump: rc=$bump_rc"

release_line="$(grep -n '^[[:space:]]*RELEASE[[:space:]]*=' "$formula_src" | cut -d: -f1 | tr '\n' ' ' | sed 's/ $//')"
sha_lines="$(grep -n '^[[:space:]]*sha256[[:space:]]*"' "$formula_src" | cut -d: -f1 | tr '\n' ' ' | sed 's/ $//')"
expected_changed="$(printf '%s %s\n' "$release_line" "$sha_lines")"
got_changed="$(changed_lines "$formula_src" "$bumped")"
record "changed lines: $expected_changed" "changed lines: $got_changed"

classify_changed() {
    local kind="$1"
    awk -v release="$release_line" -v shas="$sha_lines" -v kind="$kind" '
        BEGIN {
            split(shas, s, " ")
            for (i in s) issha[s[i]] = 1
            n = 0
        }
        {
            for (i = 1; i <= NF; i++) {
                if (kind == "release" && $i == release) n++
                else if (kind == "sha" && ($i in issha)) n++
                else if (kind == "other" && $i != release && !($i in issha)) n++
            }
        }
        END { print n }' <<< "$got_changed"
}
record "version lines changed: 1" "version lines changed: $(classify_changed release)"
record "hash lines changed: 4" "hash lines changed: $(classify_changed sha)"
record "other lines changed: 0" "other lines changed: $(classify_changed other)"

record "release value: $GOOD_VERSION" "release value: $(release_in "$bumped")"

for target in $TARGETS; do
    record "$target sha256: $(expected_hash_of "$GOOD_VERSION" "$target")" \
        "$target sha256: $(hash_in_block "$bumped" "$target")"
done

distinct_expected=4
distinct_got="$(for target in $TARGETS; do hash_in_block "$bumped" "$target"; done | LC_ALL=C sort -u | grep -c . || true)"
record "distinct hashes: $distinct_expected" "distinct hashes: $distinct_got"

# Idempotence: same version, same checksums, no diff at all.
rerun_before="$(sha256_of "$bumped")"
rerun_rc=0
run_bump "$bumped" "$GOOD_VERSION" "$base_url" "$work/rerun.log" || rerun_rc=$?
rerun_after="$(sha256_of "$bumped")"
rerun_identical="no"
if [ "$rerun_before" = "$rerun_after" ]; then rerun_identical="yes"; fi
rerun_reported="no"
if grep -q 'no diff' "$work/rerun.log"; then rerun_reported="yes"; fi
record "rerun: rc=0 identical=yes reports-no-diff=yes" \
    "rerun: rc=$rerun_rc identical=$rerun_identical reports-no-diff=$rerun_reported"

# The repository's own formula is the thing this must never touch, whatever git thinks of it.
source_after="$(sha256_of "$formula_src")"
source_untouched="no"
if [ "$source_before" = "$source_after" ]; then source_untouched="yes"; fi
record "repository formula untouched: yes" "repository formula untouched: $source_untouched"

if [ "$actual" != "$expected" ]; then
    printf 'expected:\n%s\ngot:\n%s\n' "$expected" "$actual" >&2
    printf -- '--- bump log ---\n' >&2
    cat "$work/bump.log" >&2 || true
    fail "bump-formula transcript mismatch"
fi

printf '%s' "$actual"
printf 'all bump-formula tests passed (%s cases)\n' "$(count_lines "$actual")"
