//! Writes on GitHub: pull requests (merged now or once checks pass), and
//! cancelling, rerunning and dispatching Actions runs. Each is one request,
//! sent once.
//!
//! | | endpoint |
//! |---|---|
//! | open a pull request | `POST pulls` |
//! | merge | `PUT pulls/{n}/merge` with `merge_method` and the head `sha` it must still be |
//! | delete the branch after | `DELETE git/refs/heads/{branch}`, a request of its own |
//! | merge once checks pass | GraphQL `enablePullRequestAutoMerge`, with `expectedHeadOid` |
//! | cancel that | GraphQL `disablePullRequestAutoMerge` |
//! | cancel a run | `POST actions/runs/{id}/cancel` |
//! | rerun a run, or its failed jobs | `POST actions/runs/{id}/rerun`, `.../rerun-failed-jobs` |
//! | dispatch | `POST actions/workflows/{file}/dispatches`, with the run's details from GHES 3.21 |

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::Github;
use crate::ci::{self, Dispatched, Run};
use crate::forge::ApiError;
use crate::forge::shapes::PrBody;
use crate::pulls::{MergeStyle, NewPullRequest, PullRequest};
use crate::remote::RepoRef;
use crate::version::GHES_3_21;

const ENABLE_AUTO_MERGE: &str = "mutation($id: ID!, $method: PullRequestMergeMethod!, $head: GitObjectID!) {
  enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method, expectedHeadOid: $head}) {
    pullRequest { number }
  }
}";

const DISABLE_AUTO_MERGE: &str = "mutation($id: ID!) {
  disablePullRequestAutoMerge(input: {pullRequestId: $id}) { pullRequest { number } }
}";

/// GitHub's three merge methods, as REST names them. The other styles
/// aren't GitHub's; refusing them here means nothing is sent.
fn merge_method(repo: &RepoRef, style: MergeStyle) -> Result<&'static str, ApiError> {
    match style {
        MergeStyle::Merge => Ok("merge"),
        MergeStyle::Squash => Ok("squash"),
        MergeStyle::Rebase => Ok("rebase"),
        MergeStyle::RebaseMerge | MergeStyle::FastForwardOnly => Err(refused(
            repo,
            format!(
                "GitHub merges with merge, squash or rebase; it has no {}",
                style.as_forge()
            ),
        )),
    }
}

/// A refusal decided here, before anything was sent.
fn refused(repo: &RepoRef, message: String) -> ApiError {
    ApiError::Rejected {
        host: repo.host.clone(),
        status: 0,
        message,
    }
}

#[derive(Deserialize)]
struct MergeSettings {
    /// Sent only to callers who may change the setting.
    delete_branch_on_merge: Option<bool>,
}

impl Github {
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
        let created: PrBody = serde_json::from_value(created).map_err(|e| ApiError::Decode {
            host: repo.host.clone(),
            detail: e.to_string(),
        })?;
        Ok(created.into_pull_request(true))
    }

    /// The pull request, which the caller has just read: gone now is an
    /// error.
    async fn pr_now(&self, repo: &RepoRef, number: u64) -> Result<PrBody, ApiError> {
        self.pulls_get_raw(repo, number)
            .await?
            .ok_or_else(|| ApiError::NotFound {
                host: repo.host.clone(),
                path: format!("pull request #{number}"),
            })
    }

    /// Merges, only if the head is still `head_sha`, then deletes the head
    /// branch if asked and it's in this repository. The merge has happened
    /// by then, so a branch that won't go is left for the caller to see,
    /// not reported as a failed merge.
    pub(super) async fn pulls_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        let method = merge_method(repo, style)?;
        let pr = if delete_branch {
            Some(self.pr_now(repo, number).await?)
        } else {
            None
        };
        self.http
            .put_json(
                &self.repo_api(repo, &format!("/pulls/{number}/merge")),
                &json!({ "merge_method": method, "sha": head_sha }),
            )
            .await?;
        let Some(pr) = pr else { return Ok(()) };
        let same_repo = pr.head.repo.as_ref().is_some_and(|r| {
            r.full_name
                .eq_ignore_ascii_case(&format!("{}/{}", repo.owner, repo.name))
        });
        if same_repo
            && let Err(e) = self
                .http
                .delete(&self.repo_api(repo, &format!("/git/refs/heads/{}", pr.head.git_ref)))
                .await
        {
            tracing::warn!(branch = %pr.head.git_ref, error = %e, "merged, but the branch wasn't deleted");
        }
        Ok(())
    }

    /// Sets the pull request to merge once the base branch's requirements
    /// are met, only while its head is `head_sha`. GitHub can't be told to
    /// delete the branch afterwards: only the repository's own setting
    /// does that.
    pub(super) async fn pulls_schedule_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        let method = merge_method(repo, style)?.to_ascii_uppercase();
        if delete_branch {
            let settings: MergeSettings = self.http.get_json(&self.repo_api(repo, "")).await?;
            if settings.delete_branch_on_merge != Some(true) {
                return Err(refused(
                    repo,
                    "GitHub can't delete the branch after an automatic merge unless the repository's \
                     \"Automatically delete head branches\" setting is on: schedule it without delete_branch"
                        .into(),
                ));
            }
        }
        let pr = self.pr_now(repo, number).await?;
        let id = pr.node_id.ok_or_else(|| ApiError::Decode {
            host: repo.host.clone(),
            detail: format!("pull request #{number} came without its node_id"),
        })?;
        self.http
            .graphql(
                &self.graphql,
                ENABLE_AUTO_MERGE,
                json!({ "id": id, "method": method, "head": head_sha }),
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
        let pr = self.pr_now(repo, number).await?;
        let (Some(id), Some(Value::Object(_))) = (pr.node_id, pr.auto_merge) else {
            return Ok(false);
        };
        self.http
            .graphql(&self.graphql, DISABLE_AUTO_MERGE, json!({ "id": id }))
            .await?;
        Ok(true)
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
        let failed = ci::failed_jobs(run).map_err(|message| refused(repo, message))?;
        self.http
            .post_json(
                &self.repo_api(repo, &format!("/actions/runs/{}/rerun-failed-jobs", run.id)),
                &json!({}),
            )
            .await?;
        Ok(failed.iter().map(|j| j.name.clone()).collect())
    }

    pub(super) async fn ci_dispatch(
        &self,
        repo: &RepoRef,
        workflow: &str,
        git_ref: &str,
        inputs: &Map<String, Value>,
    ) -> Result<Dispatched, ApiError> {
        let url = self.repo_api(repo, &format!("/actions/workflows/{workflow}/dispatches"));
        let mut body = json!({ "ref": git_ref, "inputs": inputs });
        if self.at_least(GHES_3_21) {
            body["return_run_details"] = json!(true);
        }
        let reply = self.http.post_json(&url, &body).await?;
        Ok(reply.map_or_else(Dispatched::default, |v| {
            let id = v["workflow_run_id"].as_u64();
            Dispatched {
                run_id: id,
                // GitHub's runs are known by their id.
                run_number: id,
                url: v["html_url"].as_str().map(str::to_owned),
            }
        }))
    }
}
