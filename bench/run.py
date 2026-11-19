#!/usr/bin/env python3
"""Runs the benchmark: each task, through each arm, in headless Claude Code.

    bench/run.py --forge forgejo-17 --arms mcp,skill --reps 1 --model sonnet

Needs the forges from bench/forges.sh (target/bench-targets.json) and an
ANTHROPIC_API_KEY. Each run gets an empty CLAUDE_CONFIG_DIR, so no settings,
memory, plugins, hooks or CLAUDE.md from the machine running it apply, and
the key is the only credential. (Not `--bare`: it lists a --plugin-dir skill
but never tells the model about it.) Each run is capped by --budget-per-run,
and the whole invocation stops before starting a run once --budget-total has
been spent.

Every run appends one JSON line to --out with the task, arm, model, Claude
Code version, the graded outcome, tokens, cost, turns, time and every tool
call. bench/report.py summarises them.
"""

import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
ARMS = ("mcp", "skill", "naive", "none")
OK_PROMPT = "Reply with OK."


def host_env(base_url):
    host = re.sub(r"^https?://", "", base_url).rstrip("/")
    return "GITFORGE_TOKEN_" + "".join(c.upper() if c.isalnum() else "_" for c in host)


def claude_version():
    out = subprocess.run(["claude", "--version"], capture_output=True, text=True)
    return out.stdout.strip()


def binary():
    path = os.path.join(ROOT, "target", "release", "gitforge-mcp")
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=ROOT, check=True)
    return path


def checkout(target, repo):
    """A clone of a private scenario repository, with the token used for the
    clone removed from the remote afterwards."""
    work = tempfile.mkdtemp(prefix="gitforge-bench-")
    url = "%s/%s/%s.git" % (target["base_url"], target["owner"], repo)
    scheme, rest = url.split("://", 1)
    authed = "%s://%s:%s@%s" % (scheme, target["owner"], target["token"], rest)
    subprocess.run(["git", "clone", "--quiet", authed, work + "/repo"], check=True)
    subprocess.run(["git", "-C", work + "/repo", "remote", "set-url", "origin", url], check=True)
    os.makedirs(work + "/claude-config")
    return work, work + "/repo"


def command(arm, prompt, model, budget, mcp_bin):
    cmd = [
        "claude", "-p", prompt,
        "--output-format", "stream-json", "--verbose",
        "--model", model,
        "--max-budget-usd", str(budget),
        "--no-session-persistence",
        "--strict-mcp-config",
        "--setting-sources", "",
        "--allowedTools", "Bash", "Read", "Grep", "Glob", "Skill", "mcp__gitforge",
    ]
    if arm == "mcp":
        cmd += ["--mcp-config", json.dumps({"mcpServers": {"gitforge": {"command": mcp_bin}}})]
    elif arm in ("skill", "naive"):
        cmd += ["--plugin-dir", os.path.join(HERE, "arms", arm)]
    return cmd


def run_agent(cmd, cwd, env, timeout):
    started = time.time()
    try:
        out = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
        stdout, stderr, timed_out = out.stdout, out.stderr, False
    except subprocess.TimeoutExpired as e:
        stdout = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
        stderr, timed_out = "timeout", True
    events = []
    for line in stdout.splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            pass
    return events, stderr[-2000:], timed_out, time.time() - started


def summarize(events):
    init = next((e for e in events if e.get("type") == "system" and e.get("subtype") == "init"), {})
    result = next((e for e in reversed(events) if e.get("type") == "result"), {})
    tools, bash = {}, []
    first_usage = None
    for e in events:
        if e.get("type") != "assistant":
            continue
        msg = e.get("message") or {}
        if first_usage is None and msg.get("usage"):
            first_usage = msg["usage"]
        for block in msg.get("content") or []:
            if block.get("type") == "tool_use":
                tools[block["name"]] = tools.get(block["name"], 0) + 1
                if block["name"] == "Bash":
                    bash.append(str((block.get("input") or {}).get("command", ""))[:300])
    usage = result.get("usage") or {}
    return {
        "model": init.get("model"),
        "mcp_servers": init.get("mcp_servers"),
        "skills_loaded": [s for s in init.get("skills") or [] if "forge" in str(s)],
        "result_text": result.get("result") or "",
        "is_error": result.get("is_error"),
        "subtype": result.get("subtype"),
        "num_turns": result.get("num_turns"),
        "duration_ms": result.get("duration_ms"),
        "cost_usd": result.get("total_cost_usd") or 0.0,
        "usage": {k: usage.get(k, 0) for k in (
            "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens")},
        "first_request_input": None if first_usage is None else sum(first_usage.get(k, 0) for k in (
            "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")),
        "tool_calls": tools,
        "bash_commands": bash,
        # Whether the run used the approach its arm provides at all.
        "used_mcp": any(t.startswith("mcp__gitforge__") for t in tools),
        "used_skill": tools.get("Skill", 0) > 0 or any("forge.py" in c for c in bash),
    }


def scrub(text, work, token):
    """Keeps local paths and the forge token out of published results."""
    text = text.replace(token, "<token>")
    for path, name in ((os.path.realpath(work), "<work>"), (work, "<work>"),
                       (ROOT, "<repo>"), (os.path.expanduser("~"), "~")):
        text = text.replace(path, name)
    # Recorded commands are cut short, which can leave a partial path that
    # the exact replacements above miss.
    text = re.sub(r"/(?:private/)?var/folders/[^\s\"'\\;|]*", "<tmp>", text)
    text = re.sub(r"/(?:Users|home)/[^/\s\"'\\;|]+", "~", text)
    return text


def parse_result(text):
    for line in reversed(text.strip().splitlines()):
        line = line.strip().strip("`")
        if line.startswith("RESULT:"):
            try:
                return json.loads(line[len("RESULT:"):].strip())
            except ValueError:
                return None
    return None


def as_text(value):
    return json.dumps(value) if isinstance(value, (list, dict)) else str(value)


def grade(task, answer, forge=""):
    """correct, wrong or no_answer, and whether it was a false green."""
    if not isinstance(answer, dict):
        return "no_answer", False
    g = task["grade"]
    for prefix, override in (task.get("grade_on") or {}).items():
        if forge.startswith(prefix):
            g = override
    value = answer.get(g["field"])
    fg = task.get("false_green")
    false_green = bool(fg) and value == fg["value"]
    if value is None or value == "":
        return "no_answer", False
    if "matches" in g:
        ok = re.search(g["matches"], as_text(value), re.I) is not None
    else:
        ok = value in g["correct"]
    for field, pattern in (g.get("and_matches") or {}).items():
        ok = ok and re.search(pattern, as_text(answer.get(field, "")), re.I) is not None
    return ("correct" if ok else "wrong"), false_green


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--targets", default=os.path.join(ROOT, "target", "bench-targets.json"))
    ap.add_argument("--forge", default="", help="name prefixes, comma-separated, e.g. forgejo-17")
    ap.add_argument("--arms", default="mcp,skill")
    ap.add_argument("--tasks", default="", help="comma-separated ids; default all")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--budget-per-run", type=float, default=0.5)
    ap.add_argument("--budget-total", type=float, default=6.0)
    ap.add_argument("--timeout", type=int, default=420)
    ap.add_argument("--at-rest", action="store_true", help="also measure each arm's cost at rest")
    ap.add_argument("--out", default="")
    ap.add_argument("--jobs", type=int, default=1,
                    help="runs at once; the tasks only read, so runs don't disturb each other")
    args = ap.parse_args()

    if not os.environ.get("ANTHROPIC_API_KEY"):
        sys.exit("ANTHROPIC_API_KEY is not set: claude --bare can't use a keychain login")
    arms = [a for a in args.arms.split(",") if a]
    for a in arms:
        if a not in ARMS:
            sys.exit("unknown arm %s (choose from %s)" % (a, ", ".join(ARMS)))
    spec = json.load(open(os.path.join(HERE, "tasks.json")))
    tasks = [t for t in spec["tasks"] if not args.tasks or t["id"] in args.tasks.split(",")]
    if not os.path.exists(args.targets):
        sys.exit("%s doesn't exist: start the forges first with bench/forges.sh "
                 "(they don't survive a Docker restart, and `cargo clean` removes the file)"
                 % args.targets)
    prefixes = [f for f in args.forge.split(",") if f] or [""]
    targets = [t for t in json.load(open(args.targets)) if any(t["name"].startswith(f) for f in prefixes)]
    if not targets:
        sys.exit("no forge in %s matches %r" % (args.targets, args.forge))

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_path = args.out or os.path.join(HERE, "results", "%s-%s.jsonl" % (stamp, args.model))
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    mcp_bin = binary()
    version = claude_version()

    jobs = []
    for target in targets:
        if args.at_rest:
            for arm in sorted(set(arms) | {"none"}):
                jobs.append((target, None, arm, 0))
        for task in tasks:
            for rep in range(args.reps):
                for arm in arms:
                    jobs.append((target, task, arm, rep))

    print("%d runs -> %s" % (len(jobs), out_path), file=sys.stderr)
    lock = threading.Lock()
    state = {"spent": 0.0, "done": 0}
    out = open(out_path, "a")

    def one(job):
        target, task, arm, rep = job
        with lock:
            if state["spent"] >= args.budget_total:
                return
        repo = task["repo"] if task else "bench-green"
        if task:
            question = task["prompt"].replace("{renumbered_run}", str(target["renumbered_run"]))
            prompt = question + "\n\n" + spec["result_instruction"] + task["shape"]
        else:
            prompt = OK_PROMPT
        work, cwd = checkout(target, repo)
        env = dict(os.environ)
        env.update({
            "FORGE_TOKEN": target["token"],
            host_env(target["base_url"]): target["token"],
            "GITFORGE_MCP_CACHE": os.path.join(work, "probes.json"),
            "GITFORGE_MCP_CONFIG": os.path.join(work, "no-config.toml"),
            "CLAUDE_CONFIG_DIR": os.path.join(work, "claude-config"),
        })
        events, stderr, timed_out, wall = run_agent(
            command(arm, prompt, args.model, args.budget_per_run, mcp_bin), cwd, env, args.timeout)
        s = summarize(events)
        record = {
            "at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "kind": "task" if task else "at_rest",
            "task": task["id"] if task else None,
            "arm": arm,
            "rep": rep,
            "forge": target["name"],
            "model_alias": args.model,
            "claude_code": version,
            "timed_out": timed_out,
            "wall_seconds": round(wall, 1),
            **{k: v for k, v in s.items() if k != "result_text"},
            "answer_text": s["result_text"][-1500:],
        }
        if task:
            answer = parse_result(s["result_text"])
            outcome, false_green = grade(task, answer, target["name"])
            record.update({"answer": answer, "outcome": outcome, "false_green": false_green})
        if not events:
            record["stderr"] = stderr
        line = scrub(json.dumps(record), work, target["token"])
        shutil.rmtree(work, ignore_errors=True)
        with lock:
            state["spent"] += s["cost_usd"]
            state["done"] += 1
            out.write(line + "\n")
            out.flush()
            label = record.get("outcome", "at rest: %s input tokens" % s["first_request_input"])
            print("[%d/%d] %-15s %-5s %-6s %s  $%.3f (total $%.2f)" % (
                state["done"], len(jobs), target["name"], record["task"] or "-", arm, label,
                s["cost_usd"], state["spent"]), file=sys.stderr)

    # At-rest runs first and alone, so they all start from the same cache state.
    rest = [j for j in jobs if j[1] is None]
    for job in rest:
        one(job)
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        list(pool.map(one, [j for j in jobs if j[1] is not None]))
    out.close()
    if state["spent"] >= args.budget_total:
        print("stopped at the budget: spent $%.2f of $%.2f" % (state["spent"], args.budget_total),
              file=sys.stderr)
    check_at_rest(out_path)
    print(out_path)


def check_at_rest(path):
    """Warns when an arm adds nothing at rest: its approach isn't visible to
    the model, and its results would really be the `none` arm's."""
    rows = [json.loads(l) for l in open(path) if l.strip()]
    rest = {r["arm"]: r["first_request_input"] or 0 for r in rows if r.get("kind") == "at_rest"}
    floor = rest.get("none")
    for arm, n in rest.items():
        if arm != "none" and floor is not None and n - floor < 20:
            print("WARNING: the %s arm adds only %d tokens at rest; its approach is probably not loaded"
                  % (arm, n - floor), file=sys.stderr)


if __name__ == "__main__":
    main()
