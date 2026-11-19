# ci_estimate

How long CI will likely take on a branch, before you push.

## Parameters

| Parameter | Meaning |
|---|---|
| `branch` | The branch to estimate for. Default: the checkout's branch. |
| `event` | `push` (default), `pull_request`, and so on. |
| `workflow` | One workflow file (`ci.yml`). Default: every workflow that ran on the branch's head. |

How long CI will likely take, before you push: *"how long will CI take on
this branch?"*. For each workflow that ran on the branch's head on the
forge (and on the default branch's), it gives the median and 90th
percentile of recent passing runs, when a failure usually shows, and the
same spread for each common job set, since a change that skips the slow
jobs finishes sooner:

```
a pass usually takes 9m (14m if slow); a failure usually shows after 4m (8m if slow);
by job set: 12 jobs (31 runs): 11m (14m if slow), 5 jobs (14 runs): 4m (5m if slow)
```

It reads `push` runs of the checkout's branch by default; pass `branch`,
`event` (`pull_request`, say) or `workflow` to choose. A branch with fewer
than three runs of its own uses the default branch's history. It doesn't
predict which job set a change will run from the files it touches: that is
decided by path filters and `if:` conditions the forge evaluates, often in a
job of their own, so it reports the spread across job sets instead.


[Estimates](estimates.md) explains the method and how often it is right.
