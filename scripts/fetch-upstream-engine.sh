#!/usr/bin/env bash
#
# Acquire the pinned kmp-lsp engine for one target and install it into a destination directory.
#
#   scripts/fetch-upstream-engine.sh --target <rust-target> --dest <dir>
#                                    [--lock <file>] [--source auto|asset|cargo]
#
# Primary route is the pinned release tarball from upstream.lock, verified by sha256. Fallback route
# is `cargo install` at the pinned version. --source asset or --source cargo pins one route so a
# failure is reported rather than papered over by the other.
#
# Exits non-zero on a checksum mismatch, a target with no record in the lock, a missing tool, or a
# route that produces no engine. There is no warn-and-continue path.

set -euo pipefail

die() { printf 'fetch-upstream-engine: %s\n' "$*" >&2; exit 1; }
note() { printf ':: %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }

usage() {
    cat << 'USAGE'
fetch-upstream-engine.sh --target <rust-target> --dest <dir>
                         [--lock <file>] [--source auto|asset|cargo]

  --target  Rust target triple, must have an asset record in the lock file
  --dest    directory to install kmp-lsp and, when available, kmp-jar-indexer into
  --lock    path to upstream.lock, defaults to the repository root
  --source  auto (default) tries the release asset then cargo install; asset or
            cargo pins a single route so its failure is reported, not papered over
USAGE
}

target=""
dest=""
lock=""
source_mode="auto"

while [ $# -gt 0 ]; do
    case "$1" in
        --target) [ -n "${2:-}" ] || die "--target needs a value"; target="$2"; shift 2 ;;
        --dest) [ -n "${2:-}" ] || die "--dest needs a value"; dest="$2"; shift 2 ;;
        --lock) [ -n "${2:-}" ] || die "--lock needs a value"; lock="$2"; shift 2 ;;
        --source) [ -n "${2:-}" ] || die "--source needs a value"; source_mode="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$target" ] || { usage >&2; die "--target is required"; }
# --dest has no default on purpose: /dist and /libexec are not in .gitignore, so a default inside the
# repository root would make committing a 19 MB engine easy.
[ -n "$dest" ] || { usage >&2; die "--dest is required"; }

case "$source_mode" in
    auto | asset | cargo) ;;
    *) die "--source must be auto, asset or cargo, got '$source_mode'" ;;
esac

if [ -z "$lock" ]; then
    lock="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)/upstream.lock"
fi
[ -f "$lock" ] || die "no lock file at $lock"

lock_field() { awk -v key="$1" '$1 == key { print $2; exit }' "$lock"; }

version="$(lock_field version)"
repo="$(lock_field repo)"
tag="$(lock_field tag)"
[ -n "$version" ] && [ -n "$repo" ] && [ -n "$tag" ] || die "$lock is missing version, repo or tag"

read -r asset expected_sha expected_bytes << EOF || true
$(awk -v t="$target" '$1 == "asset" && $2 == t { print $3, $4, $5; exit }' "$lock")
EOF
if [ "$source_mode" != "cargo" ] && [ -z "${asset:-}" ]; then
    die "$lock has no asset record for target $target"
fi

sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        die "no sha256 tool found, need sha256sum or shasum"
    fi
}

download() {
    local url="$1" out="$2"
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL --retry 3 --retry-connrefused -o "$out" "$url"
    elif command -v wget > /dev/null 2>&1; then
        wget -q -O "$out" "$url"
    else
        die "no downloader found, need curl or wget"
    fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

install_engine() {
    local from="$1"
    mkdir -p "$dest"
    install -m 0755 "$from/kmp-lsp" "$dest/kmp-lsp"
    ok "$dest/kmp-lsp"
    # kmp-jar-indexer ships in the release tarball but is not a crates.io crate, so the cargo route can
    # never supply it. Its absence costs library-symbol indexing from Gradle jars and is not an error.
    if [ -f "$from/kmp-jar-indexer" ]; then
        install -m 0755 "$from/kmp-jar-indexer" "$dest/kmp-jar-indexer"
        ok "$dest/kmp-jar-indexer"
    else
        note "no kmp-jar-indexer in this route, library-symbol indexing from Gradle jars will be absent"
    fi
}

acquire_from_asset() {
    local url="https://github.com/$repo/releases/download/$tag/$asset"
    note "downloading $asset from $repo $tag"
    download "$url" "$work/$asset" || return 1

    local got_bytes got_sha
    got_bytes="$(wc -c < "$work/$asset" | tr -d ' ')"
    got_sha="$(sha256_of "$work/$asset")"
    note "bytes $got_bytes, sha256 $got_sha"

    [ "$got_bytes" = "$expected_bytes" ] \
        || die "size mismatch for $asset: lock says $expected_bytes, got $got_bytes"
    [ "$got_sha" = "$expected_sha" ] \
        || die "sha256 mismatch for $asset
  expected $expected_sha
  got      $got_sha
Refusing to install an engine the lock file does not vouch for."
    ok "sha256 matches $lock"

    mkdir -p "$work/unpacked"
    tar -xzf "$work/$asset" -C "$work/unpacked"
    [ -f "$work/unpacked/kmp-lsp" ] || die "$asset contains no kmp-lsp at its root"
    install_engine "$work/unpacked"
}

acquire_from_cargo() {
    command -v cargo > /dev/null 2>&1 || die "cargo not found, cannot use the fallback route"
    note "building kmp-lsp $version from crates.io for $target"

    local zig_triple=""
    case "$target" in
        x86_64-unknown-linux-musl) zig_triple="x86_64-linux-musl" ;;
        aarch64-unknown-linux-musl) zig_triple="aarch64-linux-musl" ;;
    esac

    # kmp-lsp vendors C sources, so a musl target needs a musl-capable C compiler and linker that
    # rustup's self-contained musl libc does not provide. zig cc via cargo-zigbuild is the one the
    # release workflow already installs for its own musl jobs.
    if [ -n "$zig_triple" ]; then
        command -v cargo-zigbuild > /dev/null 2>&1 \
            || die "target $target needs cargo-zigbuild to supply a musl C compiler"
        local shim="$work/zigcc-$zig_triple"
        printf '#!/usr/bin/env bash\nexec cargo-zigbuild zig cc -- -target %s "$@"\n' "$zig_triple" > "$shim"
        chmod +x "$shim"
        local cc_triple env_triple
        cc_triple="${target//-/_}"
        env_triple="$(printf '%s' "$cc_triple" | tr '[:lower:]' '[:upper:]')"
        export "CC_$cc_triple=$shim"
        export "CARGO_TARGET_${env_triple}_LINKER=$shim"
    fi

    cargo install kmp-lsp --version "$version" --locked --target "$target" --root "$work/cargo"
    [ -f "$work/cargo/bin/kmp-lsp" ] || die "cargo install produced no kmp-lsp"
    install_engine "$work/cargo/bin"
}

case "$source_mode" in
    asset) acquire_from_asset; route="asset" ;;
    cargo) acquire_from_cargo; route="cargo" ;;
    auto)
        if acquire_from_asset; then
            route="asset"
        else
            note "release asset unavailable, falling back to cargo install"
            acquire_from_cargo
            route="cargo"
        fi
        ;;
esac

if command -v file > /dev/null 2>&1; then
    file "$dest/kmp-lsp"
fi
note "engine for $target installed from the $route route at pin $version"
