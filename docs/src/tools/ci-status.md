# ci_status

The CI verdict for a commit, with every run and job behind it, where CI has got to, and when it will likely end.

## Parameters

| Parameter | Meaning |
|---|---|
| `sha` | A commit sha, full or abbreviated. Default: the checkout's `HEAD`. |
| `branch` | A branch on the forge: its latest commit. |
| `run` | A run number, as in the forge's run URLs (on GitHub, the run id). |
| `jobs` | Which jobs to list: `attention` (default: failed, cancelled and unfinished ones), `all`, or `none`. |

## Reply


```jsonc
{
  "verdict": "pending",
  "summary": "1 run, 3 jobs: 1 running, 1 waiting, 1 success",
  "progress": "1 of 3 jobs finished (1 success); running: test (step 3 of 5: cargo test); waiting: macos",
  "target": "HEAD of main",
  "runs": [{ "number": 20, "state": "running", "jobs": [
    { "name": "frontend", "state": "success", "duration_seconds": 67 },
    { "name": "test", "state": "running", "steps": [
      { "name": "Set up job", "state": "success", "duration_seconds": 4 },
      { "name": "Build", "state": "success", "duration_seconds": 81 },
      { "name": "cargo test", "state": "running" }
      // and the steps still to come
    ]},
    { "name": "macos", "state": "waiting" }
  ]}]
}
```

| `verdict` | Meaning |
|---|---|
| `success` | Every job finished, none failed, and at least one actually ran. |
| `failure` | Every job finished, and at least one failed or was cancelled. |
| `failing` | A job has already failed; others are still going. |
| `pending` | Not finished, nothing failed so far. |
| `nothing_ran` | Every job was skipped. Not a pass. |
| `no_runs` | No CI run for this commit (yet). |

The verdict comes from every run for the commit, not from whichever job
finished last. A job no runner has taken yet still counts as unfinished. On
Forgejo before 16, which has no jobs endpoint, jobs come from the commit's
statuses, corrected by the runner tasks that took them.

Only jobs that need attention are listed by default: failed, cancelled, or
not finished. The rest are counted in `jobs_omitted` ("26 success, 24
skipped"). Pass `jobs: "all"` to list every job, or `jobs: "none"` for the
verdict, summary and progress only. On a 50-job workflow that passed, that
cuts the reply from about 7,300 characters to about 700.

`progress` says where CI has got to: jobs finished, which are running and at
which step, where a failed job failed, and what hasn't started. Steps are
listed for jobs that are running or failed, where the forge's jobs endpoint
reports them: Gitea, and Forgejo 17 and later.


For a run that hasn't finished, `estimates` says when it will likely end. [Estimates](estimates.md) explains how, and how good they are. CI outside the forge (Woodpecker, Jenkins and anything that posts commit statuses) is read too: see [Commit statuses](../ci/statuses.md).
