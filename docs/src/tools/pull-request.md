# pull_request

List, read, open and merge pull requests, and see where their reviews stand.

## Actions

| Action | What it does | Needs |
|---|---|---|
| `list` *(default)* | Lists pull requests, filtered by `state` (`open`, `closed`, `all`) and `limit` (default 20, at most 50) | read |
| `get` | One pull request in full, with the CI verdict of its head and where its reviews stand | read |
| `reviews` | Its reviews and their comments on lines of the diff | read |
| `create` | Opens a pull request from `head` into `base` | write |
| `merge` | Merges one, if its head hasn't moved, it doesn't conflict and its CI passed | write |
| `merge_when_ready` | Merges immediately when head CI passed or the repository has never run CI, or asks the forge to merge when required checks pass | write, and the forge's auto-merge |
| `cancel_auto_merge` | Cancels a scheduled merge | write, and the forge's auto-merge |

Only the actions your forge and access allow are offered. With `--read-only`, only the read actions are.

## Parameters

| Parameter | For | Meaning |
|---|---|---|
| `action` | all | One of the actions above. Default `list`. |
| `number` | all but `list` and `create` | The pull request's number. |
| `state` | `list` | `open` (default), `closed` or `all`. |
| `limit` | `list` | How many to return (default 20, at most 50). |
| `head` | `create` | The branch with the changes, already pushed. Default: the checkout's current branch. |
| `base` | `create` | The branch to merge into. Default: the repository's default branch. |
| `title`, `body` | `create` | The title and description. |
| `style` | `merge`, `merge_when_ready` | `merge` (default), `rebase`, `rebase-merge`, `squash` or `fast-forward-only`, as the repository allows. On GitHub: `merge`, `squash`, `rebase`. |
| `expected_head_sha` | `merge`, `merge_when_ready` | The head commit you reviewed, from `get`. Refused if the branch has moved since. Strongly recommended. |
| `delete_branch` | `merge`, `merge_when_ready` | Delete the head branch afterwards. Default false. |
| `require_ci` | `merge` | Refuse unless CI on the head passed. Default true. |
| `include_resolved` | `reviews` | Also list dismissed reviews, resolved comments and reviews with nothing to read. |

## How each action behaves

`action: "list"` (the default) lists pull requests, filtered by `state`
(`open`, `closed`, `all`). `action: "get"` with `number` returns one in full:
branches, head commit, whether it can merge cleanly, labels, the
description, the CI verdict of its head commit, and where its reviews
stand (`approved_by`, `changes_requested_by`, `review_requested`, and
`open_comments`, the line comments not yet resolved).

`action: "reviews"` with `number` lists its reviews: author, state
(`approved`, `request_changes`, `comment`), summary comment, when it was
submitted, and whether it is `stale` (given on an older head) or
`dismissed`. Each carries its comments on lines of the diff: `path`,
`line` (with `old_side: true` for a removed line), author, body, the last
few lines of the diff hunk, and `resolved_by` once resolved. By default it
lists only what is still open, and counts the rest in `omitted`: dismissed
reviews, resolved comments, and reviews with nothing to read, such as an
approval without a comment. `include_resolved: true` lists them too.
Replying to or resolving comments isn't supported yet.

With write access, more actions:

- `action: "create"` opens a pull request from `head` (default: the
  checkout's current branch, already pushed) into `base` (default: the
  repository's default branch), with a `title` and `body`.
- `action: "merge"` merges one, in a `style` the repository allows (`merge`,
  `rebase`, `rebase-merge`, `squash`, `fast-forward-only`). It refuses if:
  - the head isn't `expected_head_sha` any more: pass the head you reviewed,
    so new commits can't slip in (the forge checks this too);
  - it conflicts with its base;
  - CI on its head commit hasn't passed (`success`; `nothing_ran` doesn't
    count). `require_ci: false` overrides this, for when you've decided to
    merge anyway. A repository with no CI at all (Actions off, or it has
    never had a run) isn't held to this, and the reply says so. One
    exception slips through: a pull request that adds the repository's
    first workflow, merged before that workflow has run.

  `delete_branch: true` deletes the head branch after merging, and
  `branch_deleted` in the reply says whether it's gone. The reply also
  includes `merge_commit_sha`, the commit the merge put on the base branch. Wait on that (`sha`) to follow CI after a merge: the branch
  can move on if someone else pushes in the meantime.
- `action: "merge_when_ready"` merges one once its CI passes, like the
  forge's "merge when checks succeed". It refuses a moved head, a conflict
  or a closed pull request as `merge` does, but not unfinished CI:
  - CI on its head has already passed, or the repository has never run CI: it
    merges at once, and the reply is `merge`'s, with `scheduled: false`.
  - On the scheduling path, the server checks the base branch’s required
    status checks and refuses to schedule if there are none. The forge
    merges when those checks pass, and may merge immediately if they have
    already passed. The reply has `scheduled: true` while the merge is pending.

  Once scheduled, the forge merges whatever the head is when the checks
  pass, so commits pushed in the meantime are merged too once their checks
  pass. `action: "cancel_auto_merge"` cancels a scheduled merge
  (`cancelled: false` if none was scheduled). Follow the checks with
  `ci_wait`, and see whether it merged with `get`.
