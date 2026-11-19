//! Writes on Gitea and Forgejo: pull requests (merged now or when checks
//! pass), and cancelling, rerunning and dispatching CI runs.
//!
//! The endpoints differ by fork and version (checked against each release's
//! swagger spec):
//!
//! | | Gitea | Forgejo |
//! |---|---|---|
//! | merge body key | `do` from 1.26, `Do` before | `Do` |
//! | merge when checks succeed | `merge_when_checks_succeed`, cancel with `DELETE pulls/{n}/merge`, 1.22 on | the same, 9 on |
//! | cancel a run | `runs/{id}/cancel`, v28 | `runs/{id}/cancel`, 16 |
//! | rerun a run | `runs/{id}/rerun`, 1.26 | `runs/{id}/rerun`, 17 |
//! | rerun failed jobs | `runs/{id}/rerun-failed-jobs`, 1.26 | none: rerun each failed job, `jobs/{id}/rerun`, 17 |
//! | dispatch | `workflows/{id}/dispatches`, run details from 1.26 with `?return_run_details=true` | `workflows/{file}/dispatches`, run details with `return_run_info` |

use serde_json::{Map, Value, json};

use super::GiteaFamily;
use crate::ci::{self, Dispatched, Run};
use crate::forge::ApiError;
use crate::forge::shapes::ActionsRun;
use crate::pulls::{MergeStyle, NewPullRequest, PullRequest};
use crate::remote::RepoRef;
use crate::version::GITEA_1_26;

impl GiteaFamily {
    pub(super) async fn pulls_create(
        &self,
        repo: &RepoRef,
        new: &NewPullRequest,
    ) -> Result<PullRequest, ApiError> {
        let body = json!({
            "head": new.head,
            "base": new.base,
            "title": new.title,
            "body": new.body,
        });
        let created = self
            .http
            .post_json(&self.repo_api(repo, "/pulls"), &body)
            .await?
            .ok_or_else(|| ApiError::Decode {
                host: repo.host.clone(),
                detail: "creating the pull request returned nothing".into(),
            })?;
        let number = created["number"].as_u64().unwrap_or_default();
        // Read it back in the same shape as every other pull request.
        self.pulls_get(repo, number)
            .await?
            .ok_or_else(|| ApiError::NotFound {
                host: repo.host.clone(),
                path: format!("pull request #{number}, just created"),
            })
    }

    /// Merges now, or with `when_checks_succeed`, schedules the merge for
    /// when the base branch's required checks pass on the head. The forge
    /// honours `head_commit_id` only for a merge it does at once: a
    /// scheduled one merges whatever the head is when the checks pass.
    pub(super) async fn pulls_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
        when_checks_succeed: bool,
    ) -> Result<(), ApiError> {
        let key = if !self.is_forgejo() && self.at_least(GITEA_1_26) {
            "do"
        } else {
            "Do"
        };
        let mut body = Map::new();
        body.insert(key.into(), json!(style.as_forge()));
        // The forge refuses the merge if the head isn't this commit any more.
        body.insert("head_commit_id".into(), json!(head_sha));
        body.insert("delete_branch_after_merge".into(), json!(delete_branch));
        if when_checks_succeed {
            body.insert("merge_when_checks_succeed".into(), json!(true));
        }
        self.http
            .post_json(
                &self.repo_api(repo, &format!("/pulls/{number}/merge")),
                &Value::Object(body),
            )
            .await?;
        Ok(())
    }

    /// Cancels a scheduled merge. `false` when none was scheduled.
    pub(super) async fn pulls_cancel_scheduled_merge(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<bool, ApiError> {
        match self
            .http
            .delete(&self.repo_api(repo, &format!("/pulls/{number}/merge")))
            .await
        {
            Ok(()) => Ok(true),
            // The pull request was read just before, so a 404 is the
            // missing schedule.
            Err(ApiError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub(super) async fn ci_cancel(&self, repo: &RepoRef, run: &Run) -> Result<(), ApiError> {
        self.http
            .post_json(
                &self.repo_api(repo, &format!("/actions/runs/{}/cancel", run.id)),
                &json!({}),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn ci_rerun(
        &self,
        repo: &RepoRef,
        run: &Run,
        failed_only: bool,
    ) -> Result<Vec<String>, ApiError> {
        if !failed_only {
            self.http
                .post_json(
                    &self.repo_api(repo, &format!("/actions/runs/{}/rerun", run.id)),
                    &json!({}),
                )
                .await?;
            return Ok(vec![format!("run {} (every job)", run.number)]);
        }
        let failed = ci::failed_jobs(run).map_err(|message| ApiError::Rejected {
            host: repo.host.clone(),
            status: 0,
            message,
        })?;
        if !self.is_forgejo() {
            self.http
                .post_json(
                    &self.repo_api(repo, &format!("/actions/runs/{}/rerun-failed-jobs", run.id)),
                    &json!({}),
                )
                .await?;
            return Ok(failed.iter().map(|j| j.name.clone()).collect());
        }
        // Forgejo reruns one job at a time (and the jobs that need it).
        let mut rerun = Vec::new();
        for job in failed {
            let Some(id) = job.job_id else { continue };
            self.http
                .post_json(
                    &self.repo_api(repo, &format!("/actions/jobs/{id}/rerun")),
                    &json!({}),
                )
                .await?;
            rerun.push(job.name.clone());
        }
        Ok(rerun)
    }

    pub(super) async fn ci_dispatch(
        &self,
        repo: &RepoRef,
        workflow: &str,
        git_ref: &str,
        inputs: &Map<String, Value>,
    ) -> Result<Dispatched, ApiError> {
        let path = format!("/actions/workflows/{workflow}/dispatches");
        if self.is_forgejo() {
            let body = json!({ "ref": git_ref, "inputs": inputs, "return_run_info": true });
            let info = self
                .http
                .post_json(&self.repo_api(repo, &path), &body)
                .await?;
            return Ok(info.map_or_else(Dispatched::default, |v| Dispatched {
                run_id: v["id"].as_u64(),
                run_number: v["run_number"].as_u64(),
                url: None,
            }));
        }
        let body = json!({ "ref": git_ref, "inputs": inputs });
        if self.at_least(GITEA_1_26) {
            let info = self
                .http
                .post_json(
                    &self.repo_api(repo, &format!("{path}?return_run_details=true")),
                    &body,
                )
                .await?;
            let Some(v) = info else {
                return Ok(Dispatched::default());
            };
            let run_id = v["workflow_run_id"].as_u64();
            // The run's page names it by id from some Gitea version on, and
            // by number before, so the number is read from the run itself.
            // A read: the dispatch has already happened, so a failure here
            // leaves the number unknown rather than failing it.
            let run_number = match run_id {
                Some(id) => self
                    .http
                    .get_json::<ActionsRun>(&self.repo_api(repo, &format!("/actions/runs/{id}")))
                    .await
                    .inspect_err(|e| tracing::debug!("dispatched run {id} unreadable: {e}"))
                    .ok()
                    .map(|r| r.run_number)
                    .filter(|n| *n > 0),
                None => None,
            };
            return Ok(Dispatched {
                run_id,
                run_number,
                url: v["html_url"].as_str().map(str::to_owned),
            });
        }
        // Gitea 1.25 says nothing about the run it started.
        self.http
            .post_json(&self.repo_api(repo, &path), &body)
            .await?;
        Ok(Dispatched::default())
    }
}
