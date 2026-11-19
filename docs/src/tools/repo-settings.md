# repo_settings

Show and change a repository's settings, merge options, branch protection and webhooks, with a plan to review first and a revert afterwards.

## Sections

A repository's settings, for its admins, in four sections under names
every forge shares:

| Section | Fields |
|---|---|
| `general` | `description`, `website`, `default_branch`, `private`, `has_issues`, `has_wiki` |
| `merge` | `allowed_styles`, `delete_branch_after_merge`; `default_style` (Gitea, Forgejo); `allow_auto_merge` (GitHub) |
| `branch_protection` | rules by `pattern`: `require_pull_request`, `required_approvals`, `dismiss_stale_approvals`, `required_checks`, `require_up_to_date`, `require_signed_commits`, `push_allowlist`; `block_on_rejected_reviews` (Gitea, Forgejo); `enforce_admins` (GitHub) |
| `webhooks` | by `id`: `url`, `content_type`, `events`, `active`, and the write-only `secret`; `type`, `branch_filter` and the write-only `authorization_header` (Gitea, Forgejo) |

## Actions

| Action | What it does | Needs |
|---|---|---|
| `get` *(default)* | The settings now: one `section`, or all four | admin |
| `plan` | Each change `changes` would make, from what to what, and a fingerprint of the section | admin |
| `apply` | Makes the planned change, refusing if the section changed since the plan; returns a `revert` when read-back succeeds | admin, write |
| `revert` | Undoes an apply when its post-apply fingerprint matches; returns a `revert` when read-back succeeds | admin, write |
| `test_webhook` | Has the forge send a webhook a test delivery (a ping on GitHub) | admin, write |
| `webhook_deliveries` | A webhook's recent deliveries and how the receiver answered | admin; GitHub only |

`get` and `plan` read; with `--read-only` they are all that's offered.

## A change, step by step

A change goes in three steps, so what the user approves is what happens:

1. **`plan`** with a `section` and `changes` lists each change, from what
   to what, and returns a fingerprint of the section as it is. Fields
   already as asked are left out, so applying twice changes nothing.
2. **`apply`** with the same `section`, `changes` and the `fingerprint`
   makes the change. It reads the section again first and refuses if it
   changed since the plan. It returns a `revert` object when read-back succeeds.
3. **`revert`** with that object undoes the change, refusing if the section
   changed after the apply, since a revert would undo that too. It returns
   a `revert` of its own when read-back succeeds, to redo.

If read-back fails, automatic revert is unavailable. For a partial apply,
check `changes` and `not_done` in the reply. Use `get` to inspect the
current settings, compare them with the original plan and the reported
operations, then plan and review a corrective change before applying it.

```json
{"action": "plan", "section": "branch_protection",
 "changes": {"set": [{"pattern": "main", "required_approvals": 2, "required_checks": ["ci / test"]}],
             "delete": ["release/*"]}}
```

`changes` for `general` and `merge` is the fields to set. For
`branch_protection` it's `{"set": [rules], "delete": [patterns]}`, where a
rule with a new pattern is created. For `webhooks` it's `{"create":
[webhooks], "update": [{"id": ..., fields}], "delete": [ids]}`; a new
webhook sends JSON on push, active, unless told otherwise.

A revert puts back the forge's own record of each rule and webhook, not
just the fields above, so a deleted rule comes back with its file
patterns, approval and merge allowlists and the rest. Two things can't
come back: **a webhook's secret**, which no forge returns once set, and **a
deleted webhook's id**, which the forge assigns anew. No reply ever shows a
secret, not even an error from the forge that quotes it.

## Parameters

| Parameter | For | Meaning |
|---|---|---|
| `action` | all | One of the actions above. Default `get`. |
| `section` | `get`, `plan`, `apply` | `general`, `merge`, `branch_protection` or `webhooks`. `get` without it reads every section. |
| `changes` | `plan`, `apply` | What should change, as above. |
| `fingerprint` | `apply` | The fingerprint `plan` returned. |
| `revert` | `revert` | The `revert` object `apply` returned, as it was. |
| `webhook_id` | `test_webhook`, `webhook_deliveries` | The webhook's id. |
| `limit` | `webhook_deliveries` | How many (default 10, at most 50). |

## On each forge

On Gitea and Forgejo, a `push_allowlist` is who may push directly, so it
can't go with `require_pull_request`, which turns direct pushes off. On
GitHub it's who may push at all, and a rule's `pattern` must be the name of
an existing branch: classic branch protection has no wildcards, and
rulesets aren't supported. Gitea and Forgejo don't list a webhook's
deliveries in their API.

A scope a token lacks can't be seen in advance, so a refusal says which
scope or permission settings need. See [Credentials](../guide/credentials.md).
Each request is sent once and never retried. See [Safety and
permissions](../guide/safety.md).
