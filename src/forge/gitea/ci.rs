//! CI runs, jobs, logs, runners, and pull requests and their reviews, on
//! Gitea and Forgejo.
//!
//! The two forks shaped their Actions APIs differently:
//!
//! | | Gitea (1.25+) | Forgejo |
//! |---|---|---|
//! | run number | `run_number` | `index_in_repo` |
//! | commit | `head_sha` | `commit_sha` |
//! | state | `status` + `conclusion`, GitHub-style | `status`, its own words |
//! | jobs of a run | `{jobs: [...]}` | `[...]`, from Forgejo 16 |
//! | job times | `started_at`, `completed_at` | per step, from Forgejo 17 |
//! | runners | `{runners: [...]}`, `busy` flag | `[...]`, status `active` = busy |
//!
//! Forgejo before 16 has no jobs endpoint at all, so jobs come from the
//! commit's statuses, corrected by the runner tasks that took them.

use serde::Deserialize;

use super::GiteaFamily;
use crate::ci::{self, CommitStatus, Job, Run, RunsOf, Source, State, Step, Task};
use crate::forge::shapes::{ActionsJobList, ActionsRunList, PrBody, PrUser, RunNumber, nonzero};
use crate::forge::{ApiError, User, list_runners, reaches};
use crate::pulls::{
    self, PullRequest, Review, ReviewComment, ReviewState, StateFilter, StatusChecks,
};
use crate::remote::RepoRef;
use crate::runners::{self, RunnerReport};
use crate::version::{FORGEJO_16, FORGEJO_17, ForgeKind, GITEA_1_25, Version};

/// Logs are fetched whole only up to this size; beyond it, only the end.
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
/// The least of a log kept for a tail, however short the tail.
const MIN_TAIL_LOG_BYTES: usize = 64 * 1024;
/// Jobs report their duration in nanoseconds.
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Items asked for in each page of a list. A shorter page is the last.
const PAGE_SIZE: usize = 50;
/// Pages to search for a run by number on Gitea, which can't filter by it.
const RUN_SEARCH_PAGES: u32 = 20;
/// Pages of commit statuses read for one commit.
const STATUS_PAGES: u32 = 10;
/// Pages of runner tasks read to time Forgejo 16's jobs.
const TASK_PAGES: u32 = 2;

impl GiteaFamily {
    pub(super) fn is_forgejo(&self) -> bool {
        self.server.kind == ForgeKind::Forgejo
    }

    /// True when the server is at least `v`, or its version is unknown
    /// (assumed current, as the capability table does).
    pub(super) fn at_least(&self, release: Version) -> bool {
        self.server.version.is_none_or(|v| v >= release)
    }

    fn has_jobs_endpoint(&self) -> bool {
        if self.is_forgejo() {
            self.at_least(FORGEJO_16)
        } else {
            self.at_least(GITEA_1_25)
        }
    }

    pub(super) fn repo_api(&self, repo: &RepoRef, path: &str) -> String {
        self.api(&format!("/repos/{}/{}{path}", repo.owner, repo.name))
    }

    pub(super) async fn ci_runs_for_commit(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<Vec<Run>, ApiError> {
        let url = self.repo_api(
            repo,
            &format!("/actions/runs?head_sha={sha}&limit={PAGE_SIZE}"),
        );
        let mut runs = self.list_runs(&url).await?;
        // A forge that ignores the filter would return every run.
        runs.retain(|r| r.sha.eq_ignore_ascii_case(sha));
        self.fill_jobs(repo, &mut runs, Some(sha)).await?;
        Ok(runs)
    }

    pub(super) async fn ci_run_by_number(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<Run>, ApiError> {
        let found = if self.is_forgejo() {
            let url = self.repo_api(repo, &format!("/actions/runs?run_number={number}"));
            self.list_runs(&url)
                .await?
                .into_iter()
                .find(|r| r.number == number)
        } else {
            // Gitea has no run-number filter, and its run endpoints take the
            // internal id: passing the number there returns another run.
            let mut found = None;
            for page in 1..=RUN_SEARCH_PAGES {
                let url = self.repo_api(
                    repo,
                    &format!("/actions/runs?limit={PAGE_SIZE}&page={page}"),
                );
                let runs = self.list_runs(&url).await?;
                if runs.is_empty() {
                    break;
                }
                if let Some(run) = runs.into_iter().find(|r| r.number == number) {
                    found = Some(run);
                    break;
                }
            }
            found
        };
        let Some(run) = found else { return Ok(None) };
        let sha = run.sha.clone();
        let mut runs = vec![run];
        self.fill_jobs(repo, &mut runs, Some(&sha)).await?;
        Ok(runs.pop())
    }

    pub(super) async fn ci_recent_runs(
        &self,
        repo: &RepoRef,
        of: RunsOf<'_>,
        pages: u32,
        after: Option<u64>,
    ) -> Result<Vec<Run>, ApiError> {
        let enc = |v: &str| url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
        let mut found = Vec::new();
        for page in 1..=pages {
            // Gitea filters by branch too; Forgejo's branch filter differs
            // between versions, so it's applied here instead, as are the
            // workflow and, for a forge that ignores a filter, the rest.
            let query = if self.is_forgejo() {
                format!("event={}&limit={PAGE_SIZE}&page={page}", enc(of.event))
            } else {
                format!(
                    "event={}&branch={}&limit={PAGE_SIZE}&page={page}",
                    enc(of.event),
                    enc(of.branch)
                )
            };
            let runs = self
                .list_runs(&self.repo_api(repo, &format!("/actions/runs?{query}")))
                .await?;
            let last = runs.len() < PAGE_SIZE || reaches(&runs, after);
            found.extend(runs.into_iter().filter(|r| of.includes(r)));
            if last {
                break;
            }
        }
        Ok(found)
    }

    pub(super) async fn ci_external_runs(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<Vec<Run>, ApiError> {
        match self.statuses(repo, sha).await {
            Ok(statuses) => Ok(ci::external_runs(&statuses, sha)),
            // A commit the forge doesn't have (not pushed yet) has none.
            Err(ApiError::NotFound { .. }) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub(super) async fn ci_has_runs(&self, repo: &RepoRef) -> Result<bool, ApiError> {
        let url = self.repo_api(repo, "/actions/runs?limit=1");
        Ok(!self.list_runs(&url).await?.is_empty())
    }

    pub(super) async fn ci_jobs_of(&self, repo: &RepoRef, run: &Run) -> Result<Vec<Job>, ApiError> {
        let mut runs = vec![run.clone()];
        self.fill_jobs(repo, &mut runs, Some(&run.sha)).await?;
        Ok(runs.pop().map(|r| r.jobs).unwrap_or_default())
    }

    async fn list_runs(&self, url: &str) -> Result<Vec<Run>, ApiError> {
        if self.is_forgejo() {
            let list: ForgejoRunList = self.http.get_json(url).await?;
            Ok(list
                .workflow_runs
                .unwrap_or_default()
                .into_iter()
                .map(ForgejoRun::into_run)
                .collect())
        } else {
            let list: ActionsRunList = self.http.get_json(url).await?;
            Ok(list
                .workflow_runs
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.into_run(RunNumber::RunNumber))
                .collect())
        }
    }

    async fn fill_jobs(
        &self,
        repo: &RepoRef,
        runs: &mut [Run],
        sha: Option<&str>,
    ) -> Result<(), ApiError> {
        if runs.is_empty() {
            return Ok(());
        }
        if self.has_jobs_endpoint() {
            for run in runs.iter_mut() {
                run.jobs = self.jobs_of_run(repo, run.id).await?;
            }
            // Forgejo 16's jobs carry no times; their tasks do.
            if self.is_forgejo() && !self.at_least(FORGEJO_17) {
                let tasks = self.tasks(repo).await?;
                for job in runs.iter_mut().flat_map(|r| r.jobs.iter_mut()) {
                    if let Some(task) = job.task_id.and_then(|id| tasks.iter().find(|t| t.id == id))
                    {
                        ci::apply_task(job, task);
                    }
                }
            }
        } else if let Some(sha) = sha {
            let statuses = self.statuses(repo, sha).await?;
            let tasks = self.tasks(repo).await?;
            for run in runs.iter_mut() {
                run.jobs = ci::jobs_from_statuses(&statuses, run.number);
                ci::overlay_tasks(&mut run.jobs, &tasks, run.number);
            }
        }
        for run in runs.iter_mut() {
            run.add_unlisted_placeholder();
        }
        Ok(())
    }

    async fn jobs_of_run(&self, repo: &RepoRef, run_id: u64) -> Result<Vec<Job>, ApiError> {
        let url = self.repo_api(repo, &format!("/actions/runs/{run_id}/jobs?limit=100"));
        if self.is_forgejo() {
            let jobs: Option<Vec<ForgejoJob>> = self.http.get_json(&url).await?;
            Ok(jobs
                .unwrap_or_default()
                .into_iter()
                .map(ForgejoJob::into_job)
                .collect())
        } else {
            let list: ActionsJobList = self.http.get_json(&url).await?;
            Ok(list
                .jobs
                .unwrap_or_default()
                .into_iter()
                .map(|j| j.into_job())
                .collect())
        }
    }

    /// Every status on `sha`. Each change to a job adds one, so a commit
    /// with several runs can have many; read them all.
    async fn statuses(&self, repo: &RepoRef, sha: &str) -> Result<Vec<CommitStatus>, ApiError> {
        self.http
            .paged(
                STATUS_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/commits/{sha}/statuses?limit={PAGE_SIZE}&page={page}"),
                    )
                },
                |batch: Option<Vec<CommitStatus>>| batch.unwrap_or_default(),
            )
            .await
    }

    /// The most recent runner tasks, newest first.
    async fn tasks(&self, repo: &RepoRef) -> Result<Vec<Task>, ApiError> {
        self.http
            .paged(
                TASK_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/actions/tasks?limit={PAGE_SIZE}&page={page}"),
                    )
                },
                |list: TaskList| list.workflow_runs.unwrap_or_default(),
            )
            .await
    }

    pub(super) async fn ci_branch_head(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<String>, ApiError> {
        let url = self.repo_api(repo, &format!("/branches/{branch}"));
        let body: Option<BranchBody> = self.http.get_json_opt(&url).await?;
        Ok(body.map(|b| b.commit.id))
    }

    /// What the protection on `branch` requires of status checks, read from
    /// the branch itself, which needs only read access (the protection
    /// rules need admin). `None` if there's no such branch.
    pub(super) async fn pulls_status_checks(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<StatusChecks>, ApiError> {
        let url = self.repo_api(repo, &format!("/branches/{branch}"));
        let body: Option<BranchBody> = self.http.get_json_opt(&url).await?;
        Ok(body.map(|b| StatusChecks {
            required: b.enable_status_check,
            contexts: b.status_check_contexts.unwrap_or_default(),
        }))
    }

    pub(super) async fn ci_job_log(
        &self,
        repo: &RepoRef,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        let Some(id) = job.job_id else {
            return Err(ApiError::Unsupported {
                what: format!(
                    "reading the log of {} (no job id on this forge version)",
                    job.name
                ),
            });
        };
        let url = self.repo_api(repo, &format!("/actions/jobs/{id}/logs"));
        // Forgejo 16 serves ranges, so a tail costs only the tail. Gitea
        // sends the whole log, which is cut down after it arrives.
        let range = if self.is_forgejo() { tail_bytes } else { None };
        let max = tail_bytes.map_or(MAX_LOG_BYTES, |n| (n as usize).max(MIN_TAIL_LOG_BYTES));
        match self.http.get_text_tail(&url, range, max).await {
            Err(ApiError::Status { status: 416, .. }) => Ok((String::new(), false)),
            other => other,
        }
    }

    pub(super) async fn ci_runners(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> Result<RunnerReport, ApiError> {
        let (runners, outcomes) = list_runners(
            &self.http,
            |path| self.api(path),
            Self::scope_paths(repo, user, "/actions/runners", "runners"),
            "unavailable on this forge version",
        )
        .await?;
        let active = self.active_jobs(repo, user).await?;
        let waiting = active
            .as_ref()
            .and_then(|a| a.waiting_in(&repo.full_name()));
        Ok(runners::report(
            runners,
            outcomes,
            waiting,
            active.unwrap_or_default(),
            |_| false,
        ))
    }

    pub(super) async fn pulls_list(
        &self,
        repo: &RepoRef,
        state: StateFilter,
        limit: u32,
    ) -> Result<Vec<PullRequest>, ApiError> {
        let url = self.repo_api(
            repo,
            &format!(
                "/pulls?state={}&limit={}&page=1",
                state.as_query(),
                limit.clamp(1, 50)
            ),
        );
        let list: Option<Vec<PrBody>> = self.http.get_json(&url).await?;
        Ok(list
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.into_pull_request(false))
            .collect())
    }

    /// Every review of pull request `number`, oldest first, each with its
    /// line comments. Drafts aren't fetched for comments.
    pub(super) async fn pulls_reviews(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<Review>, ApiError> {
        const PAGES: u32 = 5;
        let mut bodies: Vec<ReviewBody> = Vec::new();
        for page in 1..=PAGES {
            let url = self.repo_api(
                repo,
                &format!("/pulls/{number}/reviews?limit={PAGE_SIZE}&page={page}"),
            );
            let list: Option<Vec<ReviewBody>> = self.http.get_json(&url).await?;
            let list = list.unwrap_or_default();
            // A server whose page size is below ours still ends with an
            // empty page.
            let done = list.is_empty();
            bodies.extend(list);
            if done {
                break;
            }
        }
        let mut reviews = Vec::with_capacity(bodies.len());
        for body in bodies {
            let comments = if body.comments_count > 0 && !body.state.eq_ignore_ascii_case("PENDING")
            {
                let url = self.repo_api(
                    repo,
                    &format!("/pulls/{number}/reviews/{}/comments", body.id),
                );
                let list: Option<Vec<ReviewCommentBody>> = self.http.get_json(&url).await?;
                list.unwrap_or_default()
                    .into_iter()
                    .map(ReviewCommentBody::into_comment)
                    .collect()
            } else {
                Vec::new()
            };
            reviews.extend(body.into_review(comments));
        }
        Ok(reviews)
    }

    pub(super) async fn pulls_get(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<PullRequest>, ApiError> {
        let url = self.repo_api(repo, &format!("/pulls/{number}"));
        let body: Option<PrBody> = self.http.get_json_opt(&url).await?;
        Ok(body.map(|p| p.into_pull_request(true)))
    }
}

/// A review as Gitea and Forgejo list it (`PullReview`).
#[derive(Deserialize)]
struct ReviewBody {
    id: u64,
    user: Option<PrUser>,
    team: Option<TeamBody>,
    #[serde(default)]
    state: String,
    body: Option<String>,
    submitted_at: Option<String>,
    #[serde(default)]
    stale: bool,
    #[serde(default)]
    dismissed: bool,
    #[serde(default)]
    comments_count: u64,
}

#[derive(Deserialize)]
struct TeamBody {
    name: String,
}

/// A comment on a line of the diff (`PullReviewComment`). `position` is
/// the line in the new file, `original_position` the line in the old one
/// for a comment on a removed line; only one is set.
#[derive(Deserialize)]
struct ReviewCommentBody {
    id: u64,
    user: Option<PrUser>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    path: String,
    position: Option<u64>,
    original_position: Option<u64>,
    #[serde(default)]
    diff_hunk: String,
    resolver: Option<PrUser>,
}

impl ReviewBody {
    /// `None` for a state this server doesn't know.
    fn into_review(self, comments: Vec<ReviewComment>) -> Option<Review> {
        Some(Review {
            id: self.id,
            author: self
                .user
                .map(|u| u.login)
                .or(self.team.map(|t| t.name))
                .unwrap_or_default(),
            state: ReviewState::parse(&self.state)?,
            body: self
                .body
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty())
                .map(|b| pulls::cut_text(&b, pulls::REVIEW_TEXT_LIMIT)),
            submitted_at: nonzero(self.submitted_at),
            stale: self.stale,
            dismissed: self.dismissed,
            comments,
        })
    }
}

impl ReviewCommentBody {
    fn into_comment(self) -> ReviewComment {
        let (line, old_side) = match (
            self.position.filter(|&p| p > 0),
            self.original_position.filter(|&p| p > 0),
        ) {
            (Some(p), _) => (Some(p), false),
            (None, Some(p)) => (Some(p), true),
            (None, None) => (None, false),
        };
        ReviewComment {
            id: self.id,
            path: self.path,
            line,
            old_side,
            author: self.user.map(|u| u.login).unwrap_or_default(),
            body: pulls::cut_text(self.body.trim(), pulls::REVIEW_TEXT_LIMIT),
            diff_hunk: pulls::cut_hunk(&self.diff_hunk),
            resolved_by: self.resolver.map(|u| u.login),
        }
    }
}

#[derive(Deserialize)]
struct ForgejoRunList {
    workflow_runs: Option<Vec<ForgejoRun>>,
}

#[derive(Deserialize)]
struct ForgejoRun {
    id: u64,
    #[serde(default)]
    index_in_repo: u64,
    #[serde(default)]
    commit_sha: String,
    #[serde(default)]
    prettyref: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    workflow_id: String,
    html_url: Option<String>,
    started: Option<String>,
    stopped: Option<String>,
    /// Nanoseconds.
    duration: Option<i64>,
}

impl ForgejoRun {
    fn into_run(self) -> Run {
        let state = State::from_forgejo(&self.status);
        let finished = if state.is_final() {
            nonzero(self.stopped)
        } else {
            None
        };
        Run {
            id: self.id,
            number: self.index_in_repo,
            attempt: None,
            workflow: self.workflow_id,
            title: self.title,
            event: self.event,
            git_ref: self.prettyref,
            sha: self.commit_sha,
            state,
            url: self.html_url,
            duration_seconds: if state.is_final() {
                self.duration
                    .filter(|d| *d > 0)
                    .map(|ns| ns / NANOS_PER_SECOND)
            } else {
                None
            },
            started_at: nonzero(self.started),
            finished_at: finished,
            jobs: Vec::new(),
            external: None,
        }
    }
}

#[derive(Deserialize)]
struct ForgejoJob {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    task_id: Option<u64>,
    steps: Option<Vec<ForgejoStep>>,
}

#[derive(Deserialize)]
struct ForgejoStep {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    started: Option<String>,
    stopped: Option<String>,
}

impl ForgejoStep {
    fn to_step(&self) -> Step {
        let state = State::from_forgejo(&self.status);
        let finished = state
            .is_final()
            .then(|| nonzero(self.stopped.clone()))
            .flatten();
        Step {
            name: self.name.clone(),
            state,
            duration_seconds: ci::seconds_between(
                nonzero(self.started.clone()).as_deref(),
                finished.as_deref(),
            ),
        }
    }
}

impl ForgejoJob {
    fn into_job(self) -> Job {
        let state = State::from_forgejo(&self.status);
        // Forgejo 17 times each step; the job ran from the first start to
        // the last stop.
        let steps = self.steps.unwrap_or_default();
        let started = steps
            .iter()
            .filter_map(|s| nonzero(s.started.clone()))
            .min_by_key(|t| ci::parse_time(t));
        let finished = if state.is_final() {
            steps
                .iter()
                .filter_map(|s| nonzero(s.stopped.clone()))
                .max_by_key(|t| ci::parse_time(t))
        } else {
            None
        };
        Job {
            name: self.name,
            state,
            job_id: Some(self.id),
            task_id: self.task_id.filter(|id| *id > 0),
            duration_seconds: ci::seconds_between(started.as_deref(), finished.as_deref()),
            started_at: started,
            finished_at: finished,
            source: Source::Jobs,
            url: None,
            steps: Job::keep_steps(state, steps.iter().map(ForgejoStep::to_step).collect()),
        }
    }
}

#[derive(Deserialize)]
struct TaskList {
    workflow_runs: Option<Vec<Task>>,
}

#[derive(Deserialize)]
struct BranchBody {
    commit: BranchCommit,
    #[serde(default)]
    enable_status_check: bool,
    #[serde(default)]
    status_check_contexts: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BranchCommit {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgejo_15_run_shape() {
        let run: ForgejoRun = serde_json::from_value(serde_json::json!({
            "id": 1354, "index_in_repo": 2, "commit_sha": "a7e2", "prettyref": "main",
            "event": "push", "status": "success", "title": "t", "workflow_id": "ci.yml",
            "html_url": "https://git.example.com/acme/widgets/actions/runs/2",
            "started": "2026-11-03T20:20:24+11:00", "stopped": "2026-11-03T20:44:11+11:00",
            "duration": 1_427_000_000_000i64
        }))
        .unwrap();
        let run = run.into_run();
        assert_eq!((run.id, run.number), (1354, 2));
        assert_eq!(run.state, State::Success);
        assert_eq!(run.duration_seconds, Some(1427));
    }

    #[test]
    fn forgejo_steps_name_where_a_job_failed() {
        let job: ForgejoJob = serde_json::from_value(serde_json::json!({
            "id": 9, "name": "build", "status": "failure", "task_id": 33,
            "steps": [
                {"name": "Set up job", "number": 0, "status": "success",
                 "started": "2026-11-03T10:00:00Z", "stopped": "2026-11-03T10:00:10Z"},
                {"name": "Compile", "number": 1, "status": "failure",
                 "started": "2026-11-03T10:00:10Z", "stopped": "2026-11-03T10:01:00Z"}
            ]
        }))
        .unwrap();
        let job = job.into_job();
        assert_eq!(job.steps.len(), 2);
        assert_eq!(job.steps[1].state, State::Failure);
        assert_eq!(job.steps[1].duration_seconds, Some(50));
    }

    #[test]
    fn forgejo_17_job_times_come_from_steps() {
        let job: ForgejoJob = serde_json::from_value(serde_json::json!({
            "id": 9, "name": "test", "status": "success", "task_id": 33,
            "steps": [
                {"started": "2026-11-03T10:00:10Z", "stopped": "2026-11-03T10:00:20Z"},
                {"started": "2026-11-03T10:00:20Z", "stopped": "2026-11-03T10:02:00Z"},
                {"started": "0001-01-01T00:00:00Z", "stopped": "0001-01-01T00:00:00Z"}
            ]
        }))
        .unwrap();
        let job = job.into_job();
        assert_eq!(job.duration_seconds, Some(110));
        assert_eq!(job.task_id, Some(33));
    }

    /// A review and its line comment as Forgejo 17 returns them (Gitea's
    /// are the same, less Forgejo's `extra_lines_count`).
    #[test]
    fn reviews_and_line_comments() {
        let review: ReviewBody = serde_json::from_value(serde_json::json!({
            "id": 12, "user": {"id": 1, "login": "alice"}, "team": null,
            "state": "REQUEST_CHANGES", "body": "  Two things.\n", "commit_id": "abc",
            "stale": true, "official": true, "dismissed": false, "comments_count": 2,
            "submitted_at": "2026-11-09T10:00:00Z", "updated_at": "2026-11-09T10:00:00Z",
            "html_url": "https://example.com/acme/widgets/pulls/3#issuecomment-40",
            "pull_request_url": "https://example.com/acme/widgets/pulls/3"
        }))
        .unwrap();
        let comments: Vec<ReviewCommentBody> = serde_json::from_value(serde_json::json!([
            {
                "id": 40, "body": "Why this line?", "user": {"login": "alice"}, "resolver": null,
                "pull_request_review_id": 12, "created_at": "2026-11-09T10:00:00Z",
                "updated_at": "2026-11-09T10:00:00Z", "path": "src/lib.rs", "commit_id": "abc",
                "original_commit_id": "", "diff_hunk":
                    "@@ -1,6 +1,7 @@\n fn a() {}\n fn b() {}\n fn c() {}\n fn d() {}\n fn e() {}\n+fn f() {}\n",
                "position": 7, "original_position": 0, "extra_lines_count": 0,
                "html_url": "https://example.com/acme/widgets/pulls/3#issuecomment-40",
                "pull_request_url": "https://example.com/acme/widgets/pulls/3"
            },
            {
                "id": 41, "body": "Keep this.", "user": {"login": "alice"},
                "resolver": {"login": "dev"}, "path": "src/old.rs",
                "diff_hunk": "@@ -3,2 +3,1 @@\n-fn old() {}", "position": 0, "original_position": 4
            }
        ]))
        .unwrap();
        let review = review
            .into_review(
                comments
                    .into_iter()
                    .map(ReviewCommentBody::into_comment)
                    .collect(),
            )
            .unwrap();
        assert_eq!(review.author, "alice");
        assert_eq!(review.state, ReviewState::RequestChanges);
        assert_eq!(review.body.as_deref(), Some("Two things."));
        assert!(review.stale && !review.dismissed);
        let new_line = &review.comments[0];
        assert_eq!((new_line.line, new_line.old_side), (Some(7), false));
        assert_eq!(
            new_line.diff_hunk,
            " fn b() {}\n fn c() {}\n fn d() {}\n fn e() {}\n+fn f() {}"
        );
        assert_eq!(new_line.resolved_by, None);
        let old_line = &review.comments[1];
        assert_eq!((old_line.line, old_line.old_side), (Some(4), true));
        assert_eq!(old_line.resolved_by.as_deref(), Some("dev"));
        assert_eq!(old_line.diff_hunk, "@@ -3,2 +3,1 @@\n-fn old() {}");
    }

    #[test]
    fn review_requests_name_a_team_and_unknown_states_are_skipped() {
        let request: ReviewBody = serde_json::from_value(serde_json::json!({
            "id": 3, "user": null, "team": {"id": 2, "name": "maintainers"},
            "state": "REQUEST_REVIEW", "body": "", "submitted_at": "0001-01-01T00:00:00Z"
        }))
        .unwrap();
        let request = request.into_review(vec![]).unwrap();
        assert_eq!(request.author, "maintainers");
        assert_eq!(request.state, ReviewState::ReviewRequested);
        assert_eq!((request.body, request.submitted_at), (None, None));
        let odd: ReviewBody =
            serde_json::from_value(serde_json::json!({"id": 4, "state": "SOMETHING_NEW"})).unwrap();
        assert!(odd.into_review(vec![]).is_none());
    }
}
