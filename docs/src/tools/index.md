# The tools

What each tool answers, when it is offered, and the conventions they share.

Ask your agent about the repository: *"did my push pass?"*, *"why did CI
fail?"*, *"is the runner busy?"*, *"is PR 12 ready to merge?"*. It picks the
tool. You don't need to know the tools to use them; this is for reading
what the agent did, or for calling them yourself with
[`gitforge-mcp call`](../guide/command-line.md#gitforge-mcp-call).

## Tools

| Tool | Answers | Offered when | Changes anything |
|---|---|---|---|
| [`forge_context`](forge-context.md) | Which repository and forge this is, who you are, and why anything is missing | always | no |
| [`ci_status`](ci-status.md) | Did CI pass on this commit, and where has it got to? | CI runs or commit statuses can be read | no |
| [`ci_logs`](ci-logs.md) | Why did it fail? | job logs can be read | no |
| [`ci_runners`](ci-runners.md) | Is a runner free, and what is waiting? | runners can be listed (repository admins) | no |
| [`ci_wait`](ci-wait.md) | Tell me when CI finishes | CI runs or commit statuses can be read | no |
| [`ci_estimate`](ci-estimate.md) | How long will CI take? | CI runs can be read | no |
| [`pull_request`](pull-request.md) | List, read, open and merge pull requests | pull requests can be read | `create`, `merge`, `merge_when_ready`, `cancel_auto_merge` |
| [`ci_control`](ci-control.md) | Cancel, rerun or dispatch CI | the forge can do one of them, with write access | always |
| [`repo_settings`](repo-settings.md) | Show and change merge options, branch protection and webhooks | you are an admin of the repository | `apply`, `revert`, `test_webhook` |

A tool that can't work on this forge, at its version, with your access,
isn't listed at all. [`forge_context`](forge-context.md) says what is
missing and why. With `--read-only`, nothing that changes the forge is
listed. See [Safety and permissions](../guide/safety.md).

## Choosing a commit

The CI tools look at the checkout's `HEAD` unless told otherwise. Give one
of:

| Parameter | Meaning |
|---|---|
| `sha` | A commit sha, full or abbreviated. |
| `branch` | A branch on the forge: its latest commit, which may not be what you have checked out. |
| `run` | A run number, as in the forge's run URLs. On GitHub, the run id. Where the forge runs no CI of its own, a Woodpecker pipeline number. |

If `HEAD` has no runs, the reply says why when the forge's copy of the
branch is elsewhere: `HEAD` is behind it, ahead and not pushed, or
diverged.

## Choosing what to list

`ci_status` and `ci_wait` list only the jobs that need attention by
default: failed, cancelled or not finished. The rest are counted in
`jobs_omitted` (`"26 success, 24 skipped"`). `jobs: "all"` lists every job,
and `jobs: "none"` gives the verdict, summary and progress only. On a
50-job workflow that passed, the default cuts the reply from about 7,300
characters to about 700.

## Verdicts

Every CI tool answers with the same verdicts:

| `verdict` | Meaning |
|---|---|
| `success` | Every job finished, none failed, and at least one actually ran. |
| `failure` | Every job finished, and at least one failed or was cancelled. |
| `failing` | A job has already failed; others are still going. |
| `pending` | Not finished, nothing failed so far. |
| `nothing_ran` | Every job was skipped. Not a pass. |
| `no_runs` | No CI run for this commit (yet). |

## Replies

Every tool replies with JSON. Errors are plain sentences that say what to
do, such as which version a capability needs, or which scope a token is
missing.
