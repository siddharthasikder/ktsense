#!/usr/bin/env bash
#
# run.sh - ask the questions in questions.yaml of two agents, one with the ktsense MCP server and one
#          without, and record what each one called, how long it took and whether it was right.
#
# Usage:
#   bench/agent-eval/run.sh --root <kotlin-repo> [options]
#
#   --root PATH        The Kotlin repository the questions are about. Required. Both arms are given
#                      this path; the ktsense arm's server is launched with it as `--root`.
#   --out DIR          Where results and transcripts go (default: bench/agent-eval/runs/<timestamp>).
#   --questions FILE   Question file (default: questions.yaml beside this script).
#   --arm ARM          `both` (default), `baseline` or `ktsense`.
#   --only ID          Repeatable. Run just these question ids.
#   --reps N           Sessions per cell (default 1). See the wall time note below.
#   --timeout SECS     Per session (default 600).
#   --min-free-mb MB   Refuse to start a session when available memory is below this (default 4096).
#   --keep-workspace   Leave the throwaway Kiro workspace behind for inspection.
#   --keep-going       Record a failed cell and continue instead of stopping. Off by default.
#
# Output          CSV at <out>/results.csv, one row per session:
#
#   question,arm,rep,verdict,wall_ms,exit,tool_calls,ktsense_calls,load1,answer
#
#                 Full transcripts land in <out>/transcripts/. Everything that is not CSV goes to
#                 stderr, so <out>/results.csv is always a clean file.
#
# Arms            Two agent configurations, both written into a throwaway workspace this script
#                 creates and deletes. Neither touches the operator's ~/.kiro: that directory is
#                 shared with every other live session on the host.
#
#                 baseline: read, grep, glob, ls and shell. A capable code-searching agent.
#                 ktsense:  the same five, plus the eight tools of the ktsense MCP server.
#
#                 So the comparison is what ktsense adds to an agent that can already search, rather
#                 than ktsense against an agent with no tools at all.
#
# The MCP opt-in  A server named in a `settings/mcp.json` file reaches a session only if the active
#                 agent sets `"useLegacyMcpJson": true`; without it a session starts with none of the
#                 tools and looks exactly like a session that chose not to use them. This script
#                 therefore declares the server inside the agent configuration, which is always
#                 honoured, and proves the tools arrived before running the matrix: a preflight
#                 session lists its own tools and the run aborts unless all eight are present.
#
#                 On top of that, a ktsense-arm session that makes zero ktsense tool calls is a
#                 failed cell, not a result. Silently scoring it would publish a comparison of the
#                 baseline against itself.
#
# Wall time       Recorded with the bash `time` keyword, as in bench/latency.sh, and the one-minute
#                 load average is sampled next to every row, because a number taken from a host with
#                 five other agents compiling on it is not a latency measurement. At --reps 1 these
#                 numbers are indicative only; they are also dominated by model latency rather than
#                 by tool latency, so they say more about how many turns an arm needed than about how
#                 fast a tool is.
#
# Corpus          `bin` directories hold byte-identical copies of the sources, written by the Eclipse
#                 Buildship import that kmp-lsp triggers on a Gradle project. The two arms do not see
#                 them alike: ktsense's tree-sitter traversal prunes `bin`, while the engine-backed
#                 tools index it. Every prompt therefore tells both arms to ignore `bin`, and this
#                 script reports the duplicate count so a reader can see what was excluded. Pruning
#                 the corpus instead would not hold, because the engine rewrites those directories.
#
# Exit status: 0 when every cell ran and was scored, 1 when a cell failed or the preflight failed,
# 2 on a usage error.

set -euo pipefail

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

script_dir=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

root=""
out=""
questions="$script_dir/questions.yaml"
arm=both
only=""
reps=1
timeout_secs=600
min_free_mb=4096
keep_workspace=0
keep_going=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --root) [ "$#" -ge 2 ] || { echo "run.sh: --root needs a value" >&2; exit 2; }; root=$2; shift 2 ;;
        --out) [ "$#" -ge 2 ] || { echo "run.sh: --out needs a value" >&2; exit 2; }; out=$2; shift 2 ;;
        --questions) [ "$#" -ge 2 ] || { echo "run.sh: --questions needs a value" >&2; exit 2; }; questions=$2; shift 2 ;;
        --arm)
            [ "$#" -ge 2 ] || { echo "run.sh: --arm needs a value" >&2; exit 2; }
            case "$2" in both|baseline|ktsense) arm=$2 ;; *) echo "run.sh: --arm wants both, baseline or ktsense" >&2; exit 2 ;; esac
            shift 2 ;;
        --only) [ "$#" -ge 2 ] || { echo "run.sh: --only needs a value" >&2; exit 2; }; only="$only $2"; shift 2 ;;
        --reps) [ "$#" -ge 2 ] || { echo "run.sh: --reps needs a value" >&2; exit 2; }; reps=$2; shift 2 ;;
        --timeout) [ "$#" -ge 2 ] || { echo "run.sh: --timeout needs a value" >&2; exit 2; }; timeout_secs=$2; shift 2 ;;
        --min-free-mb) [ "$#" -ge 2 ] || { echo "run.sh: --min-free-mb needs a value" >&2; exit 2; }; min_free_mb=$2; shift 2 ;;
        --keep-workspace) keep_workspace=1; shift ;;
        --keep-going) keep_going=1; shift ;;
        --) shift; break ;;
        -*) echo "run.sh: unknown option: $1" >&2; usage 2 ;;
        *) echo "run.sh: unexpected argument: $1" >&2; usage 2 ;;
    esac
done

[ -n "$root" ] || { echo "run.sh: missing --root" >&2; usage 2; }
[ -d "$root" ] || { echo "run.sh: not a directory: $root" >&2; exit 2; }
[ -f "$questions" ] || { echo "run.sh: no such question file: $questions" >&2; exit 2; }
for value in "$reps" "$timeout_secs" "$min_free_mb"; do
    case "$value" in ''|*[!0-9]*) echo "run.sh: expected a whole number, got: $value" >&2; exit 2 ;; esac
done
[ "$reps" -ge 1 ] || { echo "run.sh: --reps must be at least 1" >&2; exit 2; }

command -v kiro-cli >/dev/null || { echo "run.sh: kiro-cli is not on PATH" >&2; exit 2; }
command -v python3 >/dev/null || { echo "run.sh: python3 is not on PATH" >&2; exit 2; }

resolve_bin() {
    if [ -n "${KTSENSE_BIN:-}" ]; then
        command -v -- "$KTSENSE_BIN" && return 0
        echo "run.sh: KTSENSE_BIN is set but not executable: $KTSENSE_BIN" >&2
        return 1
    fi
    for candidate in "$repo_root/target/release/ktsense" "$repo_root/target/debug/ktsense"; do
        [ -x "$candidate" ] && { printf '%s\n' "$candidate"; return 0; }
    done
    command -v ktsense && return 0
    return 1
}

if ! bin=$(resolve_bin); then
    echo "run.sh: no ktsense binary found. Build ktsense-cli or set KTSENSE_BIN." >&2
    exit 2
fi

root_abs=$(cd "$root" && pwd)
grader="$script_dir/grade.py"
[ -f "$grader" ] || { echo "run.sh: missing grade.py beside this script" >&2; exit 2; }

: "${out:=$script_dir/runs/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out/transcripts"
out=$(cd "$out" && pwd)
results="$out/results.csv"
echo 'question,arm,rep,verdict,wall_ms,exit,tool_calls,ktsense_calls,load1,answer' > "$results"

workspace=$(mktemp -d "${TMPDIR:-/tmp}/ktsense-agent-eval-XXXXXX")
cleanup() {
    if [ "$keep_workspace" -eq 0 ]; then
        rm -rf "$workspace"
    else
        echo "run.sh: workspace left at $workspace" >&2
    fi
}
trap cleanup EXIT INT TERM

mkdir -p "$workspace/.kiro/agents"
python3 - "$workspace/.kiro/agents" "$bin" "$root_abs" <<'PY'
import json, sys
from pathlib import Path

agents, binary, root = sys.argv[1], sys.argv[2], sys.argv[3]
shared = ["read", "grep", "glob", "ls", "shell"]

baseline = {
    "name": "eval-baseline",
    "description": "KT-39 baseline arm: generic file tools, no ktsense",
    "tools": shared,
    "allowedTools": shared,
    "resources": [],
    "mcpServers": {},
}
ktsense = {
    "name": "eval-ktsense",
    "description": "KT-39 with-MCP arm: the same generic file tools plus the ktsense MCP server",
    "tools": shared + ["@ktsense"],
    "allowedTools": shared + ["@ktsense"],
    "resources": [],
    "mcpServers": {
        "ktsense": {"command": binary, "args": ["--root", root, "mcp"], "timeout": 120000}
    },
}
for name, config in (("eval-baseline", baseline), ("eval-ktsense", ktsense)):
    Path(agents, f"{name}.json").write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
PY

strip_ansi() {
    sed -r 's/\x1B\[[0-9;?]*[a-zA-Z]//g; s/\x1B\][^\x07]*\x07//g'
}

load1() {
    cut -d' ' -f1 < /proc/loadavg 2>/dev/null || echo na
}

available_mb() {
    awk '/^MemAvailable:/ { printf "%d", $2 / 1024 }' /proc/meminfo 2>/dev/null || echo 0
}

kotlin_files=$(cd "$root_abs" && find . \
    \( -type d \( -name .git -o -name target -o -name build -o -name bin -o -name .gradle \
        -o -name .idea -o -name node_modules \) -prune \) \
    -o \( -type f -name '*.kt' -print \) | wc -l)
bin_duplicates=$(find "$root_abs" -type f -name '*.kt' -path '*/bin/*' | wc -l)

{
    printf 'run.sh: corpus %s\n' "$root_abs"
    printf 'run.sh: %d .kt files after pruning, %d duplicate copies under bin/ excluded by prompt\n' \
        "$kotlin_files" "$bin_duplicates"
    printf 'run.sh: ktsense %s (%s)\n' "$bin" "$("$bin" --version 2>/dev/null | head -1)"
    printf 'run.sh: engine %s\n' "$(kmp-lsp --version 2>/dev/null | head -1 || echo 'not on PATH')"
    printf 'run.sh: kiro-cli %s\n' "$(kiro-cli --version 2>/dev/null | head -1)"
    printf 'run.sh: workspace %s\n' "$workspace"
    printf 'run.sh: results %s\n' "$results"
    printf 'run.sh: load at start %s, available memory %s MB\n' "$(load1)" "$(available_mb)"
} >&2

agent_for() {
    case "$1" in
        baseline) printf 'eval-baseline\n' ;;
        ktsense) printf 'eval-ktsense\n' ;;
    esac
}

# One session. Writes the stripped transcript to $2 and prints the elapsed milliseconds.
# `time` is a bash keyword, so nothing forks inside the measured span.
session() {
    agent=$1
    transcript=$2
    prompt=$3
    raw="$transcript.raw"
    TIMEFORMAT='%3R'
    elapsed=$( { time { ( cd "$workspace" && timeout "$timeout_secs" kiro-cli chat \
        --no-interactive --trust-all-tools --agent "$agent" "$prompt" > "$raw" 2>&1 ) \
        && status=0 || status=$?; echo "$status" > "$transcript.exit"; } ; } 2>&1 )
    strip_ansi < "$raw" > "$transcript"
    rm -f "$raw"
    python3 -c "import sys; print(round(float(sys.argv[1]) * 1000))" "$elapsed"
}

fail_count=0

# Two checks, because neither alone is enough. The handshake proves the server exposes the tools and
# involves no model, so it cannot be confounded by an agent that simply chose not to mention one. The
# session check proves the tools actually reached a chat session, which is the thing the
# `useLegacyMcpJson` trap silently breaks. A count of self-reported names is recorded for interest but
# not gated on: an agent asked to list its tools sometimes omits the one it is about to call.
preflight() {
    echo "run.sh: preflight, proving the server exposes the tools and a session can call them" >&2

    handshake="$out/transcripts/preflight-handshake.json"
    printf '%s\n' \
      '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"run.sh","version":"0"}}}' \
      '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
      '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
      | "$bin" --root "$root_abs" mcp > "$handshake" 2>/dev/null || true
    exposed=$(python3 - "$handshake" <<'PY'
import json, sys
names = []
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    for tool in (message.get("result") or {}).get("tools", []):
        names.append(tool["name"])
print(len(names))
PY
)
    printf 'run.sh: preflight handshake exposed %s tools\n' "$exposed" >&2

    transcript="$out/transcripts/preflight-session.txt"
    session eval-ktsense "$transcript" \
        "Call the get_kotlin_repo_map tool once, then output only the word done." >/dev/null
    status=$(cat "$transcript.exit")
    calls=$(grep -c 'from mcp server: ktsense' "$transcript" || true)
    listed=$(grep -cE '^(> )?(analyze_kotlin_dependencies|check_kotlin_syntax|explain_kotlin_symbol|find_kotlin_symbol|get_kotlin_outline|get_kotlin_repo_map|ktsense_status|trace_kotlin_symbol)[[:space:]]*$' \
        "$transcript" || true)
    printf 'run.sh: preflight session exit %s, %s ktsense call(s) observed (%s tool names echoed)\n' \
        "$status" "$calls" "$listed" >&2

    if [ "$exposed" -ne 8 ] || [ "$status" -ne 0 ] || [ "$calls" -lt 1 ]; then
        {
            echo "run.sh: PREFLIGHT FAILED."
            printf '  tools exposed by the server: %s of 8. ktsense calls seen in a session: %s.\n' \
                "$exposed" "$calls"
            echo "  A run in this state would compare the baseline against itself."
            echo "  Check the agent configuration in $workspace/.kiro/agents/eval-ktsense.json,"
            echo "  and that $bin --root $root_abs mcp answers a tools/list handshake."
            echo "  Transcripts: $handshake and $transcript"
        } >&2
        exit 1
    fi
}

run_cell() {
    question_id=$1
    arm_name=$2
    rep=$3

    free_mb=$(available_mb)
    if [ "$free_mb" -lt "$min_free_mb" ]; then
        {
            echo "run.sh: STOPPING. Available memory ${free_mb} MB is below the ${min_free_mb} MB floor."
            echo "  Sessions on this host cost one to two GB each and sibling agents hold uncommitted"
            echo "  work, so an out-of-memory kill would not only be this run's problem."
            echo "  Completed cells are in $results."
        } >&2
        exit 1
    fi

    agent=$(agent_for "$arm_name")
    prompt=$(python3 "$grader" prompt "$questions" "$question_id" "$root_abs")
    transcript="$out/transcripts/$question_id-$arm_name-rep$rep.txt"
    load=$(load1)

    printf 'run.sh: %s %s rep %s (load %s, %s MB free) ' "$question_id" "$arm_name" "$rep" "$load" "$free_mb" >&2
    wall_ms=$(session "$agent" "$transcript" "$prompt")
    status=$(cat "$transcript.exit")

    ktsense_calls=$(grep -c 'from mcp server: ktsense' "$transcript" || true)
    native_calls=$(grep -cE '\(using tool: [a-z_]+\)' "$transcript" || true)
    tool_calls=$((ktsense_calls + native_calls))

    read -r verdict answer <<EOF || true
$(python3 "$grader" grade "$questions" "$question_id" "$transcript")
EOF

    printf '%s, %s ms, %s calls (%s ktsense), exit %s\n' \
        "$verdict" "$wall_ms" "$tool_calls" "$ktsense_calls" "$status" >&2

    python3 - "$results" "$question_id" "$arm_name" "$rep" "$verdict" "$wall_ms" "$status" \
        "$tool_calls" "$ktsense_calls" "$load" "${answer:-}" <<'PY'
import csv, sys
path, *fields = sys.argv[1:]
with open(path, "a", newline="", encoding="utf-8") as handle:
    csv.writer(handle).writerow(fields)
PY

    problem=""
    if [ "$status" -ne 0 ]; then
        problem="the session exited $status"
    elif [ "$arm_name" = ktsense ] && [ "$ktsense_calls" -eq 0 ]; then
        problem="the ktsense arm made no ktsense tool call, so this cell is not a with-MCP result"
    elif [ "$tool_calls" -eq 0 ]; then
        problem="the session answered without calling any tool, so it did not consult the corpus"
    fi

    if [ -n "$problem" ]; then
        fail_count=$((fail_count + 1))
        {
            echo "run.sh: CELL FAILED $question_id $arm_name rep $rep: $problem"
            echo "  Transcript: $transcript"
        } >&2
        if [ "$keep_going" -eq 0 ]; then
            {
                echo "run.sh: stopping rather than filling the rest of the table with cells that"
                echo "  cannot be interpreted. Pass --keep-going to record and carry on."
            } >&2
            exit 1
        fi
    fi
}

if [ -n "$only" ]; then
    ids=$only
else
    ids=$(python3 "$grader" ids "$questions")
fi

case "$arm" in
    both) arms="baseline ktsense" ;;
    *) arms=$arm ;;
esac

if printf '%s\n' $arms | grep -qx ktsense; then
    preflight
fi

rep=1
while [ "$rep" -le "$reps" ]; do
    for question_id in $ids; do
        for arm_name in $arms; do
            run_cell "$question_id" "$arm_name" "$rep"
        done
    done
    rep=$((rep + 1))
done

rm -f "$out"/transcripts/*.exit

{
    echo
    echo "run.sh: summary"
    python3 "$grader" summary "$questions" "$results"
    echo
    printf 'run.sh: load at end %s, available memory %s MB\n' "$(load1)" "$(available_mb)"
} >&2

cat "$results"

if [ "$fail_count" -gt 0 ]; then
    printf 'run.sh: %d cell(s) failed; the table above is not a clean result\n' "$fail_count" >&2
    exit 1
fi
