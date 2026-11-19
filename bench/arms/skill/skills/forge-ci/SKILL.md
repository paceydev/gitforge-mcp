---
name: forge-ci
description: Check CI status, read CI logs, check runner availability, and read pull requests on a Gitea or Forgejo repository. Use when asked whether CI passed, why a build failed, why a job hasn't started, or whether a pull request is ready to merge.
---

# Forge CI (Gitea and Forgejo)

Use the bundled helper. Run it from inside the repository's checkout; it
reads the forge, owner and repository from `origin`, and the token from
`$FORGE_TOKEN` (or git's credential helper).

```bash
python3 "${CLAUDE_PLUGIN_ROOT}/skills/forge-ci/forge.py" <command>
```

| Command | Purpose |
|---|---|
| `status [--sha SHA \| --branch B \| --run N]` | Every job of every run for a commit (default: HEAD), and a `VERDICT:` line |
| `logs [--sha \| --branch \| --run] [--job NAME] [--all] [--tail 80] [--grep RE]` | Job logs; failed jobs only by default |
| `runners` | Runners in every scope the token can read, and jobs waiting for a runner |
| `prs [--state open\|closed\|all]` | List pull requests |
| `pr NUMBER` | One pull request: branches, whether it merges cleanly, CI on its head |

## Verdicts

`status` prints one of:

- `success`: every job finished, none failed, at least one ran.
- `failure`: every job finished and one failed or was cancelled.
- `failing`: a job failed and others are still running.
- `pending`: not finished yet.
- `nothing_ran`: every job was skipped.
- `no_runs`: no run exists for the commit yet.

## Rules that cost people time

Each of these has produced a confident wrong answer. The helper follows them;
follow them too if you read the API by hand.

- **Judge a commit by its runs, not by its newest job.** Jobs finish out of
  order, and a job no runner has taken yet has no task, so it may not be
  listed anywhere. A run that is still going is not finished, whatever its
  listed jobs say.
- **Skipped is not a result.** A job excluded by `if:` reports `skipped`, and
  its commit status says `success`. A commit whose jobs were all skipped ran
  nothing.
- **A commit status's state is coarse.** Read the description: "Has been
  skipped", "Has been cancelled", "Waiting to run", "Blocked by required
  conditions".
- **A run's id is not its number.** The web UI shows the number; endpoints
  under `/actions/runs/{id}` take the id. Passing the number there succeeds
  and returns an unrelated run. Use `--run N`, which resolves the number.
- **Filter runs by the full 40-character sha.** A short sha matches nothing.
- **Logs are long.** Read the failed job's tail, or `--grep` for the error,
  rather than the whole log. Forgejo before 16 has no log API at all.
- **"Mergeable" only means no conflicts.** A pull request with failing CI on
  its head isn't ready either.
- **Runners are listed per scope.** The repository's own list leaves out
  owner and instance runners. A job waiting for labels no online runner has
  will wait forever.
