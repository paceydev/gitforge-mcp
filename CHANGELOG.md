# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.1] - 2026-11-20

### Added

- `gitforge-mcp init` guides Claude Code and Codex setup, checks credentials and fresh repository access, previews and confirms client configuration changes, and retains private backups. `--dry-run` previews without writing configuration or storing credentials.
- Crates.io package metadata and a focused source archive for the 0.2.1 release.
- Homebrew installation and upgrade instructions for macOS and Linux, including desktop MCP client paths and switching from a Cargo installation.

### Changed

- Documentation-only pushes and pull requests build documentation and scan credentials, while source/configuration changes, tags and manual runs retain the full CI suite. The required gate checks the expected route.

## [0.2.0] - 2026-11-19

First public release, with native archives for macOS, Linux and Windows on x64 and ARM64, SHA-256 checksums and GitHub build attestations. Platform publisher signing is planned.

### Added

- `ci_runners` names the repositories holding shared runners. It lists the
  jobs running and waiting in the repository, its owner and the instance,
  and groups them into `pools`: the runners carrying each set of labels
  jobs asked for, with each repository's jobs running and waiting, how long
  the oldest has waited, and whether runners are taking several jobs at
  once. Gitea, Forgejo and Woodpecker list other repositories' jobs; GitHub
  lists only the repository's own.
- A job waiting for a runner says whose jobs are running on the runners it
  wants, and how many other jobs wait for them and from which
  repositories.
- A run that has gone on longer than 9 in 10 of its passes says what else
  its runners are running: runners taking several jobs at once, other
  repositories' jobs on the same runners, or, where every job could be
  seen, nothing else.

- A documentation site, built with mdBook from `docs/` by
  `scripts/docs/build.sh`: a getting-started guide, a page per tool with its
  parameters and actions, a safety and permissions page with every guard in
  one table, configuration, the command line, CI outside the forge, and an
  architecture page. The README is now a short front page that links into
  it.
- The site as `llms.txt` (every page with a one-sentence description),
  `llms-full.txt` (every page in one file), and each page's markdown beside
  its HTML, for agents.
- `gitforge-mcp config`: every setting and where it came from (an option,
  an environment variable, the config file, the git remote or a default),
  the token variables it would read and whether each is set, and the
  credential order for each host. Reads no credential and asks the forge
  nothing; `--json` for JSON. Exits 1 if the config file isn't valid or
  there's no repository to bind to.
- `tests/docs.rs` holds the site to the code: every tool, parameter,
  action, command and environment variable documented, every page in the
  table of contents, every link and anchor resolving, and every config
  example valid.
- `repo_settings`, for a repository's admins: its general settings, merge
  options, branch protection rules and webhooks, under names every forge
  shares. `plan` lists each change and a fingerprint of the section;
  `apply` refuses if the section changed since, and returns a `revert` that
  puts back the forge's own record of each rule and webhook, refusing in
  turn if the section changed after the apply. `test_webhook` sends a test
  delivery, and on GitHub `webhook_deliveries` lists recent ones. Webhook
  secrets are write-only and never appear in a reply. Read-only mode keeps
  `get` and `plan`. On Gitea, Forgejo and GitHub (classic branch
  protection).
- `ci_estimate`: how long CI will likely take, before a push. For each
  workflow that ran on the branch's head (or the default branch's), the
  median and 90th-percentile time of recent passing runs, when a failure
  usually shows, and the spread by job set, from the branch's own history
  or the default branch's when it has too little. Offered wherever CI runs
  can be read.
- Estimates in `ci_status` and `ci_wait` match a running run with the past
  runs whose job set (the jobs that ran, skipped ones left out) agrees with
  what is known of its own so far, so path filters and conditional jobs
  no longer mix quick and slow runs. Where fewer than three agree, the
  whole workflow is used, as before. `matched_job_set` says which.
- A job waiting for a runner says why when none is free: every runner with
  its labels is busy (and how many other jobs wait for them), or none is
  online. Shown only where the credential may list runners.
- Run history is kept on disk beside the probe cache (`history/`, one
  small JSON file per repository), and refreshed with only the runs that
  are new since the last read, so estimates survive a restart and cost
  one request when nothing has changed.
- Woodpecker, through its own API (3.0 and later). The repository's
  commit statuses lead to it, or `[ci."<host>"]` in the config names it.
  Its pipelines then read as runs with their numbers, workflows as jobs
  with steps and timings. `ci_logs` reads step logs, `ci_control` cancels,
  restarts a whole pipeline (`rerun`) and starts manual pipelines with
  variables (`dispatch`; one workflow alone from 3.20; `ci` picks the
  system when the forge can dispatch too), and `ci_runners` lists agents
  and waiting tasks for Woodpecker admins. Each feature is gated on the
  caller's access there, read-only mode turns off its writes, and
  `forge_context` reports each system under `ci_systems` and its
  capabilities with `provider` set. A Woodpecker token is looked up like a
  forge token, under the Woodpecker host's name. The contract tests run a
  real Woodpecker 3.20 beside Forgejo 17.
- Jenkins, through its JSON API. Each build a status links to
  reads as a job with its result, timings and, for Pipeline builds on a
  Jenkins with Stage View, its stages as steps; a build waiting for a node
  reads as waiting, one paused for input as blocked, and a stage skipped
  after an earlier failure as skipped rather than failed. `ci_logs` reads
  the build's console, and `ci_runners` lists the nodes and this
  repository's queued builds. `ci_control` stops a run's unfinished
  builds (`cancel`), builds each of its jobs again with the parameters
  its build had (`rerun`; core Jenkins has no rebuild), and starts a
  build (`dispatch`): of the job `workflow` names, or of the repository's
  one project, its branch job for a multibranch project, with `inputs` as
  parameters, refusing one the job doesn't define before anything is
  sent. Each write is a form POST with the CSRF crumb and its session
  cookie, sent once; Jenkins doesn't report a user's permissions, so the
  writes are offered to any signed-in user and a refusal names the
  permission it most likely lacked. The API token goes with its user,
  stored as `user:token` or with `username` in `[ci."<host>"]`. The
  contract tests run Jenkins 2.580.2 LTS beside Forgejo 17.
- `scripts/contract/run-local.sh` runs beside another set of forges with
  `GITFORGE_CONTRACT_NAME` and `GITFORGE_CONTRACT_PORT_OFFSET`.
- GitHub, and GitHub Enterprise Server 3.14 and later, get every CI and
  pull request tool Gitea and Forgejo have: `ci_status`, `ci_wait`,
  `ci_logs`, `ci_runners`, `ci_control` (cancel, rerun, rerun failed,
  dispatch) and `pull_request` (list, get, reviews, create, merge,
  merge_when_ready, cancel_auto_merge). A run is named by its id, as its
  URL is; a rerun shows its attempt and each job's latest attempt; a run
  awaiting a maintainer's approval is pending, not failed; CI outside
  Actions is read from check runs as well as commit statuses. Merging once
  checks pass uses GitHub's auto-merge, through its GraphQL API, which is
  also where review threads' resolution comes from. `forge_context`
  reports the repository's auto-merge setting where GitHub shows it.
- Credentials for GitHub hosts also come from the GitHub CLI
  (`gh auth token`), and for Enterprise Server from `GH_ENTERPRISE_TOKEN`
  and `GITHUB_ENTERPRISE_TOKEN`. A token from git's credential helper is
  sent to GitHub as a token.
- A rate limit is reported as such, with when it lifts, instead of as a
  refused credential. GitHub's validation errors say what was invalid.
- `ci_runners` says which waiting jobs asked for hosted runners, which the
  API doesn't list, rather than counting them as matching no runner.
- `tests/contract_github.rs`: opt-in contract tests against a real
  github.com repository, given a token and a disposable repository.
- CI outside the forge (Woodpecker, Jenkins, Drone…) is read from commit
  statuses: one run per CI system, one job per status, with links. It
  counts in `ci_status`, `ci_wait` and merge's CI check, including in
  repositories with Actions off, where those tools are now offered. Gitea's
  `warning` status counts as a pass.
- `pull_request` `reviews` lists a pull request's reviews and their
  comments on lines of the diff (path, line, author, body, the end of the
  diff hunk), by default only what is still open: no dismissed reviews or
  resolved comments. `get` adds where the reviews stand: who approved, who
  asked for changes, whose review is still requested, and how many line
  comments are open.
- `pull_request` `merge_when_ready` merges a pull request once its CI
  passes: at once if it already has (or the repository has no CI),
  otherwise the forge schedules the merge for when the checks its base
  branch requires pass. It refuses a base that requires none, where the
  forge would merge without waiting. `cancel_auto_merge` cancels a
  scheduled merge. Gitea and Forgejo; a new capability,
  `pull_request_auto_merge`, gates both.
- `pull_request` `merge` reports `branch_deleted` when asked to delete the
  head branch, by checking that it's gone.
- A merge in a repository with no CI at all (Actions off, or never a run)
  isn't refused for want of CI; the reply notes it. A repository that has
  run CI before still waits for CI on the head.
- Estimates for runs that haven't finished, in `ci_status`, `ci_wait` and
  the progress line: when the run will likely end if it passes (median and
  90th percentile of recent passing runs of the same workflow, event and
  branch), and by when a failure usually shows (time to the first failed
  job of recent failed runs). Past the point where 9 in 10 failures had
  shown, it says the run is likely to pass.
- A hidden `backtest` command that measures how well those estimates would
  have done on a workflow's history. Results are in the README.
- `ci_status` and `ci_wait` list only the jobs that need attention (failed,
  cancelled or unfinished) and count the rest in `jobs_omitted`; `jobs:
  "all"` lists every job, `jobs: "none"` none. On a 50-job workflow that
  passed, the reply drops from about 7,300 characters to about 700.
- `pull_request` `merge` returns `merge_commit_sha`, so CI after a merge
  can be followed on that commit rather than on a branch that may move.
- `ci_wait` runs as an MCP task for clients that support tasks, so it no
  longer holds up the conversation there. The task's status message is the
  progress line, and its timeout defaults to 1800 seconds.
- `gitforge-mcp wait`: `ci_wait` from the command line, for running in the
  background. It prints a progress line whenever CI moves on, the result as
  JSON at the end, and exits 0 (passed), 1 (failed), 2 (not finished or no
  runs) or 3 (couldn't check). The server's instructions point agents that
  can run background commands at it, with the binary's full path.
- Job steps, for jobs that are running or failed, where the jobs endpoint
  reports them: Gitea, and Forgejo 17 and later. A `progress` line in
  `ci_status` and `ci_wait` says how many jobs finished, which are running
  and at which step, where failed ones failed, and which haven't started.
  `ci_wait`'s progress notifications carry it too.
- When the checkout's HEAD has no runs, `ci_status`, `ci_logs` and
  `ci_wait` say why if the forge's copy of the branch is elsewhere: HEAD is
  behind it (with CI on the forge's head), ahead of it (not pushed),
  diverged, missing commits not yet fetched, or the branch isn't on the
  forge at all.
- `pull_request` `create` and `merge`. A merge is refused if the head moved
  from `expected_head_sha`, if it conflicts, or (unless `require_ci: false`)
  if CI on its head hasn't passed; the forge is also given the head commit,
  so it refuses a moved branch itself.
- `ci_control`: `cancel`, `rerun`, `rerun_failed` and `dispatch`, each offered
  only on forge versions that have it.
- `ci_wait`: waits for CI to finish, with progress notifications, a timeout,
  fail-fast, and a 90-second limit on waiting for a run to appear.
- Actions are gated one by one: a tool's schema lists only the actions this
  forge version and the token allow, and other actions are refused with the
  reason. `pull_request` is marked read-only when it offers no writes.
- `--read-only` (`GITFORGE_MCP_READ_ONLY`) offers no writes at all.
- Contract tests for every write on every forge version in the matrix.

- `ci_status`: the CI verdict for a commit, branch or run number (default:
  the checkout's HEAD), with every run and job. Waiting jobs count as
  unfinished, all-skipped is `nothing_ran`, and on Forgejo before 16 jobs
  come from commit statuses corrected by runner tasks.
- `ci_logs`: failed jobs' log tails by default, with `job`, `all`, `grep`,
  `tail` and `timestamps`; ranged reads on Forgejo 16 and later.
- `ci_runners`: runners in the repository, owner and instance scopes, each
  scope's outcome, and the jobs waiting for a runner.
- `pull_request`: `list` and `get`, with mergeability and the head commit's
  CI verdict.
- `gitforge-mcp call <tool> [json]` runs one tool from the command line.
- Replay tests of recorded Forgejo 15 API snapshots, and contract tests
  with a real runner and workflow on every forge version in the matrix.

### Fixed

- Windows background-wait instructions use PowerShell invocation and literal quoting. Windows releases statically link the C runtime.
- Upgrade the MCP SDK to 3.9.0 to address RUSTSEC-2026-0462.
- On Gitea, jobs waiting for a runner were read from the oldest 50 jobs of
  the repository, so a repository with a longer history reported none.
  They are now read by status.
- On Gitea 1.26 and later, a dispatched run was reported by the number in
  its page's URL, which recent Gitea names by the run's id, so `ci_wait` on
  it found no such run once another repository had run CI. The number is
  now read from the run.

- Repository settings refuse a revert without a post-apply fingerprint and
  explain how to recover when settings cannot be read back.
