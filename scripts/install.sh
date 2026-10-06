#!/usr/bin/env bash
#
# One entry point that takes a machine from nothing to a working `ktsense`, and proves it works.
#
#   scripts/install.sh [--bootstrap-brew] [--trust-tap] [--yes] [--skip-ripgrep] [--help]
#
# The steps are: find or install Homebrew, put it on this shell's PATH, install ripgrep, tap
# siddharthasikder/ktsense, grant tap trust, install the formula, then prove the result by running the
# binary rather than by trusting exit codes.
#
# Three of those steps exist for reasons that are not obvious, so they are recorded here.
#
# Trust is never granted silently. Homebrew 7 refuses to load a formula from a non-official tap until
# the tap or the formula is trusted, and that refusal is a safety decision about running third-party
# code. A script that quietly answered it for you would defeat it, so this one requires either
# `--trust-tap` or an interactive confirmation, and prints what is being trusted first. The README
# says never to automate the decision, and this script is bound by that too.
#
# ripgrep is installed even though `Formula/ktsense.rb` does not declare it. The engine execs `rg` for
# reference and declaration search, and without it those searches return nothing rather than failing,
# which is indistinguishable from "no such symbol". A missing `rg` therefore produces wrong answers
# instead of errors, which is worth one extra formula at install time.
#
# A Linux host below glibc 2.28 is warned, not refused. `ktsense` itself is musl-static and its
# tree-sitter commands work anywhere, so a partial install is genuinely useful; it is the bundled
# glibc-linked engine that will not start. The warning names which commands still answer so the
# outcome is never a surprise.

set -euo pipefail

TAP="siddharthasikder/ktsense"
FORMULA="ktsense"
MIN_GLIBC="2.28"

bootstrap_brew=0
trust_tap=0
assume_yes=0
skip_ripgrep=0
probe_dir=""

# A single EXIT trap owns the probe directory. A RETURN trap referencing a function-local path fires
# after that local is out of scope, which under `set -u` aborts the script after the work succeeded.
cleanup() {
    [[ -n ${probe_dir:-} ]] && rm -rf "$probe_dir"
    return 0
}
trap cleanup EXIT

usage() {
    cat <<'EOS'
Usage: scripts/install.sh [options]

Options:
  --bootstrap-brew   Install Homebrew if it is not present. Without this, a missing Homebrew is
                     reported with instructions rather than installed.
  --trust-tap        Grant trust to the tap without prompting. Use only when you have read
                     Formula/ktsense.rb and the tap repository and accept running them.
  --yes              Answer yes to the confirmations this script would otherwise ask.
                     Implies --bootstrap-brew and --trust-tap.
  --skip-ripgrep     Do not install ripgrep. Reference search will silently return nothing.
  -h, --help         Print this message.

Exit codes:
  0 installed and proven, 1 usage error, 2 a required step was declined or unavailable,
  3 installed but the proof failed.
EOS
}

log()  { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '    warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit "${2:-1}"; }

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --bootstrap-brew) bootstrap_brew=1 ;;
            --trust-tap)      trust_tap=1 ;;
            --skip-ripgrep)   skip_ripgrep=1 ;;
            --yes|-y)         assume_yes=1; bootstrap_brew=1; trust_tap=1 ;;
            -h|--help)        usage; return 10 ;;
            *)                printf 'error: unknown option: %s\n' "$1" >&2; usage >&2; return 1 ;;
        esac
        shift
    done
    return 0
}

# Numeric dotted-version comparison: is $1 at least $2. Used for the glibc floor, where a string
# compare would rank 2.9 above 2.28. Each component is reduced to its leading digits, so a vendor
# suffix such as 2.28-stable compares as 28 rather than collapsing to 0.
version_ge() {
    local have="$1" want="$2" h w
    local -a hp wp
    IFS='.' read -r -a hp <<<"$have"
    IFS='.' read -r -a wp <<<"$want"
    local i
    for ((i = 0; i < ${#wp[@]} || i < ${#hp[@]}; i++)); do
        h="${hp[i]:-0}"; h="${h%%[^0-9]*}"; [[ -n $h ]] || h=0
        w="${wp[i]:-0}"; w="${w%%[^0-9]*}"; [[ -n $w ]] || w=0
        ((10#$h > 10#$w)) && return 0
        ((10#$h < 10#$w)) && return 1
    done
    return 0
}

detect_glibc() {
    local out
    if command -v getconf >/dev/null 2>&1 && out=$(getconf GNU_LIBC_VERSION 2>/dev/null); then
        printf '%s\n' "${out##* }"
        return 0
    fi
    if command -v ldd >/dev/null 2>&1 && out=$(ldd --version 2>/dev/null | head -1); then
        printf '%s\n' "${out##* }"
        return 0
    fi
    return 1
}

confirm() {
    local prompt="$1"
    ((assume_yes)) && return 0
    if [[ ! -t 0 ]]; then
        warn "not an interactive terminal, so cannot ask: $prompt"
        return 1
    fi
    local reply
    read -r -p "    $prompt [y/N] " reply
    [[ $reply == [yY] || $reply == [yY][eE][sS] ]]
}

report_platform() {
    log "Platform"
    info "uname: $(uname -s) $(uname -m)"
    [[ $(uname -s) == Linux ]] || { info "macOS: the bundled engine has no glibc floor here."; return 0; }

    local glibc
    if ! glibc=$(detect_glibc); then
        warn "could not determine the glibc version; if it is below $MIN_GLIBC the bundled engine will not start."
        return 0
    fi
    info "glibc: $glibc"
    if version_ge "$glibc" "$MIN_GLIBC"; then
        info "at or above the $MIN_GLIBC floor, so the bundled engine should start."
        return 0
    fi
    warn "glibc $glibc is below $MIN_GLIBC, so the bundled kmp-lsp engine will not start on this host."
    info "ktsense itself is musl-static and still works: outline, deps, map and status will answer."
    info "symbols, trace, check, diagnose, context and the daemon will fail until you build an engine"
    info "for this host and point KTSENSE_LSP_PATH at it."
}

ensure_brew() {
    log "Homebrew"
    if command -v brew >/dev/null 2>&1; then
        info "found: $(command -v brew)"
    else
        local prefix found=""
        for prefix in /home/linuxbrew/.linuxbrew /opt/homebrew /usr/local "$HOME/.linuxbrew"; do
            [[ -x "$prefix/bin/brew" ]] && { found="$prefix/bin/brew"; break; }
        done
        if [[ -n $found ]]; then
            info "found an installed Homebrew not on PATH: $found"
            eval "$("$found" shellenv)"
        else
            if ((bootstrap_brew)) || confirm "Homebrew is not installed. Install it now?"; then
                install_brew
            else
                die "Homebrew is required and was not installed. Re-run with --bootstrap-brew, or install it from https://brew.sh and re-run." 2
            fi
        fi
    fi

    eval "$(brew shellenv)"
    export HOMEBREW_NO_AUTO_UPDATE=1
    export HOMEBREW_NO_ENV_HINTS=1
    info "prefix: $(brew --prefix)"
    info "version: $(brew --version | head -1)"
}

install_brew() {
    command -v curl >/dev/null 2>&1 || die "curl is required to install Homebrew and was not found." 2
    command -v git  >/dev/null 2>&1 || die "git is required to install Homebrew and was not found." 2
    info "installing Homebrew from the official installer"
    NONINTERACTIVE=1 /bin/bash -c \
        "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" \
        || die "the Homebrew installer failed. Install it manually from https://brew.sh and re-run." 2

    local prefix
    for prefix in /home/linuxbrew/.linuxbrew /opt/homebrew /usr/local "$HOME/.linuxbrew"; do
        [[ -x "$prefix/bin/brew" ]] && { eval "$("$prefix/bin/brew" shellenv)"; break; }
    done
    command -v brew >/dev/null 2>&1 \
        || die "Homebrew installed but 'brew' is still not on PATH. Add its shellenv to your profile and re-run." 2
}

ensure_ripgrep() {
    log "ripgrep"
    if ((skip_ripgrep)); then
        warn "skipped at your request. Reference and declaration search will return nothing rather than failing."
        return 0
    fi
    if command -v rg >/dev/null 2>&1; then
        info "found: $(command -v rg)"
        return 0
    fi
    info "not found, installing it, because the engine execs rg and answers nothing without it"
    brew install ripgrep >/dev/null 2>&1 \
        || die "could not install ripgrep. Install it yourself and re-run, or pass --skip-ripgrep to accept degraded search." 2
    info "installed: $(command -v rg || echo 'not on PATH')"
}

ensure_tap() {
    log "Tap $TAP"
    if brew tap 2>/dev/null | grep -qx "$TAP"; then
        info "already tapped"
        return 0
    fi
    brew tap "$TAP" >/dev/null 2>&1 || die "could not tap $TAP. Check network access and that the repository is public." 2
    info "tapped"
}

ensure_trust() {
    log "Trust"
    if brew trust --tap 2>/dev/null | grep -q "$TAP"; then
        info "already trusted"
        return 0
    fi

    info "Homebrew refuses to load a formula from a non-official tap until you trust it."
    info "Granting trust means you accept running what $TAP serves on this machine."
    info "Read these first: $(brew --repository "$TAP")/Formula/ktsense.rb"
    info "and https://github.com/$TAP"

    if ((trust_tap)) || confirm "Trust the tap $TAP now?"; then
        brew trust "$TAP" || die "could not record trust for $TAP." 2
    else
        die "trust was not granted, so the formula cannot be loaded. Re-run with --trust-tap once you have read the formula, or install a prerelease tarball instead." 2
    fi
}

install_formula() {
    log "Install $FORMULA"
    if brew list --formula 2>/dev/null | grep -qx "$FORMULA"; then
        info "already installed, reinstalling is not attempted; run 'brew upgrade $FORMULA' to move versions"
    else
        brew install "$FORMULA" || die "brew install $FORMULA failed." 2
    fi
    info "prefix: $(brew --prefix "$FORMULA")"
}

prove_install() {
    log "Proof"
    local bin
    bin="$(brew --prefix)/bin/ktsense"
    [[ -x $bin ]] || die "expected an executable at $bin and did not find one." 3

    local version
    version="$("$bin" --version 2>&1)" || die "'$bin --version' failed: $version" 3
    info "version: $version"

    probe_dir="$(mktemp -d)"
    local file="$probe_dir/Probe.kt"
    cat >"$file" <<'KOTLIN'
package probe

class Greeter(val name: String) {
    fun greet(): String = "hi, $name"
}
KOTLIN

    local outline
    outline="$("$bin" outline "$file" 2>&1)" || die "'ktsense outline' failed on a generated file: $outline" 3
    grep -q 'fun greet' <<<"$outline" || die "'ktsense outline' ran but did not report the declaration it was given." 3
    info "outline: reported 'fun greet' from a generated file"

    # `status` takes --root, not a positional path, and it is documented to answer whether or not an
    # engine is usable, so a non-zero exit here is a real failure rather than an absent engine.
    local status
    status="$("$bin" status --root "$probe_dir" 2>&1)" \
        || die "'ktsense status --root' exited non-zero, which it should not do even with no usable engine: $status" 3

    local engine_line
    engine_line="$(grep -m1 '^engine:' <<<"$status" || true)"
    info "${engine_line:-engine: not reported}"
    if grep -q 'unavailable' <<<"${engine_line:-}"; then
        warn "the engine is unavailable on this host, so engine-backed commands will fail."
        info "tree-sitter commands (outline, deps, map) and status work regardless."
    fi
}

summary() {
    log "Done"
    info "binary: $(brew --prefix)/bin/ktsense"
    info "add Homebrew to future shells with: eval \"\$($(command -v brew) shellenv)\""
    info "MCP server for an agent: ktsense --root <repo> mcp"
    info "remove everything, trust record included:"
    info "  brew uninstall $FORMULA && brew untrust --tap $TAP && brew untap $TAP"
}

main() {
    local rc=0
    parse_args "$@" || rc=$?
    ((rc == 10)) && return 0
    ((rc != 0)) && return "$rc"

    report_platform
    ensure_brew
    ensure_ripgrep
    ensure_tap
    ensure_trust
    install_formula
    prove_install
    summary
}

if [[ ${KTSENSE_INSTALL_SOURCE_ONLY:-0} != 1 ]]; then
    main "$@"
fi
