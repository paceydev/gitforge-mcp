# What works on which forge

The minimum Gitea, Forgejo and GitHub versions for each capability, checked against each forge's own API description.

These are the minimum versions that have the API each capability needs. The
contract tests check this table against each server's own API description.

| Capability | Gitea | Forgejo | GitHub |
|---|---|---|---|
| CI runs: list and read | 1.25 | 12 | ✓ |
| CI jobs per run | 1.25 | 16 | ✓ (each job's latest attempt) |
| Job logs | 1.25 | 16 (with ranged reads for a cheap tail) | ✓ (once the job has finished) |
| Cancel a run | 28 | 16 | ✓ |
| Rerun a run, or its failed jobs | 1.26 | 17 (failed jobs one at a time) | ✓ |
| Dispatch a workflow | 1.24 | 9 | ✓ (names the run it started on github.com and Enterprise Server 3.21+) |
| Runner availability (repo admin) | 1.24 | 15 | ✓ (self-hosted runners; GitHub-hosted ones aren't listed) |
| Job timings for estimates | 1.25 | 9 (approximate before 17) | ✓ |
| CI outside the forge, from commit statuses | ✓ | ✓ | ✓ (and check runs) |
| Pull requests | ✓ | ✓ | ✓ |
| Merge once checks pass | ✓ | ✓ | ✓ (if the repository allows auto-merge) |
| Repository settings, branch protection, webhooks (repo admin) | ✓ | ✓ | ✓ (classic branch protection) |
| Webhook delivery history | — | — | ✓ |

For Woodpecker, everything above that it has (jobs with steps, logs,
cancel, rerun of a whole pipeline, dispatch, agents) needs 3.0 or later; the
contract tests run 3.20 beside Forgejo 17 and check every endpoint used
against Woodpecker's own API description.

For Jenkins, everything uses endpoints in core since 1.x (`api/json`,
`consoleText`, `whoAmI`, `computer`, `queue`, `crumbIssuer`, a build's
`stop`, a job's `build` and `buildWithParameters`) and, for stages,
Pipeline: Stage View's `wfapi`. The contract tests run Jenkins 2.580.2 LTS
beside Forgejo 17, with no plugins, and check every write against
Jenkins's own account of it afterwards.

On GitHub, ✓ means github.com and GitHub Enterprise Server 3.14 or later,
the oldest release whose published API description has everything the
driver uses. Enterprise Server needs `kind = "github"` in the
[configuration](../guide/configuration.md).

GitHub differs from Gitea and Forgejo in a few ways an agent may notice:

- **A run is named by its id**, the number in its URL. GitHub's own run
  number counts per workflow, so it repeats; `run=` takes the id.
- **A rerun is the same run, on its next attempt.** `ci_status` shows the
  attempt, and each job's latest attempt: a job that failed and passed on a
  rerun counts as passed.
- **A run from a first-time contributor's fork waits for approval.** That
  reads as pending (blocked), not failed.
- **CI outside Actions** reports through check runs as well as commit
  statuses; each app is one run. Actions' own check runs are its jobs and
  aren't counted twice.
- **Merging once checks pass** uses GitHub's auto-merge, which waits for
  what branch protection and rulesets require. GitHub can't delete the
  branch afterwards unless the repository's *Automatically delete head
  branches* setting is on, so `merge_when_ready` refuses `delete_branch`
  otherwise. Merge styles are `merge`, `squash` and `rebase`.
- **Rate limits** are reported with the time they lift.
