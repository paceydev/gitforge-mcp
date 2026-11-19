//! CI on GitHub: Actions runs, jobs, logs and runners, and CI outside
//! Actions, which reports through commit statuses and check runs.
//!
//! Where it differs from Gitea and Forgejo:
//!
//! - A run is named by its id, as its URL is. `run_number` counts per
//!   workflow, so every workflow has a run 5.
//! - A rerun keeps the run's id and adds an attempt. The jobs listed are
//!   each job's latest attempt (`filter=latest`), so a job that failed and
//!   then passed on a rerun counts as passed.
//! - A run from a first-time contributor's fork waits for a maintainer to
//!   approve it, reported as `action_required`: blocked, not failed.
//! - Every Actions job is also a check run, from the `github-actions` app.
//!   Check runs from other apps are CI outside Actions; Actions' own are
//!   left out there, or every job would count twice.
//! - A job's log is served once the job has finished, by a redirect to
//!   storage on another host. The credential isn't sent there.
//! - GitHub-hosted runners aren't listed by the API, and a personal account
//!   has no runners of its own.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::Github;
use crate::ci::{self, CommitStatus, Job, Run, RunsOf, Source, State};
use crate::forge::shapes::{
    ActionsJob, ActionsJobList, ActionsRun, ActionsRunList, RunNumber, named, nonzero,
};
use crate::forge::{ApiError, User, list_runners, reaches};
use crate::remote::RepoRef;
use crate::runners::{self, ActiveJob, ActiveJobs, Activity, RunnerReport, Scope, ScopeJobs};

/// Logs are fetched whole only up to this size; beyond it, only the end.
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
/// The least of a log kept for a tail, however short the tail.
const MIN_TAIL_LOG_BYTES: usize = 64 * 1024;
/// GitHub's largest page.
const PAGE_SIZE: usize = 100;
/// Pages of jobs read for one run: a big matrix has hundreds.
const JOB_PAGES: u32 = 10;
/// Pages of commit statuses, and of check runs, read for one commit.
const STATUS_PAGES: u32 = 5;
/// Runs looked at, per state, for jobs waiting for a runner.
const QUEUE_RUNS: usize = 10;
/// The label every self-hosted runner carries. A job without it asked for
/// a GitHub-hosted runner.
const SELF_HOSTED: &str = "self-hosted";
/// The app that posts Actions' own check runs.
const ACTIONS_APP: &str = "github-actions";

fn encode(v: &str) -> String {
    url::form_urlencoded::byte_serialize(v.as_bytes()).collect()
}

impl Github {
    pub(super) async fn ci_runs_for_commit(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<Vec<Run>, ApiError> {
        let mut runs = self
            .list_runs(&self.repo_api(
                repo,
                &format!(
                    "/actions/runs?head_sha={sha}&per_page={PAGE_SIZE}&exclude_pull_requests=true"
                ),
            ))
            .await?;
        runs.retain(|r| r.sha.eq_ignore_ascii_case(sha));
        self.fill_jobs(repo, &mut runs).await?;
        Ok(runs)
    }

    /// The run with id `id`: what GitHub's URLs, and so people, call it.
    pub(super) async fn ci_run_by_id(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> Result<Option<Run>, ApiError> {
        let Some(run) = self
            .http
            .get_json_opt::<ActionsRun>(&self.repo_api(repo, &format!("/actions/runs/{id}")))
            .await?
        else {
            return Ok(None);
        };
        let mut runs = vec![run.into_run(RunNumber::Id)];
        self.fill_jobs(repo, &mut runs).await?;
        Ok(runs.pop())
    }

    pub(super) async fn ci_recent_runs(
        &self,
        repo: &RepoRef,
        of: RunsOf<'_>,
        pages: u32,
        after: Option<u64>,
    ) -> Result<Vec<Run>, ApiError> {
        let mut found = Vec::new();
        for page in 1..=pages {
            let url = self.repo_api(
                repo,
                &format!(
                    "/actions/workflows/{}/runs?event={}&branch={}&per_page={PAGE_SIZE}&page={page}&exclude_pull_requests=true",
                    encode(of.workflow),
                    encode(of.event),
                    encode(of.branch)
                ),
            );
            // A run no workflow file defines (code scanning, say) has no
            // workflow endpoint, and no history to learn from.
            let Some(list) = self.http.get_json_opt::<ActionsRunList>(&url).await? else {
                break;
            };
            let runs: Vec<Run> = list
                .workflow_runs
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.into_run(RunNumber::Id))
                .collect();
            let last = runs.len() < PAGE_SIZE || reaches(&runs, after);
            found.extend(runs.into_iter().filter(|r| of.includes(r)));
            if last {
                break;
            }
        }
        Ok(found)
    }

    pub(super) async fn ci_has_runs(&self, repo: &RepoRef) -> Result<bool, ApiError> {
        let list: ActionsRunList = self
            .http
            .get_json(&self.repo_api(repo, "/actions/runs?per_page=1"))
            .await?;
        Ok(list.total_count.unwrap_or(0) > 0 || !list.workflow_runs.unwrap_or_default().is_empty())
    }

    pub(super) async fn ci_jobs_of(&self, repo: &RepoRef, run: &Run) -> Result<Vec<Job>, ApiError> {
        let mut runs = vec![run.clone()];
        self.fill_jobs(repo, &mut runs).await?;
        Ok(runs.pop().map(|r| r.jobs).unwrap_or_default())
    }

    async fn list_runs(&self, url: &str) -> Result<Vec<Run>, ApiError> {
        let list: ActionsRunList = self.http.get_json(url).await?;
        Ok(list
            .workflow_runs
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.into_run(RunNumber::Id))
            .collect())
    }

    async fn fill_jobs(&self, repo: &RepoRef, runs: &mut [Run]) -> Result<(), ApiError> {
        for run in runs.iter_mut() {
            run.jobs = self
                .raw_jobs(repo, run.id)
                .await?
                .into_iter()
                .map(ActionsJob::into_job)
                .collect();
            if run.state == State::Blocked && run.jobs.is_empty() {
                run.jobs.push(awaiting_approval());
            }
            run.add_unlisted_placeholder();
        }
        Ok(())
    }

    /// The latest attempt of each job of run `id`.
    async fn raw_jobs(&self, repo: &RepoRef, id: u64) -> Result<Vec<ActionsJob>, ApiError> {
        self.http
            .paged(
                JOB_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!(
                            "/actions/runs/{id}/jobs?filter=latest&per_page={PAGE_SIZE}&page={page}"
                        ),
                    )
                },
                |list: ActionsJobList| list.jobs.unwrap_or_default(),
            )
            .await
    }

    /// CI outside Actions: commit statuses (Jenkins, Woodpecker…) and check
    /// runs from apps other than Actions (CircleCI, Codecov…), one run per
    /// system.
    pub(super) async fn ci_external_runs(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<Vec<Run>, ApiError> {
        let statuses = match self.statuses(repo, sha).await {
            Ok(statuses) => statuses,
            // A commit GitHub doesn't have (not pushed yet) has none.
            Err(ApiError::NotFound { .. }) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut runs = ci::external_runs(&statuses, sha);
        let checks: Vec<CheckRun> = self
            .http
            .paged(
                STATUS_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!(
                            "/commits/{sha}/check-runs?filter=latest&per_page={PAGE_SIZE}&page={page}"
                        ),
                    )
                },
                |list: CheckRunList| list.check_runs,
            )
            .await?;
        runs.extend(runs_from_checks(checks, sha));
        Ok(runs)
    }

    async fn statuses(&self, repo: &RepoRef, sha: &str) -> Result<Vec<CommitStatus>, ApiError> {
        self.http
            .paged(
                STATUS_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/commits/{sha}/statuses?per_page={PAGE_SIZE}&page={page}"),
                    )
                },
                |batch: Vec<CommitStatus>| batch,
            )
            .await
    }

    pub(super) async fn ci_branch_head(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<String>, ApiError> {
        Ok(self.branch(repo, branch).await?.map(|b| b.commit.sha))
    }

    pub(super) async fn branch(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<BranchBody>, ApiError> {
        self.http
            .get_json_opt(&self.repo_api(repo, &format!("/branches/{branch}")))
            .await
    }

    pub(super) async fn ci_job_log(
        &self,
        repo: &RepoRef,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        let Some(id) = job.job_id else {
            return Err(ApiError::Unavailable {
                reason: format!("{} has no log: it hasn't been given to a runner", job.name),
            });
        };
        if !job.state.is_final() {
            return Err(ApiError::Unavailable {
                reason: format!(
                    "GitHub serves a job's log once the job has finished, and {} is {}; follow it on the run's page",
                    job.name,
                    format!("{:?}", job.state).to_lowercase()
                ),
            });
        }
        // GitHub can't serve just the end, so a tail is cut once it arrives.
        let max = tail_bytes.map_or(MAX_LOG_BYTES, |n| (n as usize).max(MIN_TAIL_LOG_BYTES));
        let url = self.repo_api(repo, &format!("/actions/jobs/{id}/logs"));
        match self.http.get_text_tail(&url, None, max).await {
            Err(ApiError::NotFound { .. } | ApiError::Status { status: 410, .. }) => {
                Err(ApiError::Unavailable {
                    reason: format!(
                        "GitHub has no log for {}: logs expire (90 days by default), and a skipped job has none",
                        job.name
                    ),
                })
            }
            other => other,
        }
    }

    pub(super) async fn ci_runners(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> Result<RunnerReport, ApiError> {
        let owner = match user {
            Some(u) if u.login.eq_ignore_ascii_case(&repo.owner) => Err(
                "unavailable: a personal account has no runners of its own on GitHub, only its repositories do"
                    .to_owned(),
            ),
            _ => Ok(format!("/orgs/{}/actions/runners?per_page={PAGE_SIZE}", repo.owner)),
        };
        let scopes = [
            (
                Scope::Repository,
                Ok(format!(
                    "/repos/{}/{}/actions/runners?per_page={PAGE_SIZE}",
                    repo.owner, repo.name
                )),
            ),
            (Scope::Owner, owner),
            (
                Scope::Instance,
                Err(
                    "unavailable: GitHub-hosted runners (ubuntu-latest, windows-latest, macos-latest…) aren't listed by the API, and enterprise runners need the enterprise's name"
                        .to_owned(),
                ),
            ),
        ];
        let (found, outcomes) = list_runners(
            &self.http,
            |path| format!("{}{path}", self.api),
            scopes,
            "unavailable: the owner isn't an organisation",
        )
        .await?;
        let active = self.active_jobs(repo).await?;
        let waiting = active.waiting_in(&repo.full_name());
        Ok(runners::report(
            found,
            outcomes,
            waiting,
            active,
            |labels: &[String]| !labels.iter().any(|l| l.eq_ignore_ascii_case(SELF_HOSTED)),
        ))
    }

    /// The repository's jobs waiting for a runner and running on one, with
    /// the labels they asked for: those of the newest queued and running
    /// runs. GitHub lists jobs only per repository, so the owner's other
    /// repositories, which may share its runners, aren't seen.
    async fn active_jobs(&self, repo: &RepoRef) -> Result<ActiveJobs, ApiError> {
        let raw = self.queued_and_running_jobs(repo).await?;
        Ok(ActiveJobs::of(github_scopes(repo, raw)))
    }

    /// The jobs of the newest queued and running runs.
    async fn queued_and_running_jobs(&self, repo: &RepoRef) -> Result<Vec<ActionsJob>, ApiError> {
        let mut jobs = Vec::new();
        for status in ["queued", "in_progress"] {
            let runs = self
                .list_runs(&self.repo_api(
                    repo,
                    &format!(
                        "/actions/runs?status={status}&per_page={QUEUE_RUNS}&exclude_pull_requests=true"
                    ),
                ))
                .await?;
            for run in runs {
                jobs.extend(self.raw_jobs(repo, run.id).await?);
            }
        }
        Ok(jobs)
    }
}

/// The repository's jobs waiting and running, and the scopes GitHub can't
/// list jobs for. A run both queued and running is listed twice; its jobs
/// count once.
fn github_scopes(repo: &RepoRef, raw: Vec<ActionsJob>) -> Vec<ScopeJobs> {
    let jobs = raw
        .into_iter()
        .filter_map(|job| {
            let activity = match job.state() {
                State::Waiting => Activity::Waiting,
                State::Running => Activity::Running,
                _ => return None,
            };
            Some(ActiveJob {
                id: job.id,
                name: job.name,
                repo: repo.full_name(),
                labels: job.labels.unwrap_or_default(),
                activity,
                runner: (activity == Activity::Running)
                    .then(|| named(job.runner_name))
                    .flatten(),
                queued_at: nonzero(job.created_at),
            })
        })
        .collect();
    let unlisted = |scope| {
        ScopeJobs::not_listed(
            scope,
            String::new(),
            "unavailable: GitHub lists jobs only per repository".into(),
        )
    };
    vec![
        ScopeJobs::listed(
            Scope::Repository,
            format!(
                "/repos/{}/{}/actions/runs/{{run}}/jobs",
                repo.owner, repo.name
            ),
            jobs,
        ),
        unlisted(Scope::Owner),
        unlisted(Scope::Instance),
    ]
}

/// The job that stands for a run waiting for a maintainer's approval, which
/// lists none until it's approved.
fn awaiting_approval() -> Job {
    Job {
        name: "(waiting for a maintainer to approve the run)".into(),
        state: State::Blocked,
        job_id: None,
        task_id: None,
        started_at: None,
        finished_at: None,
        duration_seconds: None,
        source: Source::Run,
        url: None,
        steps: Vec::new(),
    }
}

#[derive(Deserialize)]
pub(super) struct BranchBody {
    pub commit: BranchCommit,
    /// Classic branch protection, where the caller may see it.
    #[serde(default)]
    pub protection: Option<Protection>,
}

#[derive(Deserialize)]
pub(super) struct BranchCommit {
    pub sha: String,
}

#[derive(Deserialize)]
pub(super) struct Protection {
    #[serde(default)]
    pub enabled: bool,
    pub required_status_checks: Option<RequiredChecks>,
}

#[derive(Deserialize)]
pub(super) struct RequiredChecks {
    /// `off`, `non_admins` or `everyone`.
    #[serde(default)]
    pub enforcement_level: String,
    #[serde(default)]
    pub contexts: Vec<String>,
}

#[derive(Deserialize)]
struct CheckRunList {
    #[serde(default)]
    check_runs: Vec<CheckRun>,
}

#[derive(Deserialize)]
struct CheckRun {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
    details_url: Option<String>,
    app: Option<CheckApp>,
}

#[derive(Deserialize)]
struct CheckApp {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
}

impl CheckRun {
    fn state(&self) -> State {
        match self.conclusion.as_deref() {
            // GitHub gives up on a check left unfinished for two weeks.
            Some("stale") => State::Cancelled,
            conclusion => State::from_status_conclusion(&self.status, conclusion),
        }
    }

    fn into_job(self) -> Job {
        let state = self.state();
        let started = nonzero(self.started_at);
        let finished = state
            .is_final()
            .then(|| nonzero(self.completed_at))
            .flatten();
        Job {
            name: self.name,
            state,
            job_id: None,
            task_id: None,
            duration_seconds: ci::seconds_between(started.as_deref(), finished.as_deref()),
            started_at: started,
            finished_at: finished,
            source: Source::Checks,
            url: self.details_url.or(self.html_url).filter(|u| !u.is_empty()),
            steps: Vec::new(),
        }
    }
}

/// One run per app, from the check runs of apps other than Actions.
fn runs_from_checks(checks: Vec<CheckRun>, sha: &str) -> Vec<Run> {
    let mut apps: BTreeMap<String, Vec<Job>> = BTreeMap::new();
    for check in checks {
        let app = match &check.app {
            Some(app) if app.slug == ACTIONS_APP => continue,
            Some(app) if !app.name.is_empty() => app.name.clone(),
            Some(app) if !app.slug.is_empty() => app.slug.clone(),
            _ => "checks".to_owned(),
        };
        apps.entry(app).or_default().push(check.into_job());
    }
    apps.into_iter()
        .map(|(app, jobs)| {
            let url = jobs.iter().find_map(|j| j.url.clone());
            ci::external_run(app, jobs, url, sha)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(v: serde_json::Value) -> Vec<CheckRun> {
        serde_json::from_value::<CheckRunList>(v)
            .unwrap()
            .check_runs
    }

    #[test]
    fn a_running_job_keeps_the_runner_it_runs_on() {
        let jobs: Vec<ActionsJob> = serde_json::from_value(serde_json::json!([
            { "id": 1, "name": "build", "status": "in_progress", "labels": ["self-hosted"],
              "runner_name": "mac-1", "created_at": "2026-11-17T00:00:00Z" },
            { "id": 2, "name": "lint", "status": "queued", "labels": ["self-hosted"],
              "runner_name": null, "created_at": "2026-11-17T00:01:00Z" },
            { "id": 3, "name": "done", "status": "completed", "conclusion": "success" }
        ]))
        .unwrap();
        let repo = RepoRef {
            host: "github.com".into(),
            base_url: url::Url::parse("https://github.com").unwrap(),
            owner: "acme".into(),
            name: "widgets".into(),
        };
        let scopes = github_scopes(&repo, jobs);
        let listed = &scopes[0].jobs;
        assert_eq!(listed.len(), 2, "the finished job is left out");
        assert_eq!(listed[0].runner.as_deref(), Some("mac-1"));
        assert_eq!(listed[1].runner, None);
        assert_eq!(
            scopes[1].outcome,
            "unavailable: GitHub lists jobs only per repository"
        );
    }

    /// Actions' own check runs are its jobs, already counted; another app's
    /// are CI outside Actions, one run per app.
    #[test]
    fn check_runs_from_other_apps_are_external_ci() {
        let runs = runs_from_checks(
            checks(serde_json::json!({"total_count": 4, "check_runs": [
                {"id": 1, "name": "test", "status": "completed", "conclusion": "failure",
                 "app": {"slug": "github-actions", "name": "GitHub Actions"}},
                {"id": 2, "name": "ci/build", "status": "completed", "conclusion": "success",
                 "started_at": "2026-11-03T10:00:00Z", "completed_at": "2026-11-03T10:02:00Z",
                 "details_url": "https://ci.example.com/b/2", "app": {"slug": "acme-ci", "name": "Acme CI"}},
                {"id": 3, "name": "ci/lint", "status": "in_progress", "conclusion": null,
                 "started_at": "2026-11-03T10:00:00Z", "app": {"slug": "acme-ci", "name": "Acme CI"}},
                {"id": 4, "name": "coverage", "status": "completed", "conclusion": "neutral",
                 "html_url": "https://github.com/acme/widgets/runs/4", "app": {"slug": "cov", "name": "Cov"}}
            ]})),
            "abc",
        );
        let names: Vec<&str> = runs.iter().map(|r| r.workflow.as_str()).collect();
        assert_eq!(names, ["Acme CI", "Cov"]);
        assert_eq!(runs[0].state, State::Running);
        assert_eq!(runs[0].external.as_deref(), Some("Acme CI"));
        assert_eq!(runs[0].jobs[0].duration_seconds, Some(120));
        assert_eq!(runs[0].url.as_deref(), Some("https://ci.example.com/b/2"));
        assert_eq!(runs[1].state, State::Success);
        assert!(
            runs.iter()
                .flat_map(|r| &r.jobs)
                .all(|j| j.source == Source::Checks)
        );
    }

    #[test]
    fn stale_and_skipped_checks() {
        let runs = runs_from_checks(
            checks(serde_json::json!({"check_runs": [
                {"name": "old", "status": "completed", "conclusion": "stale", "app": {"slug": "x", "name": "X"}},
                {"name": "docs", "status": "completed", "conclusion": "skipped", "app": {"slug": "y", "name": "Y"}}
            ]})),
            "abc",
        );
        assert_eq!(runs[0].jobs[0].state, State::Cancelled);
        assert_eq!(runs[0].state, State::Failure);
        assert_eq!(runs[1].state, State::Skipped);
    }
}
