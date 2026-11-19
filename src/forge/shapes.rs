//! Response shapes GitHub defined and Gitea copied: Actions runs, jobs and
//! steps, and pull requests. Both drivers read them; where the two differ,
//! a field is optional and the difference is noted on it.
//!
//! | | GitHub | Gitea |
//! |---|---|---|
//! | run start | `run_started_at` | `started_at` |
//! | run end | none: `updated_at` once completed | `completed_at` |
//! | the number people use | the run `id` (`run_number` counts per workflow) | `run_number` |
//! | rerun attempt | `run_attempt` | none |
//! | merged, in a list | only `merged_at` | `merged` |

use serde::Deserialize;

use crate::ci::{self, Job, Run, Source, State, Step};
use crate::pulls::{self, PrState, PullRequest};

/// Forges report never-set times as the zero time.
pub(crate) fn nonzero(t: Option<String>) -> Option<String> {
    t.filter(|t| ci::parse_time(t).is_some())
}

/// A name, unless the forge sent none: a job not yet taken has an empty
/// `runner_name`, or none at all.
pub(crate) fn named(name: Option<String>) -> Option<String> {
    name.filter(|n| !n.is_empty())
}

#[derive(Deserialize)]
pub(crate) struct ActionsRunList {
    /// GitHub only.
    #[serde(default)]
    pub total_count: Option<u64>,
    pub workflow_runs: Option<Vec<ActionsRun>>,
}

#[derive(Deserialize)]
pub(crate) struct ActionsRun {
    pub id: u64,
    #[serde(default)]
    pub run_number: u64,
    /// GitHub only: 1, then one more for each rerun.
    pub run_attempt: Option<u64>,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: String,
    pub conclusion: Option<String>,
    #[serde(default)]
    pub display_title: String,
    #[serde(default)]
    pub path: String,
    pub html_url: Option<String>,
    #[serde(alias = "run_started_at")]
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Which number a forge's people and URLs use for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunNumber {
    /// Gitea: `run_number`, per repository.
    RunNumber,
    /// GitHub: the id. Its `run_number` counts per workflow, so two
    /// workflows share every number, while its URLs carry the id.
    Id,
}

impl ActionsRun {
    pub(crate) fn into_run(self, number: RunNumber) -> Run {
        let state = match self.conclusion.as_deref() {
            // GitHub: a run from a first-time contributor's fork waits for
            // a maintainer to approve it. Nothing has run, nothing failed.
            Some("action_required") => State::Blocked,
            // GitHub gives up on a run left unfinished for weeks.
            Some("stale") => State::Cancelled,
            conclusion => State::from_status_conclusion(&self.status, conclusion),
        };
        let started = nonzero(self.started_at);
        let finished = if state.is_final() {
            nonzero(self.completed_at).or_else(|| nonzero(self.updated_at))
        } else {
            None
        };
        Run {
            id: self.id,
            number: match number {
                RunNumber::RunNumber => self.run_number,
                RunNumber::Id => self.id,
            },
            attempt: self.run_attempt.filter(|a| *a > 1),
            workflow: workflow_file(&self.path),
            title: self.display_title,
            event: self.event,
            git_ref: self.head_branch.unwrap_or_default(),
            sha: self.head_sha,
            state,
            url: self.html_url,
            duration_seconds: ci::seconds_between(started.as_deref(), finished.as_deref()),
            started_at: started,
            finished_at: finished,
            jobs: Vec::new(),
            external: None,
        }
    }
}

/// ".gitea/workflows/ci.yml@refs/heads/main" -> "ci.yml". GitHub's runs
/// that no file defines (code scanning, Dependabot) have paths such as
/// "dynamic/github-code-scanning/codeql", which give "codeql".
fn workflow_file(path: &str) -> String {
    path.split('@')
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[derive(Deserialize)]
pub(crate) struct ActionsJobList {
    pub jobs: Option<Vec<ActionsJob>>,
}

#[derive(Deserialize)]
pub(crate) struct ActionsJob {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// The `runs-on` labels the job asked for.
    pub labels: Option<Vec<String>>,
    pub steps: Option<Vec<ActionsStep>>,
    /// The runner that took it.
    pub runner_name: Option<String>,
    /// When it was queued.
    pub created_at: Option<String>,
    /// Its page, which names the repository.
    pub html_url: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ActionsStep {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

impl ActionsStep {
    fn into_step(self) -> Step {
        let state = State::from_status_conclusion(&self.status, self.conclusion.as_deref());
        let finished = state
            .is_final()
            .then(|| nonzero(self.completed_at))
            .flatten();
        Step {
            name: self.name,
            state,
            duration_seconds: ci::seconds_between(
                nonzero(self.started_at).as_deref(),
                finished.as_deref(),
            ),
        }
    }
}

impl ActionsJob {
    pub(crate) fn state(&self) -> State {
        State::from_status_conclusion(&self.status, self.conclusion.as_deref())
    }

    pub(crate) fn into_job(self) -> Job {
        let state = self.state();
        // GitHub stamps a queued job's start with when it was queued.
        let started = if matches!(state, State::Waiting | State::Blocked) {
            None
        } else {
            nonzero(self.started_at)
        };
        let finished = if state.is_final() {
            nonzero(self.completed_at)
        } else {
            None
        };
        Job {
            name: self.name,
            state,
            job_id: Some(self.id),
            task_id: None,
            duration_seconds: ci::seconds_between(started.as_deref(), finished.as_deref()),
            started_at: started,
            finished_at: finished,
            source: Source::Jobs,
            url: None,
            steps: Job::keep_steps(
                state,
                self.steps
                    .unwrap_or_default()
                    .into_iter()
                    .map(ActionsStep::into_step)
                    .collect(),
            ),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct PrBody {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    /// Gitea always; GitHub only for a single pull request, not in a list.
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub draft: bool,
    pub user: Option<PrUser>,
    pub head: PrBranch,
    pub base: PrBranch,
    /// GitHub: `null` while it works out whether the merge would conflict.
    pub mergeable: Option<bool>,
    #[serde(default)]
    pub labels: Option<Vec<PrLabel>>,
    #[serde(default)]
    pub html_url: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub body: Option<String>,
    /// GitHub: the id its GraphQL API takes.
    #[serde(default)]
    pub node_id: Option<String>,
    /// GitHub: the merge scheduled for when checks pass, if any.
    #[serde(default)]
    pub auto_merge: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct PrUser {
    pub login: String,
}

#[derive(Deserialize)]
pub(crate) struct PrBranch {
    #[serde(rename = "ref", default)]
    pub git_ref: String,
    #[serde(default)]
    pub sha: String,
    /// The repository the branch is in: a fork's, for a pull request from
    /// one. `null` once a fork is deleted.
    #[serde(default)]
    pub repo: Option<PrRepo>,
}

#[derive(Deserialize)]
pub(crate) struct PrRepo {
    #[serde(default)]
    pub full_name: String,
}

#[derive(Deserialize)]
pub(crate) struct PrLabel {
    pub name: String,
}

impl PrBody {
    pub(crate) fn is_merged(&self) -> bool {
        self.merged || nonzero(self.merged_at.clone()).is_some()
    }

    pub(crate) fn into_pull_request(self, with_body: bool) -> PullRequest {
        let state = if self.is_merged() {
            PrState::Merged
        } else if self.state == "closed" {
            PrState::Closed
        } else {
            PrState::Open
        };
        PullRequest {
            number: self.number,
            title: self.title,
            state,
            draft: self.draft,
            author: self.user.map(|u| u.login).unwrap_or_default(),
            head: self.head.git_ref,
            head_sha: self.head.sha,
            base: self.base.git_ref,
            mergeable: if state == PrState::Open {
                self.mergeable
            } else {
                None
            },
            labels: self
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
            url: self.html_url,
            created_at: self.created_at,
            updated_at: self.updated_at,
            merged_at: nonzero(self.merged_at),
            merge_commit_sha: self
                .merge_commit_sha
                .filter(|sha| state == PrState::Merged && !sha.is_empty()),
            body: if with_body {
                self.body
                    .filter(|b| !b.is_empty())
                    .map(|b| pulls::cut_body(&b))
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitea_run_shape() {
        let run: ActionsRun = serde_json::from_value(serde_json::json!({
            "id": 1197, "run_number": 1143, "head_sha": "abc", "head_branch": "develop",
            "event": "push", "status": "completed", "conclusion": "failure",
            "display_title": "t", "path": ".gitea/workflows/build.yml@refs/heads/develop",
            "started_at": "2026-11-03T10:00:00Z", "completed_at": "2026-11-03T10:05:00Z"
        }))
        .unwrap();
        let run = run.into_run(RunNumber::RunNumber);
        assert_eq!((run.id, run.number, run.attempt), (1197, 1143, None));
        assert_eq!(run.workflow, "build.yml");
        assert_eq!(run.state, State::Failure);
        assert_eq!(run.duration_seconds, Some(300));
    }

    /// A rerun GitHub run: numbered by its id, on its second attempt, timed
    /// from the attempt's start to its last update.
    #[test]
    fn github_run_shape() {
        let run: ActionsRun = serde_json::from_value(serde_json::json!({
            "id": 30433642, "name": "CI", "run_number": 562, "run_attempt": 2,
            "event": "pull_request", "status": "completed", "conclusion": "success",
            "head_branch": "feature/x", "head_sha": "abc", "display_title": "Add x",
            "path": ".github/workflows/ci.yml",
            "html_url": "https://github.com/acme/widgets/actions/runs/30433642",
            "created_at": "2026-11-03T09:00:00Z",
            "run_started_at": "2026-11-03T10:00:00Z", "updated_at": "2026-11-03T10:04:30Z"
        }))
        .unwrap();
        let run = run.into_run(RunNumber::Id);
        assert_eq!(
            (run.id, run.number, run.attempt),
            (30433642, 30433642, Some(2))
        );
        assert_eq!(run.workflow, "ci.yml");
        assert_eq!(run.duration_seconds, Some(270));
    }

    #[test]
    fn a_github_run_awaiting_approval_is_blocked_not_failed() {
        let run: ActionsRun = serde_json::from_value(serde_json::json!({
            "id": 5, "status": "completed", "conclusion": "action_required",
            "path": "dynamic/github-code-scanning/codeql", "updated_at": "2026-11-03T10:00:00Z"
        }))
        .unwrap();
        let run = run.into_run(RunNumber::Id);
        assert_eq!(run.state, State::Blocked);
        assert_eq!(run.finished_at, None);
        assert_eq!(run.workflow, "codeql");
    }

    #[test]
    fn gitea_steps_are_kept_while_a_job_runs() {
        let job: ActionsJob = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "test", "status": "in_progress",
            "started_at": "2026-11-03T10:00:00Z",
            "steps": [
                {"name": "Set up job", "number": 1, "status": "completed", "conclusion": "success",
                 "started_at": "2026-11-03T10:00:00Z", "completed_at": "2026-11-03T10:00:05Z"},
                {"name": "cargo test", "number": 2, "status": "in_progress",
                 "started_at": "2026-11-03T10:00:05Z", "completed_at": "0001-01-01T00:00:00Z"},
                {"name": "Upload", "number": 3, "status": "queued"}
            ]
        }))
        .unwrap();
        let job = job.into_job();
        let states: Vec<State> = job.steps.iter().map(|s| s.state).collect();
        assert_eq!(states, [State::Success, State::Running, State::Waiting]);
        assert_eq!(job.steps[0].duration_seconds, Some(5));
        assert_eq!(job.steps[1].duration_seconds, None);
    }

    #[test]
    fn a_queued_github_job_has_not_started() {
        let job: ActionsJob = serde_json::from_value(serde_json::json!({
            "id": 8, "name": "build", "status": "queued", "conclusion": null,
            "started_at": "2026-11-03T10:00:00Z", "completed_at": null,
            "labels": ["ubuntu-latest"], "steps": []
        }))
        .unwrap();
        let job = job.into_job();
        assert_eq!(job.state, State::Waiting);
        assert_eq!(job.started_at, None);
    }

    #[test]
    fn merged_pull_requests() {
        let pr: PrBody = serde_json::from_value(serde_json::json!({
            "number": 4, "title": "t", "state": "closed", "merged": true, "mergeable": true,
            "user": {"login": "dev"}, "head": {"ref": "feature/x", "sha": "abc"},
            "base": {"ref": "main", "sha": "def"}, "html_url": "u",
            "merged_at": "2026-11-03T10:00:00Z", "body": "hello", "merge_commit_sha": "fed"
        }))
        .unwrap();
        let pr = pr.into_pull_request(true);
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.merge_commit_sha.as_deref(), Some("fed"));
        assert_eq!(pr.mergeable, None);
        assert_eq!(pr.body.as_deref(), Some("hello"));
    }

    /// GitHub's list leaves out `merged`; `merged_at` says it. An open pull
    /// request's `merge_commit_sha` is GitHub's test merge, not a merge.
    #[test]
    fn github_lists_say_merged_only_by_merged_at() {
        let merged: PrBody = serde_json::from_value(serde_json::json!({
            "number": 9, "state": "closed", "head": {"ref": "a", "sha": "1"},
            "base": {"ref": "main", "sha": "2"}, "merged_at": "2026-11-03T10:00:00Z",
            "merge_commit_sha": "3"
        }))
        .unwrap();
        assert_eq!(merged.into_pull_request(false).state, PrState::Merged);
        let open: PrBody = serde_json::from_value(serde_json::json!({
            "number": 10, "state": "open", "head": {"ref": "b", "sha": "1"},
            "base": {"ref": "main", "sha": "2"}, "merged_at": null, "mergeable": null,
            "merge_commit_sha": "4"
        }))
        .unwrap();
        let open = open.into_pull_request(false);
        assert_eq!(open.state, PrState::Open);
        assert_eq!((open.mergeable, open.merge_commit_sha), (None, None));
    }
}
