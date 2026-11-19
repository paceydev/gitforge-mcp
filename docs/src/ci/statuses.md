# Commit statuses

How gitforge reads CI that runs outside the forge, from the commit statuses it posts.

Woodpecker, Jenkins, Drone and other CI systems report to the forge through
commit statuses. gitforge reads those too: each system becomes a run named
after its status context (`ci/woodpecker/push`), each status a job, with a
link to its page on that system. `ci_status`, `ci_wait` and the CI check in
`pull_request merge` work as usual, and also in a repository with Actions
turned off. Logs, cancel, rerun and dispatch stay with that system:
`ci_logs` gives the links, and `ci_control` says where to go. A repository
counts as having CI, for merging, when its base branch has such statuses.
