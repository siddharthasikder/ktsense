#!/usr/bin/env python3
"""cold-index.py - measure kmp-lsp index time with a provably empty cache.

Usage:
    bench/cold-index.py <root> [<root> ...]

For each root, runs two LSP sessions against a fresh, empty cache directory
(kmp-lsp honours XDG_CACHE_HOME, so the developer's real ~/.cache/kmp-lsp is
never touched): first cold, then warm against the cache the cold run just
wrote. Each session sends initialize and initialized, then follows the
$/progress stream until the work-done `end`, and reports:

    initialize   seconds until the initialize response
    begin        seconds until indexing reported `begin`
    end          seconds until indexing reported `end` (the number that matters)
    status       the engine's own status.json: indexed, total, cache_hits, symbols

`cache_hits` is the proof of coldness: 0 on the cold run, equal to `total` on
the warm one. What this does NOT control is the operating system's page cache;
source files read recently are served from memory, so a first run after a
reboot may read slower. Set KTSENSE_COLD_TIMEOUT to raise the 300 s limit.

The engine is closed by stdin EOF, which is the only thing that ends it (see
AGENTS.md); no process is left behind.
"""

import glob
import json
import os
import select
import subprocess
import sys
import tempfile
import time

TIMEOUT_SECS = float(os.environ.get("KTSENSE_COLD_TIMEOUT", "300"))
IGNORE_PATTERNS = ["**/build/**"]


def frame(message):
    body = json.dumps(message).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


def messages(process, deadline):
    buffer = b""
    while time.time() < deadline:
        ready, _, _ = select.select([process.stdout], [], [], 0.05)
        if not ready:
            continue
        chunk = os.read(process.stdout.fileno(), 1 << 20)
        if not chunk:
            return
        buffer += chunk
        while b"\r\n\r\n" in buffer:
            head, rest = buffer.split(b"\r\n\r\n", 1)
            length = int(
                [h for h in head.split(b"\r\n") if h.lower().startswith(b"content-length")][0]
                .split(b":")[1]
            )
            if len(rest) < length:
                break
            body, buffer = rest[:length], rest[length:]
            yield json.loads(body)


def session(root, cache_home):
    env = {**os.environ, "RUST_LOG": "error", "XDG_CACHE_HOME": cache_home}
    process = subprocess.Popen(
        ["kmp-lsp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    started = time.time()
    initialize = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": None,
            "rootUri": "file://" + root,
            "capabilities": {},
            "initializationOptions": {"indexingOptions": {"ignorePatterns": IGNORE_PATTERNS}},
        },
    }
    process.stdin.write(frame(initialize))
    process.stdin.flush()

    timings = {"initialize": None, "begin": None, "end": None}
    for message in messages(process, started + TIMEOUT_SECS):
        if message.get("id") == 1 and "result" in message:
            timings["initialize"] = time.time() - started
            process.stdin.write(frame({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            process.stdin.flush()
        if message.get("method") == "$/progress":
            kind = message["params"]["value"].get("kind")
            if kind == "begin" and timings["begin"] is None:
                timings["begin"] = time.time() - started
            if kind == "end":
                timings["end"] = time.time() - started
                break
    process.stdin.close()
    process.wait(timeout=10)
    return timings


def engine_status(cache_home):
    for path in glob.glob(os.path.join(cache_home, "kmp-lsp", "status.json")):
        with open(path, encoding="utf-8") as handle:
            status = json.load(handle)
        return {key: status.get(key) for key in ("indexed", "total", "cache_hits", "symbols")}
    return {}


def seconds(value):
    return "n/a" if value is None else f"{value:.2f}s"


def report(label, timings, status):
    print(
        f"  {label:<5} initialize={seconds(timings['initialize'])} "
        f"begin={seconds(timings['begin'])} end={seconds(timings['end'])} status={status}"
    )


def main(roots):
    if not roots:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: bench/cold-index.py <root> [<root> ...]", file=sys.stderr)
        return 2
    for name in roots:
        root = os.path.abspath(name)
        cache_home = tempfile.mkdtemp(prefix="ktsense-cold-index-")
        print(f"{name} (empty cache at {cache_home})")
        cold = session(root, cache_home)
        report("cold", cold, engine_status(cache_home))
        warm = session(root, cache_home)
        report("warm", warm, engine_status(cache_home))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
