# Estimates

How `ci_status`, `ci_wait` and `ci_estimate` predict when CI will finish, from the workflow's own history, and how often they are right.

For a run that hasn't finished, `estimates` says when it will likely end,
from the recent runs of the same workflow, for the same event, on the same
branch:

- **If it passes:** the median and 90th-percentile duration of up to 20
  recent passing runs with the same jobs, and the clock times those put the
  end at.
- **If it fails:** the median and 90th-percentile time from start to the
  first failed job of up to 10 recent failed runs. That's when a failure
  usually shows, and when a fail-fast `ci_wait` would return. Once a run is
  past the point where 9 in 10 past failures had shown, it says the run is
  likely to pass.
- **If a job waits for a runner** and none with its labels is free, it
  says whether they're all busy or none is online. When they're busy it
  names the repositories whose jobs are running on them, and how many other
  jobs wait for them and from where: "lint waits for a runner: all 3 with
  labels docker are busy, running acme/infra 2, acme/site, and 4 other jobs
  wait for them (acme/widgets 3, acme/site)". That needs the access
  `ci_runners` needs, and other repositories show only where it can list
  their jobs (see [ci_runners](ci-runners.md)). A job a free runner will
  take, or one for runners the forge hosts, gets nothing.
- **If a run has gone on longer than 9 in 10 of its passes**, it says what
  else its runners are doing. The forges don't report how loaded a
  runner's machine is, so this is the evidence they do give, from
  strongest to weakest: its runners are taking several jobs at once
  ("likely slowed by its runners taking several jobs at once"); other
  repositories' jobs are running on the same runners, which slows it only
  if they share a machine; or, where every job on the instance could be
  seen, its runners run nothing else, so the time is going on the run
  itself. With none of these to go on it says nothing.

The progress line carries the same forecast, for example:

```
3 of 9 jobs finished (3 success); running: test (step 4 of 7: cargo test); waiting: deploy;
12m in; a pass usually takes 20m (28m if slow): around 10:42, or 10:50 if slow;
a failure usually shows by 10:43 (10:54 if slow)
```

**Job sets.** Path filters and `if:` conditions make one workflow run very
different sets of jobs, and a docs-only push that skips the build shouldn't
be timed by runs that built. So passing runs are grouped by the jobs that
ran, skipped ones left out. A running run's set isn't fully known until its
jobs are decided: a job waiting on others may still be skipped. It is
matched with the past runs whose set agrees with what is known so far:
every job that has run, is running or is queued is in it, and no job
already skipped is. At the start that's usually the whole workflow, and it
narrows as jobs are decided. Where fewer than 3 past runs agree, the whole
workflow is used; `matched_job_set` and `basis` say which. Failures aren't
grouped: a failure skips the jobs after it, so a failed run's set says
little about the next one.

It needs at least 3 past runs of each kind. When runs vary a lot (the 90th
percentile is over twice the median) it says to treat the forecast as rough.

**History** is kept on disk, beside the probe cache (`history/`, one JSON
file per repository: up to 150 runs for each of 30 workflow, event and
branch combinations). It's refreshed at most every 10 minutes, reading only
the runs newer than the last read, so a quiet workflow costs one request. A
finished run's jobs are read once, 20 runs per call (5 at a time), so the
first estimate for a workflow with a long history fills in over a few
calls. Woodpecker and Jenkins runs get no estimate: their history isn't in
the forge's run list, and reading it from each system is left for later.

## Accuracy and limits

Estimates describe the workflow's recent history. They can change when jobs, runner capacity or caches change. A 90th percentile is an observed historical duration. Future runs can take longer, and the first failure can occur in a different job.

The `backtest` developer command evaluates each historical run using only earlier runs. Public benchmark scripts and fixtures cover CI verdicts; they do not provide a reproducible accuracy study for duration forecasts. Treat forecasts as guidance when deciding whether to wait.

If the checkout's `HEAD` has no runs, a note says why when the forge's copy
of the branch is elsewhere: `HEAD` is behind it (with CI on the forge's
head), ahead of it and not pushed, or diverged from it.
