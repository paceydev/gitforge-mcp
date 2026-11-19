# Safety and permissions

What the server will change, what it refuses, and how it keeps your credentials out of sight.

An agent with forge access can merge the wrong commit, merge before CI has
run, or cancel someone else's build. gitforge-mcp is built so that the safe
call is the easy one, and so that a refusal always says why.

## At a glance

| Risk | What stops it |
|---|---|
| A tool that can't work on this forge | It isn't offered. Each tool is gated on the forge's kind and version and your permissions, and a tool missing from the gate table is never offered. |
| An action your access doesn't allow | It isn't in the tool's schema, and a call naming it is refused with the forge's reason. |
| Any change at all, when you don't want one | `--read-only`: `ci_control` disappears, `pull_request` offers only `list`, `get` and `reviews`, and `repo_settings` only `get` and `plan`, whatever the token allows. |
| A settings change other than the one reviewed | `repo_settings` `apply` takes the fingerprint `plan` returned and refuses if the section changed since; `revert` requires a post-apply fingerprint and refuses if the section changed after the apply. |
| A webhook secret in the conversation | Secrets are write-only: no forge returns one, and every `repo_settings` reply, errors from the forge included, has any secret from the request scrubbed. |
| Merging commits nobody reviewed | `merge` and `merge_when_ready` take `expected_head_sha` and refuse if the branch has moved. The forge checks it too. |
| Merging before CI passed | `merge` refuses unless CI on the head commit is `success`. `nothing_ran` (every job skipped) doesn't count. Turning this off takes `require_ci: false`. |
| Scheduling a merge that wouldn't wait | `merge_when_ready` checks the base branch’s required status checks when scheduling and refuses to schedule if there are none. |
| A write sent twice | Writes are never retried once their request has gone. A timeout is reported, not repeated. |
| A Jenkins parameter dropped without a word | `dispatch` refuses a parameter the job doesn't define, before sending anything. `rerun` refuses a build with a password or file parameter it can't repeat. |
| A token in a log, an error or a reply | Credentials live in a type that won't print itself, and the tests check tool output for them. |
| A token sent to the wrong host | A token goes only to the host it was given for: a forge token to the forge, a Woodpecker token to that Woodpecker. |
| A token on a command line | The server finds credentials itself. The agent never sees a token or a command carrying one. |
| A token in a remote URL | The URL is shown with its credentials removed. |

## Only what will work is offered

The server probes the forge when it starts: its kind and version, who the
credential belongs to, and what that user may do in the repository. Each
tool and each action is then checked against the [capability
table](../reference/forges.md). What isn't available isn't listed, and
[`forge_context`](../tools/forge-context.md) says why, in words such as
*"cancel needs Forgejo 16 or later; this server runs 15.0.11"*.

Capabilities come from the forge's version, not from calling an endpoint
and reading a 404. Tools are marked for clients: `readOnlyHint` when none of
the actions offered writes, and `destructiveHint` when one does.

## Writes

Every write is gated on a capability, refused in read-only mode, and
covered by contract tests against each forge version in the test matrix.

| Write | Needs |
|---|---|
| `pull_request` `create`, `merge` | write access to the repository |
| `pull_request` `merge_when_ready`, `cancel_auto_merge` | write access, and the forge's auto-merge (on GitHub, the repository must allow it) |
| `ci_control` `cancel`, `rerun`, `rerun_failed`, `dispatch` | write access, and a forge version with that API (see [`ci_control`](../tools/ci-control.md)) |

On Jenkins, which doesn't say what a user may do, writes are offered to
any signed-in user, and a refusal names the permission most likely
missing.

Each write request is sent once. If it times out, the reply says so and
the agent can look at what happened (`ci_status`, `pull_request get`)
before deciding what to do next.

## Merging

`merge` refuses when:

- the head isn't `expected_head_sha` any more;
- the pull request conflicts with its base;
- CI on its head commit hasn't passed, unless `require_ci: false`.

A repository with no CI at all (Actions off, or never a run) isn't held to
the CI check, and the reply says so. One case slips through: a pull
request that adds the repository's first workflow, merged before that
workflow has run.

`merge_when_ready` refuses a moved head, a conflict or a closed pull
request in the same way. It can merge immediately if CI on the head has
already passed or the repository has never run CI. Once it schedules a
merge, the forge merges
whatever the head is when the checks pass, so commits pushed in the
meantime are merged too, once their checks pass. On GitHub it refuses
`delete_branch` unless the repository deletes head branches itself,
because GitHub can't do it afterwards.

## Credentials

- Tokens come from an environment variable, the OS keyring, the GitHub CLI
  or git's credential helper, at the moment they're needed. See
  [Credentials](credentials.md).
- The server never writes a token to disk. The probe cache holds the
  forge's kind and version, your login and your permissions only.
- git's credential helper is asked non-interactively: no prompts, no
  askpass or credential-manager windows.
- `forge_context`, `gitforge-mcp config` and `gitforge-mcp auth status` say
  where a credential came from, never what it is.
- Give a token the narrowest scopes that cover what you need:
  `read:repository` on Gitea and Forgejo is enough for everything
  read-only. Or run with `--read-only` and rule writes out whatever the
  token allows.

## Reporting a vulnerability

Please report security issues privately, as `SECURITY.md` in the
repository describes, not in a public issue.
