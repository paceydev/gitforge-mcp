#!/usr/bin/env python3
"""CI status, logs, runners and pull requests on Gitea and Forgejo.

The benchmark's hardened skill arm: the rules a careful team learns the hard
way, kept in a helper script. Standard library only.

    forge.py status  [--sha SHA | --branch B | --run N]
    forge.py logs    [--sha SHA | --branch B | --run N] [--job NAME] [--all] [--tail 80] [--grep RE]
    forge.py runners
    forge.py prs     [--state open|closed|all]
    forge.py pr      NUMBER

Run it inside the repository's checkout: the forge, owner and repository come
from the `origin` remote. The token comes from $FORGE_TOKEN, else from git's
credential helper.

Exit codes for `status`: 0 success, 1 failure or nothing ran, 3 unfinished,
2 error.
"""

import argparse
import base64
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

FINAL = {"success", "failure", "cancelled", "skipped"}
# A commit status's state is coarse (a skipped job is `success`, a cancelled
# one `failure`); its description says what happened.
DESCRIPTIONS = [
    ("Waiting to run", "waiting"),
    ("Blocked by required conditions", "blocked"),
    ("Has started running", "running"),
    ("Successful in", "success"),
    ("Failing after", "failure"),
    ("Has been cancelled", "cancelled"),
    ("Has been skipped", "skipped"),
]


class ApiError(Exception):
    pass


def git(*args):
    out = subprocess.run(["git", *args], capture_output=True, text=True)
    return out.stdout.strip() if out.returncode == 0 else None


def remote():
    url = git("remote", "get-url", "origin")
    if not url:
        raise ApiError("no origin remote")
    m = re.match(r"^(https?)://(?:[^@/]+@)?([^/]+)(/.*)$", url)
    if m:
        scheme, host, path = m.groups()
    else:
        m = re.match(r"^(?:ssh://)?(?:[^@]+@)?([^:/]+)[:/](.*)$", url)
        if not m:
            raise ApiError("can't parse origin %s" % url)
        scheme, host, path = "https", m.group(1), "/" + m.group(2)
    parts = [p for p in path.split("/") if p]
    name = parts[-1][:-4] if parts[-1].endswith(".git") else parts[-1]
    owner = parts[-2]
    prefix = "/".join(parts[:-2])
    base = "%s://%s%s" % (scheme, host, "/" + prefix if prefix else "")
    return base, host, owner, name


def credential(scheme, host):
    token = os.environ.get("FORGE_TOKEN")
    if token:
        return "token " + token
    out = subprocess.run(
        ["git", "credential", "fill"],
        input="protocol=%s\nhost=%s\n\n" % (scheme, host),
        capture_output=True, text=True, env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    )
    kv = dict(l.split("=", 1) for l in out.stdout.splitlines() if "=" in l)
    if "password" not in kv:
        return None
    pair = "%s:%s" % (kv.get("username", ""), kv["password"])
    return "Basic " + base64.b64encode(pair.encode()).decode()


class Forge:
    def __init__(self):
        self.base, host, self.owner, self.name = remote()
        self.auth = credential(self.base.split(":")[0], host)
        self.forgejo, self.version = self.detect()

    def request(self, path, raw=False, api="/api/v1", optional=False):
        url = self.base + api + path
        headers = {"User-Agent": "forge-ci-skill", "Accept": "application/json"}
        if self.auth:
            headers["Authorization"] = self.auth
        for attempt in range(3):
            try:
                with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=30) as r:
                    body = r.read()
                    return body.decode("utf-8", "replace") if raw else json.loads(body or b"null")
            except urllib.error.HTTPError as e:
                if e.code == 404 and optional:
                    return None
                if e.code in (502, 503, 504) and attempt < 2:
                    time.sleep(1 + attempt)
                    continue
                detail = e.read().decode("utf-8", "replace")[:200]
                raise ApiError("%s -> %d %s" % (path, e.code, detail))
            except urllib.error.URLError as e:
                if attempt < 2:
                    time.sleep(1 + attempt)
                    continue
                raise ApiError("%s unreachable: %s" % (self.base, e))

    def repo(self, path):
        return "/repos/%s/%s%s" % (self.owner, self.name, path)

    def detect(self):
        fj = self.request("/version", api="/api/forgejo/v1", optional=True)
        raw = (fj or self.request("/version"))["version"]
        nums = [int(x) for x in re.findall(r"\d+", raw.split("+")[0])[:2]] + [0, 0]
        return fj is not None, (nums[0], nums[1])

    def has_jobs_endpoint(self):
        return self.version >= (16, 0) if self.forgejo else self.version >= (1, 25)

    # ------------------------------------------------------------ runs

    def runs(self, query):
        body = self.request(self.repo("/actions/runs?" + query)) or {}
        return [self.norm_run(r) for r in body.get("workflow_runs") or []]

    def norm_run(self, r):
        if self.forgejo:
            return {"id": r["id"], "number": r.get("index_in_repo"), "sha": r.get("commit_sha"),
                    "ref": r.get("prettyref"), "state": r.get("status"), "jobs": []}
        state = r.get("conclusion") or {"queued": "waiting", "in_progress": "running",
                                         "waiting": "blocked"}.get(r.get("status"), r.get("status"))
        return {"id": r["id"], "number": r.get("run_number"), "sha": r.get("head_sha"),
                "ref": r.get("head_branch"), "state": state, "jobs": []}

    def run_by_number(self, number):
        # A run's id and its number differ, and run endpoints take the id:
        # passing the number succeeds and returns some other run.
        if self.forgejo:
            for r in self.runs("run_number=%d" % number):
                if r["number"] == number:
                    return r
            return None
        for page in range(1, 21):
            batch = self.runs("limit=50&page=%d" % page)
            if not batch:
                return None
            for r in batch:
                if r["number"] == number:
                    return r
        return None

    def fill_jobs(self, runs, sha):
        if self.has_jobs_endpoint():
            for run in runs:
                body = self.request(self.repo("/actions/runs/%d/jobs?limit=100" % run["id"]))
                jobs = body if isinstance(body, list) else (body or {}).get("jobs") or []
                for j in jobs:
                    state = j.get("status")
                    if not self.forgejo:
                        state = j.get("conclusion") or {"queued": "waiting", "in_progress": "running",
                                                        "waiting": "blocked"}.get(state, state)
                    run["jobs"].append({"name": j.get("name"), "state": state, "job_id": j.get("id")})
        else:
            # No jobs endpoint: every job has a status from the moment its run
            # exists; a task only once a runner takes it.
            statuses = []
            for page in range(1, 11):
                batch = self.request(self.repo("/commits/%s/statuses?limit=50&page=%d" % (sha, page))) or []
                statuses += batch
                if len(batch) < 50:
                    break
            tasks = (self.request(self.repo("/actions/tasks?limit=50")) or {}).get("workflow_runs") or []
            for run in runs:
                latest = {}
                for s in statuses:
                    m = re.search(r"/actions/runs/(\d+)/jobs/(\d+)", s.get("target_url") or "")
                    if not m or int(m.group(1)) != run["number"]:
                        continue
                    if m.group(2) not in latest or s["id"] > latest[m.group(2)]["id"]:
                        latest[m.group(2)] = s
                for s in latest.values():
                    ctx = re.sub(r" \([^()]*\)$", "", s.get("context", ""))
                    name = ctx.split(" / ", 1)[-1]
                    state = next((st for prefix, st in DESCRIPTIONS
                                  if (s.get("description") or "").startswith(prefix)), s.get("status"))
                    task = max((t for t in tasks if t.get("run_number") == run["number"]
                                and t.get("name") == name), key=lambda t: t["id"], default=None)
                    if task:
                        state = task.get("status") or state
                    run["jobs"].append({"name": name, "state": state,
                                        "task_id": task["id"] if task else None})
        for run in runs:
            # A run still going with every listed job final has jobs nobody
            # lists yet. Don't let it look finished.
            if run["state"] not in FINAL and all(j["state"] in FINAL for j in run["jobs"]):
                run["jobs"].append({"name": "(not yet listed)", "state": run["state"] or "waiting"})

    def select(self, args):
        if args.run:
            run = self.run_by_number(args.run)
            if not run:
                raise ApiError("no run number %d" % args.run)
            self.fill_jobs([run], run["sha"])
            return run["sha"], [run]
        if args.branch:
            b = self.request(self.repo("/branches/" + urllib.parse.quote(args.branch, safe="/")))
            sha = b["commit"]["id"]
        else:
            sha = git("rev-parse", (args.sha or "HEAD") + "^{commit}") or args.sha
            if not sha or len(sha) != 40:
                raise ApiError("need the full 40-character sha (head_sha matches nothing shorter)")
        runs = self.runs("head_sha=%s&limit=50" % sha)
        runs = [r for r in runs if r["sha"] == sha]
        self.fill_jobs(runs, sha)
        return sha, runs


def verdict(runs):
    if not runs:
        return "no_runs"
    jobs = [j for r in runs for j in r["jobs"]]
    unfinished = any(r["state"] not in FINAL for r in runs) or any(j["state"] not in FINAL for j in jobs)
    failed = any(j["state"] in ("failure", "cancelled") for j in jobs)
    if unfinished:
        return "failing" if failed else "pending"
    if failed:
        return "failure"
    # Every job skipped means nothing ran: not a pass.
    if jobs and all(j["state"] == "skipped" for j in jobs):
        return "nothing_ran"
    return "success"


def cmd_status(f, args):
    sha, runs = f.select(args)
    print("commit %s" % sha)
    for r in runs:
        for j in r["jobs"]:
            print("  run %-4s %-10s %s" % (r["number"], j["state"], j["name"]))
    v = verdict(runs)
    print("VERDICT: %s" % v)
    return {"success": 0, "failure": 1, "nothing_ran": 1, "no_runs": 3}.get(v, 3)


def cmd_logs(f, args):
    if not f.has_jobs_endpoint():
        print("This forge version has no job log API (Forgejo needs 16+, Gitea 1.25+).")
        return 2
    sha, runs = f.select(args)
    shown = 0
    for r in runs:
        for j in r["jobs"]:
            if not j.get("job_id"):
                continue
            if args.job and args.job.lower() not in (j["name"] or "").lower():
                continue
            if not args.job and not args.all and j["state"] not in ("failure", "cancelled"):
                continue
            text = f.request(f.repo("/actions/jobs/%d/logs" % j["job_id"]), raw=True)
            lines = [re.sub(r"^\d{4}-\d\d-\d\dT[\d:.]+Z ", "", l) for l in text.splitlines()]
            if args.grep:
                lines = [l for l in lines if re.search(args.grep, l, re.I)]
            print("===== run %s job %s (%s)" % (r["number"], j["name"], j["state"]))
            print("\n".join(lines[-args.tail:] if args.tail else lines))
            shown += 1
    if not shown:
        print("no matching jobs (failed jobs only by default; use --all or --job NAME)")
    return 0


def cmd_runners(f, args):
    scopes = [("repository", f.repo("/actions/runners")), ("user", "/user/actions/runners"),
              ("org", "/orgs/%s/actions/runners" % f.owner), ("instance", "/admin/actions/runners")]
    seen = {}
    for scope, path in scopes:
        try:
            body = f.request(path, optional=True)
        except ApiError as e:
            print("%-10s not readable: %s" % (scope, e))
            continue
        items = body if isinstance(body, list) else (body or {}).get("runners") or []
        print("%-10s %d runners" % (scope, len(items)))
        for r in items:
            labels = [l if isinstance(l, str) else l.get("name") for l in r.get("labels") or []]
            state = r.get("status")
            if r.get("busy"):
                state = "busy"
            seen[r.get("id")] = (r.get("name"), state, labels)
    for name, state, labels in seen.values():
        print("  runner %-20s %-8s labels=%s" % (name, state, ",".join(labels)))
    path = "/actions/runners/jobs" if f.forgejo else "/actions/jobs?limit=50"
    body = f.request(f.repo(path), optional=True)
    jobs = body if isinstance(body, list) else (body or {}).get("jobs") or []
    for j in jobs:
        state = j.get("status")
        if state not in ("waiting", "queued"):
            continue
        labels = j.get("runs_on") or j.get("labels") or []
        able = [n for n, s, ls in seen.values() if all(l in ls for l in labels) and s != "offline"]
        print("  waiting job %-15s needs labels=%s; online runners with them: %s"
              % (j.get("name"), ",".join(labels), ", ".join(able) or "none"))
    return 0


def cmd_prs(f, args):
    for p in f.request(f.repo("/pulls?state=%s&limit=50" % args.state)) or []:
        print("#%d %-7s %s -> %s  %s" % (p["number"], "merged" if p.get("merged") else p["state"],
                                         p["head"]["ref"], p["base"]["ref"], p["title"]))
    return 0


def cmd_pr(f, args):
    p = f.request(f.repo("/pulls/%d" % args.number))
    print("#%d %s (%s)" % (p["number"], p["title"], "merged" if p.get("merged") else p["state"]))
    print("  %s -> %s, head %s" % (p["head"]["ref"], p["base"]["ref"], p["head"]["sha"]))
    print("  mergeable (no conflicts): %s" % p.get("mergeable"))
    runs = f.runs("head_sha=%s&limit=50" % p["head"]["sha"])
    runs = [r for r in runs if r["sha"] == p["head"]["sha"]]
    f.fill_jobs(runs, p["head"]["sha"])
    print("  CI on the head commit: %s" % verdict(runs))
    return 0


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("status", "logs"):
        s = sub.add_parser(name)
        s.add_argument("--sha")
        s.add_argument("--branch")
        s.add_argument("--run", type=int)
        if name == "logs":
            s.add_argument("--job")
            s.add_argument("--all", action="store_true")
            s.add_argument("--tail", type=int, default=80)
            s.add_argument("--grep")
    sub.add_parser("runners")
    s = sub.add_parser("prs")
    s.add_argument("--state", default="open")
    s = sub.add_parser("pr")
    s.add_argument("number", type=int)
    args = ap.parse_args()
    try:
        f = Forge()
        return {"status": cmd_status, "logs": cmd_logs, "runners": cmd_runners,
                "prs": cmd_prs, "pr": cmd_pr}[args.cmd](f, args)
    except ApiError as e:
        print("error: %s" % e, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
