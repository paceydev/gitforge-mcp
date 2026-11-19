#!/usr/bin/env python3
"""Summarises benchmark results (JSON lines from bench/run.py) as Markdown.

    bench/report.py bench/results/*.jsonl > bench/results/summary.md
"""

import importlib.util
import json
import os
import random
import statistics
import sys
from collections import defaultdict


HERE = os.path.dirname(os.path.abspath(__file__))


def load(paths):
    """Results, regraded against the current tasks.json, so a fix to a
    grading rule applies to past runs without spending on new ones."""
    spec = importlib.util.spec_from_file_location("bench_run", os.path.join(HERE, "run.py"))
    run = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(run)
    tasks = {t["id"]: t for t in json.load(open(os.path.join(HERE, "tasks.json")))["tasks"]}
    rows = []
    for p in paths:
        with open(p) as f:
            rows += [json.loads(l) for l in f if l.strip()]
    for r in rows:
        if r.get("kind") == "task":
            r["outcome"], r["false_green"] = run.grade(tasks[r["task"]], r.get("answer"), r["forge"])
    return rows


def tokens(r):
    u = r.get("usage") or {}
    return sum(u.get(k, 0) for k in (
        "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"))


def interval(values, stat=statistics.median, n=2000):
    """95% bootstrap interval, or None for fewer than 5 values."""
    if len(values) < 5:
        return None
    rng = random.Random(0)
    boots = sorted(stat([rng.choice(values) for _ in values]) for _ in range(n))
    return boots[int(0.025 * n)], boots[int(0.975 * n)]


def fmt(values, f="%.3f"):
    if not values:
        return "–"
    m = statistics.median(values)
    ci = interval(values)
    return (f % m) + ("" if ci is None else " (" + f % ci[0] + "–" + f % ci[1] + ")")


def main():
    rows = load(sys.argv[1:])
    tasks = [r for r in rows if r.get("kind") == "task"]
    rest = [r for r in rows if r.get("kind") == "at_rest"]
    models = sorted({r.get("model") or r.get("model_alias") for r in rows})
    forges = sorted({r["forge"] for r in rows})
    print("Model: %s. Forge: %s. Claude Code: %s.\n" % (
        ", ".join(map(str, models)), ", ".join(forges), ", ".join(sorted({r["claude_code"] for r in rows}))))

    by_arm = defaultdict(list)
    for r in tasks:
        by_arm[r["arm"]].append(r)
    print("| Arm | Runs | Correct | False green | Wrong | No answer | Cost per task (USD) | Tokens per task | Turns |")
    print("|---|---|---|---|---|---|---|---|---|")
    for arm in ("mcp", "skill", "naive", "none"):
        rs = by_arm.get(arm)
        if not rs:
            continue
        n = len(rs)
        count = lambda o: sum(1 for r in rs if r["outcome"] == o)
        fg = sum(1 for r in rs if r.get("false_green"))
        print("| %s | %d | %d (%.0f%%) | %d | %d | %d | %s | %s | %s |" % (
            arm, n, count("correct"), 100 * count("correct") / n, fg, count("wrong"), count("no_answer"),
            fmt([r["cost_usd"] for r in rs]), fmt([tokens(r) for r in rs], "%.0f"),
            fmt([r["num_turns"] or 0 for r in rs], "%.0f")))

    print("\nMedians; 95% bootstrap intervals in brackets where there are at least 5 runs.\n")
    ids = sorted({r["task"] for r in tasks})
    arms = [a for a in ("mcp", "skill", "naive", "none") if a in by_arm]
    print("| Task | " + " | ".join(arms) + " |")
    print("|---|" + "---|" * len(arms))
    for t in ids:
        cells = []
        for a in arms:
            rs = [r for r in tasks if r["task"] == t and r["arm"] == a]
            ok = sum(1 for r in rs if r["outcome"] == "correct")
            fg = sum(1 for r in rs if r.get("false_green"))
            cells.append("%d/%d%s" % (ok, len(rs), " (%d false green)" % fg if fg else ""))
        print("| %s | %s |" % (t, " | ".join(cells)))

    if rest:
        print("\nAt rest (\"Reply with OK.\"), input tokens of the first request:\n")
        print("| Arm | Input tokens | Over none |")
        print("|---|---|---|")
        base = statistics.median([r["first_request_input"] for r in rest if r["arm"] == "none"] or [0])
        for arm in ("none", "mcp", "skill", "naive"):
            vals = [r["first_request_input"] for r in rest if r["arm"] == arm and r["first_request_input"]]
            if vals:
                m = statistics.median(vals)
                print("| %s | %d | %+d |" % (arm, m, m - base))

    print("\nRuns that used the approach their arm provides:\n")
    for arm, key in (("mcp", "used_mcp"), ("skill", "used_skill"), ("naive", "used_skill")):
        rs = by_arm.get(arm)
        if rs:
            print("- %s: %d of %d" % (arm, sum(1 for r in rs if r.get(key)), len(rs)))
    bypass = [r for r in tasks if r["arm"] == "mcp" and any("curl" in c for c in r.get("bash_commands") or [])]
    if bypass:
        print("\n%d mcp-arm runs also called the API with curl." % len(bypass))


if __name__ == "__main__":
    main()
