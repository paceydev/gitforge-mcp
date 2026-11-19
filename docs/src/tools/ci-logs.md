# ci_logs

The end of each failed job's log, trimmed and searchable.

## Parameters

| Parameter | Meaning |
|---|---|
| `sha`, `branch`, `run` | Which commit or run, as for [`ci_status`](ci-status.md). Default: the checkout's `HEAD`. |
| `job` | Only jobs whose name contains this text (case-insensitive). |
| `all` | Include jobs that didn't fail. Default: only failed and cancelled jobs. |
| `tail` | Lines from the end of each log. Default 80; `0` reads the whole log. |
| `grep` | A regular expression: keep only matching lines, before `tail` applies. Case-insensitive. |
| `timestamps` | Keep the runner's timestamp on each line. |

The end of each failed job's log, without the timestamp on every line:
80 lines by default. Pick jobs with `job` (part of the name) or `all`, keep
matching lines with `grep` (a regular expression), or read a whole log with
`tail: 0`. Output is capped at about 60,000 characters per job, keeping the
end. Forgejo 16 and later serve just the end of a log, so a tail is cheap
there.
