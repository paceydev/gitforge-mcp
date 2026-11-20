#!/usr/bin/env python3
"""Select full CI unless a push or PR changes documentation alone."""
import json
import os
import re
import subprocess
from pathlib import Path


def documentation_only(event, ref, paths):
    if event not in {"push", "pull_request"} or ref.startswith("refs/tags/"):
        return False
    return bool(paths) and all(
        path.startswith("docs/") or ("/" not in path and path.endswith(".md"))
        for path in paths
    )


def main():
    event = os.environ["GITHUB_EVENT_NAME"]
    ref = os.environ["GITHUB_REF"]
    payload = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    base = (payload.get("pull_request", {}).get("base", {}).get("sha")
            if event == "pull_request" else payload.get("before", ""))
    head = os.environ["GITHUB_SHA"]
    valid = lambda sha: isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{40,64}", sha) and set(sha) != {"0"}
    paths = []
    if event in {"push", "pull_request"} and valid(base) and valid(head):
        # Keep deletion and addition paths separate so renaming code to a
        # Markdown filename cannot turn a source change into a docs-only run.
        result = subprocess.run(["git", "diff", "--no-renames", "--name-only", "-z", base, head, "--"],
                                capture_output=True, check=True)
        paths = [os.fsdecode(path) for path in result.stdout.split(b"\0") if path]
    full = not documentation_only(event, ref, paths)
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"full={'true' if full else 'false'}\n")
    print("Full CI" if full else "Documentation-only CI")


if __name__ == "__main__":
    main()
