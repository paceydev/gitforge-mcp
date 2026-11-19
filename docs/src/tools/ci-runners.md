# ci_runners

Every runner the credential can see, idle, busy or offline; the jobs waiting for one; and which repositories' jobs are holding them.

Every runner the credential can see (registered to the repository, its
owner, or the whole instance) as idle, busy or offline with its labels.
Also the jobs waiting for a runner, and how many visible runners carry
each job's labels. Scopes the credential can't read are listed with the
forge's reason, so an empty list is never mistaken for "no runners".
Offered to repository admins.

It takes no parameters. For Woodpecker and Jenkins it lists their agents and nodes too, under `ci_systems`: see [Woodpecker](../ci/woodpecker.md) and [Jenkins](../ci/jenkins.md).

## Who is holding the runners

When runners are shared between projects, "all busy" isn't the whole
answer: you want to know whose jobs they're busy with. So `ci_runners` also
lists the jobs running and waiting in every scope it can (`job_scopes`: the
repository, its owner, the instance), and groups them into `pools`. A pool is
the runners carrying one set of labels, with the jobs that asked for that
set:

```json
{
  "labels": ["docker"],
  "runners_idle": 0, "runners_busy": 3, "runners_offline": 0,
  "running": 5, "waiting": 4,
  "repos": [
    { "repo": "acme/infra", "running": 3, "waiting": 0 },
    { "repo": "acme/widgets", "running": 2, "waiting": 3 },
    { "repo": "acme/site", "running": 0, "waiting": 1 }
  ],
  "oldest_wait_seconds": 840,
  "doubled_up": true,
  "summary": "docker: 3 of 3 runners busy, 5 jobs running (acme/infra 3, acme/widgets 2), 4 waiting (acme/widgets 3, acme/site), the oldest for 14m - 5 jobs on 3 runners, so some runners are taking several at once"
}
```

The top-level `summary` names the pools with jobs waiting for busy runners,
or runners taking several jobs at once.

A job counts in the pool of exactly the labels it asked for. Runners
carrying several of those sets are counted in each pool, so pools can
overlap.

**`doubled_up`** means some runners are running several of the pool's jobs
at once: a runner with a capacity above one, or several runners on one
machine. It's the only sign of an overloaded machine the forges give; none
of them reports a runner's CPU, memory or load. On Gitea, GitHub and
Woodpecker each running job names its runner, so it's counted. On Forgejo it
shows as more jobs running than busy runners, and is claimed only when every
runner scope could be read: a runner the credential can't see could be the
one running the extra jobs.

What each forge gives:

| | Jobs from other repositories | Which runner runs a job | When a job was queued |
|---|---|---|---|
| Forgejo 11+ | owner and instance scopes | no | no |
| Gitea 1.25+ | owner and instance scopes | yes | yes |
| GitHub | no: jobs are listed only per repository | yes | yes |
| Woodpecker | the whole server's queue | yes | no |
| Jenkins | not yet | not yet | not yet |

Forgejo names a job's repository only by its id before Forgejo 17; each one
is looked up once, and one the credential can't read shows as
`repository #<id>`.

**What you need to see other repositories' jobs.** The owner's jobs need the
same access as the owner's runners: `read:user` for your own repositories,
an organisation admin for an organisation's. The instance's jobs need a site
administrator's token with `read:admin`. Without them, pools hold only this
repository's jobs; the `job_scopes` entries say which scopes were refused and
why. See [Credentials](../guide/credentials.md).

`ci_status` and `ci_wait` use the same pools: a job waiting for a runner
says whose jobs hold it, and a run that's gone on longer than usual says
what else its runners are running. See [Estimates](estimates.md).
