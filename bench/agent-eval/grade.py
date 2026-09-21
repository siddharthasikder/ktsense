#!/usr/bin/env python3
"""Prompt rendering, answer extraction and grading for the agent evaluation harness.

`run.sh` owns process orchestration, timing and the resource gate; this file owns everything that
reads `questions.yaml`. Keeping the split means a grading change cannot alter how a session is run.

Subcommands:

    ids      QUESTIONS                          question ids, one per line
    prompt   QUESTIONS QID ROOT                 the full prompt text for one question
    grade    QUESTIONS QID TRANSCRIPT           verdict TAB answer, tab separated
    regrade  QUESTIONS RESULTS_CSV TRANSCRIPTS  rescore an existing run from its transcripts
    summary  QUESTIONS RESULTS_CSV              the results tables, as markdown

A verdict is one of `correct`, `wrong`, `no_answer_line` or `ungraded`. Only `correct` counts as
correct. `no_answer_line` is reported separately rather than folded into `wrong`, because an arm that
answered in prose without the requested final line failed to follow the format rather than failed to
navigate. `ungraded` is a question carrying `graded: false`, which is how a question whose own ground
truth turned out to be indefensible is retired without deleting the answers it drew.

`regrade` exists because a question's ground truth can be found wanting after the sessions have run.
Scoring reads only the stored transcripts, so a correction costs no credits and no rerun, and the
recorded answers stay exactly as the arms gave them.
"""

import csv
import re
import sys
from pathlib import Path

import yaml

ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07")
ANSWER_LINE = re.compile(r"ANSWER:\s*(.*)", re.IGNORECASE)
PLACEHOLDER = "<your answer"


def load(path):
    return yaml.safe_load(Path(path).read_text(encoding="utf-8"))


def question(spec, qid):
    for entry in spec["questions"]:
        if entry["id"] == qid:
            return entry
    raise SystemExit(f"grade.py: no such question: {qid}")


def render_prompt(spec, qid, root):
    entry = question(spec, qid)
    preamble = spec["preamble"].replace("{root}", root)
    return f"{preamble}\n{entry['ask'].strip()}\n"


def extract_answer(transcript_text):
    """The last `ANSWER:` line in the visible transcript, with decoration stripped."""
    found = None
    for line in ANSI.sub("", transcript_text).splitlines():
        line = line.strip().lstrip("> ").strip()
        if PLACEHOLDER in line:
            continue
        match = ANSWER_LINE.search(line)
        if match:
            found = match.group(1).strip()
    return found


def grade(spec, qid, transcript):
    entry = question(spec, qid)
    answer = extract_answer(Path(transcript).read_text(encoding="utf-8", errors="replace"))
    if entry.get("graded", True) is False:
        return "ungraded", answer or ""
    if not answer:
        return "no_answer_line", ""
    expected = [re.compile(pattern, re.IGNORECASE) for pattern in entry.get("expect", [])]
    forbidden = [re.compile(pattern, re.IGNORECASE) for pattern in entry.get("forbid", [])]
    hit = all(pattern.search(answer) for pattern in expected)
    tripped = any(pattern.search(answer) for pattern in forbidden)
    return ("correct" if hit and not tripped else "wrong"), answer


def regrade(spec, results_csv, transcripts):
    path = Path(results_csv)
    with path.open(encoding="utf-8") as handle:
        reader = csv.DictReader(handle)
        fields = reader.fieldnames
        rows = list(reader)
    changed = 0
    for row in rows:
        name = f"{row['question']}-{row['arm']}-rep{row['rep']}.txt"
        transcript = Path(transcripts, name)
        if not transcript.exists():
            continue
        verdict, answer = grade(spec, row["question"], transcript)
        if row["verdict"] != verdict:
            changed += 1
        row["verdict"], row["answer"] = verdict, answer
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
    return changed


def summary(spec, results_csv):
    rows = list(csv.DictReader(Path(results_csv).open(encoding="utf-8")))
    if not rows:
        raise SystemExit("grade.py: no rows to summarize")
    arms = sorted({row["arm"] for row in rows})
    ids = [entry["id"] for entry in spec["questions"] if any(r["question"] == entry["id"] for r in rows)]
    out = []

    def cell(qid, arm):
        matching = [r for r in rows if r["question"] == qid and r["arm"] == arm]
        if not matching:
            return None
        return matching[-1]

    out.append("| Question | Kind | " + " | ".join(f"{arm}" for arm in arms) + " |")
    out.append("|---|---|" + "---|" * len(arms))
    for qid in ids:
        kind = question(spec, qid)["kind"]
        cells = []
        for arm in arms:
            row = cell(qid, arm)
            if row is None:
                cells.append("not run")
                continue
            mark = {
                "correct": "correct",
                "wrong": "wrong",
                "no_answer_line": "no answer line",
                "ungraded": "ungraded",
            }[row["verdict"]]
            cells.append(f"{mark}, {row['wall_ms']} ms, {row['tool_calls']} calls")
        out.append(f"| {qid} | {kind} | " + " | ".join(cells) + " |")

    out.append("")
    out.append("| Arm | Sessions | Graded | Correct | Wrong | No answer line | Ungraded | Median wall | Tool calls | ktsense calls |")
    out.append("|---|---|---|---|---|---|---|---|---|---|")
    for arm in arms:
        arm_rows = [r for r in rows if r["arm"] == arm]
        times = sorted(int(r["wall_ms"]) for r in arm_rows)
        median = times[(len(times) - 1) // 2] if times else 0
        counts = {
            verdict: sum(1 for r in arm_rows if r["verdict"] == verdict)
            for verdict in ("correct", "wrong", "no_answer_line", "ungraded")
        }
        graded = len(arm_rows) - counts["ungraded"]
        out.append(
            f"| {arm} | {len(arm_rows)} | {graded} | {counts['correct']} | {counts['wrong']} | "
            f"{counts['no_answer_line']} | {counts['ungraded']} | {median} ms | "
            f"{sum(int(r['tool_calls']) for r in arm_rows)} | "
            f"{sum(int(r['ktsense_calls']) for r in arm_rows)} |"
        )
    return "\n".join(out)


def main(argv):
    if len(argv) < 3:
        raise SystemExit(__doc__)
    command, questions = argv[1], argv[2]
    spec = load(questions)
    if command == "ids":
        print("\n".join(entry["id"] for entry in spec["questions"]))
    elif command == "prompt":
        print(render_prompt(spec, argv[3], argv[4]), end="")
    elif command == "grade":
        verdict, answer = grade(spec, argv[3], argv[4])
        print(f"{verdict}\t{answer}")
    elif command == "regrade":
        changed = regrade(spec, argv[3], argv[4])
        print(f"regraded {argv[3]}: {changed} verdict(s) changed")
    elif command == "summary":
        print(summary(spec, argv[3]))
    else:
        raise SystemExit(f"grade.py: unknown subcommand: {command}")


if __name__ == "__main__":
    main(sys.argv)
