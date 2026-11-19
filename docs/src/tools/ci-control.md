# ci_control

Cancel, rerun or dispatch CI runs where the forge or CI provider supports the action and grants permission.

## Actions

The version columns show the minimum supported release.

| Action | What it does | GitHub | Gitea | Forgejo | Woodpecker | Jenkins |
|---|---|---|---|---|---|---|
| `cancel` | Stops an unfinished run | GitHub.com or GHES 3.14 | 28 | 16 | 3.0 | Core (Job/Cancel) |
| `rerun` | Runs the jobs again | GitHub.com or GHES 3.14 | 1.26 | 17 | 3.0 | Core (Job/Build) |
| `rerun_failed` | Runs failed and cancelled jobs again | GitHub.com or GHES 3.14 | 1.26 | 17 (one job at a time) | Unsupported | Unsupported |
| `dispatch` | Starts a new run | GitHub.com or GHES 3.14 | 1.24 | 9 | 3.0 | Core (Job/Build) |

GitHub, Gitea and Forgejo require repository push access. GitHub fine-grained
tokens need Actions write. Gitea and Forgejo tokens need `write:repository`.
Woodpecker requires a token with push access to
the repository. Jenkins requires a signed-in user and checks the listed
permissions when each request is sent. Jenkins reruns build the jobs again
using their original parameters and the branch’s current revision.

Name the run with `run` (its number), or with a `sha` or `branch` that has
exactly one run. The default is the checkout’s `HEAD`. See
[Woodpecker](../ci/woodpecker.md) and [Jenkins](../ci/jenkins.md) for
provider-specific behavior.

## Parameters

| Parameter | For | Meaning |
|---|---|---|
| `action` | all | `cancel`, `rerun`, `rerun_failed` or `dispatch`. There is no default. |
| `sha`, `branch`, `run` | `cancel`, `rerun`, `rerun_failed` | Which run: a run number, or a commit or branch with exactly one run. Default: the checkout's `HEAD`. |
| `workflow` | `dispatch` | The workflow's file name, such as `deploy.yml`. On Jenkins, the job, as `folder/job`. |
| `ref` | `dispatch` | The branch or tag to run it on. Default: the repository's default branch. |
| `inputs` | `dispatch` | The workflow's inputs. On Woodpecker, variables for the manual pipeline; on Jenkins, the job's build parameters. |
| `ci` | `dispatch` | Where CI runs both on the forge and outside it: `forge`, or the system outside it (`woodpecker`, `jenkins`). |

Each request is sent once. A write is never retried after its request has gone, so a timeout never cancels or dispatches twice. See [Safety and permissions](../guide/safety.md).
