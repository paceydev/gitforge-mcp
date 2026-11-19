//! Pull requests on GitHub: listing and reading them, their reviews, and
//! what the base branch requires of status checks.
//!
//! Reviews differ from Gitea's:
//!
//! - States are `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`
//!   and `PENDING`. A dismissed review keeps no record of what it said.
//! - Line comments come from one list for the whole pull request, each
//!   naming its review.
//! - Requested reviewers are a separate list, of who is asked now: someone
//!   re-requested after reviewing is listed again.
//! - Whether a comment thread is resolved is only in GraphQL.
//! - A review is stale when it was given on another commit than the head.
//!
//! Required checks come from classic branch protection and from rulesets,
//! which apply together.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::json;

use super::Github;
use crate::forge::ApiError;
use crate::forge::shapes::{PrBody, PrUser, nonzero};
use crate::pulls::{
    self, PullRequest, Review, ReviewComment, ReviewState, StateFilter, StatusChecks,
};
use crate::remote::RepoRef;

/// GitHub's largest page.
const PAGE_SIZE: usize = 100;
/// Pages of reviews, and of line comments, read for one pull request.
const REVIEW_PAGES: u32 = 5;
/// Review threads, and comments per thread, read for their resolution.
const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes { isResolved resolvedBy { login } comments(first: 100) { nodes { databaseId } } }
      }
    }
  }
}";

impl Github {
    pub(super) async fn pulls_list(
        &self,
        repo: &RepoRef,
        state: StateFilter,
        limit: u32,
    ) -> Result<Vec<PullRequest>, ApiError> {
        let url = self.repo_api(
            repo,
            &format!(
                "/pulls?state={}&sort=updated&direction=desc&per_page={}&page=1",
                state.as_query(),
                limit.clamp(1, PAGE_SIZE as u32)
            ),
        );
        let list: Vec<PrBody> = self.http.get_json(&url).await?;
        Ok(list
            .into_iter()
            .map(|p| p.into_pull_request(false))
            .collect())
    }

    pub(super) async fn pulls_get_raw(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<PrBody>, ApiError> {
        self.http
            .get_json_opt(&self.repo_api(repo, &format!("/pulls/{number}")))
            .await
    }

    /// Every review of pull request `number`, with its line comments, and
    /// a review request for each reviewer asked for one now.
    pub(super) async fn pulls_reviews(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<Review>, ApiError> {
        let pr = self
            .pulls_get_raw(repo, number)
            .await?
            .ok_or_else(|| ApiError::NotFound {
                host: repo.host.clone(),
                path: format!("pull request #{number}"),
            })?;
        let bodies: Vec<ReviewBody> = self
            .http
            .paged(
                REVIEW_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/pulls/{number}/reviews?per_page={PAGE_SIZE}&page={page}"),
                    )
                },
                |list: Vec<ReviewBody>| list,
            )
            .await?;
        let comments: Vec<CommentBody> = self
            .http
            .paged(
                REVIEW_PAGES,
                PAGE_SIZE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/pulls/{number}/comments?per_page={PAGE_SIZE}&page={page}"),
                    )
                },
                |list: Vec<CommentBody>| list,
            )
            .await?;
        let resolved = if comments.is_empty() {
            HashMap::new()
        } else {
            self.resolved_comments(repo, number).await?
        };
        let requested: Option<Requested> = self
            .http
            .get_json_opt(&self.repo_api(repo, &format!("/pulls/{number}/requested_reviewers")))
            .await?;
        Ok(assemble(
            bodies,
            comments,
            &resolved,
            requested.unwrap_or_default(),
            &pr.head.sha,
        ))
    }

    /// Who resolved each resolved comment, by comment id.
    async fn resolved_comments(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<HashMap<u64, String>, ApiError> {
        let data = self
            .http
            .graphql(
                &self.graphql,
                THREADS_QUERY,
                json!({ "owner": repo.owner, "name": repo.name, "number": number }),
            )
            .await?;
        let threads: Threads =
            serde_json::from_value(data["repository"]["pullRequest"]["reviewThreads"].clone())
                .unwrap_or_default();
        Ok(resolutions(threads))
    }

    /// What merging into `branch` requires of status checks: classic
    /// protection's required checks and rulesets' together. Both read with
    /// read access. `None` if there's no such branch.
    pub(super) async fn pulls_status_checks(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<StatusChecks>, ApiError> {
        let Some(body) = self.branch(repo, branch).await? else {
            return Ok(None);
        };
        let rules: Vec<Rule> = self
            .http
            .get_json(&self.repo_api(
                repo,
                &format!("/rules/branches/{branch}?per_page={PAGE_SIZE}"),
            ))
            .await?;
        let mut contexts: Vec<String> = body
            .protection
            .filter(|p| p.enabled)
            .and_then(|p| p.required_status_checks)
            .filter(|c| c.enforcement_level != "off")
            .map(|c| c.contexts)
            .unwrap_or_default();
        for rule in rules
            .into_iter()
            .filter(|r| r.kind == "required_status_checks")
        {
            for check in rule.parameters.required_status_checks {
                if !contexts.contains(&check.context) {
                    contexts.push(check.context);
                }
            }
        }
        // GitHub has no "every check": protection that names none
        // requires none.
        Ok(Some(StatusChecks {
            required: !contexts.is_empty(),
            contexts,
        }))
    }
}

#[derive(Deserialize)]
struct ReviewBody {
    id: u64,
    user: Option<PrUser>,
    #[serde(default)]
    state: String,
    body: Option<String>,
    submitted_at: Option<String>,
    commit_id: Option<String>,
}

#[derive(Deserialize)]
struct CommentBody {
    id: u64,
    pull_request_review_id: Option<u64>,
    user: Option<PrUser>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    path: String,
    /// The line in the current diff; `null` once outdated.
    line: Option<u64>,
    original_line: Option<u64>,
    /// `LEFT` for a removed line, `RIGHT` otherwise.
    side: Option<String>,
    original_side: Option<String>,
    #[serde(default)]
    diff_hunk: String,
}

#[derive(Deserialize, Default)]
struct Requested {
    #[serde(default)]
    users: Vec<PrUser>,
    #[serde(default)]
    teams: Vec<TeamBody>,
}

#[derive(Deserialize)]
struct TeamBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
}

#[derive(Deserialize, Default)]
struct Threads {
    #[serde(default)]
    nodes: Vec<Thread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    #[serde(default)]
    is_resolved: bool,
    resolved_by: Option<PrUser>,
    #[serde(default)]
    comments: ThreadComments,
}

#[derive(Deserialize, Default)]
struct ThreadComments {
    #[serde(default)]
    nodes: Vec<ThreadComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadComment {
    database_id: Option<u64>,
}

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    parameters: RuleParameters,
}

#[derive(Deserialize, Default)]
struct RuleParameters {
    #[serde(default)]
    required_status_checks: Vec<RuleCheck>,
}

#[derive(Deserialize)]
struct RuleCheck {
    context: String,
}

/// Who resolved each comment in a resolved thread. A resolver GitHub won't
/// name (a deleted account) is "someone".
fn resolutions(threads: Threads) -> HashMap<u64, String> {
    let mut resolved = HashMap::new();
    for thread in threads.nodes.into_iter().filter(|t| t.is_resolved) {
        let by = thread
            .resolved_by
            .map_or_else(|| "someone".to_owned(), |u| u.login);
        for id in thread
            .comments
            .nodes
            .into_iter()
            .filter_map(|c| c.database_id)
        {
            resolved.insert(id, by.clone());
        }
    }
    resolved
}

/// GitHub's review states in the forge-neutral ones. A dismissed review's
/// verdict is gone, so it reads as a comment, dismissed.
fn review_state(state: &str) -> Option<(ReviewState, bool)> {
    Some(match state.to_ascii_uppercase().as_str() {
        "APPROVED" => (ReviewState::Approved, false),
        "CHANGES_REQUESTED" => (ReviewState::RequestChanges, false),
        "COMMENTED" => (ReviewState::Comment, false),
        "DISMISSED" => (ReviewState::Comment, true),
        "PENDING" => (ReviewState::Pending, false),
        _ => return None,
    })
}

impl CommentBody {
    fn into_comment(self, resolved: &HashMap<u64, String>) -> ReviewComment {
        let (line, side) = match self.line {
            Some(line) => (Some(line), self.side),
            None => (self.original_line, self.original_side.or(self.side)),
        };
        ReviewComment {
            id: self.id,
            path: self.path,
            line,
            old_side: side.as_deref() == Some("LEFT"),
            author: self.user.map(|u| u.login).unwrap_or_default(),
            body: pulls::cut_text(self.body.trim(), pulls::REVIEW_TEXT_LIMIT),
            diff_hunk: pulls::cut_hunk(&self.diff_hunk),
            resolved_by: resolved.get(&self.id).cloned(),
        }
    }
}

/// The reviews, each with its comments, then a review request per reviewer
/// asked now: numbered after every review, since a request standing now is
/// newer than any review.
fn assemble(
    bodies: Vec<ReviewBody>,
    comments: Vec<CommentBody>,
    resolved: &HashMap<u64, String>,
    requested: Requested,
    head_sha: &str,
) -> Vec<Review> {
    let mut by_review: HashMap<u64, Vec<ReviewComment>> = HashMap::new();
    for c in comments {
        if let Some(review) = c.pull_request_review_id {
            by_review
                .entry(review)
                .or_default()
                .push(c.into_comment(resolved));
        }
    }
    let mut reviews: Vec<Review> = bodies
        .into_iter()
        .filter_map(|b| {
            let (state, dismissed) = review_state(&b.state)?;
            Some(Review {
                id: b.id,
                author: b.user.map(|u| u.login).unwrap_or_default(),
                state,
                body: b
                    .body
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty())
                    .map(|t| pulls::cut_text(&t, pulls::REVIEW_TEXT_LIMIT)),
                submitted_at: nonzero(b.submitted_at),
                stale: b
                    .commit_id
                    .is_some_and(|c| !head_sha.is_empty() && !c.eq_ignore_ascii_case(head_sha)),
                dismissed,
                comments: by_review.remove(&b.id).unwrap_or_default(),
            })
        })
        .collect();
    let after = reviews.iter().map(|r| r.id).max().unwrap_or(0);
    let asked = requested.users.into_iter().map(|u| u.login).chain(
        requested
            .teams
            .into_iter()
            .map(|t| if t.name.is_empty() { t.slug } else { t.name }),
    );
    for (offset, who) in (1..).zip(asked) {
        reviews.push(Review {
            id: after + offset,
            author: who,
            state: ReviewState::ReviewRequested,
            body: None,
            submitted_at: None,
            stale: false,
            dismissed: false,
            comments: Vec::new(),
        });
    }
    reviews
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviews_from(
        reviews: serde_json::Value,
        comments: serde_json::Value,
        threads: serde_json::Value,
        requested: serde_json::Value,
    ) -> Vec<Review> {
        let threads: Threads = serde_json::from_value(threads).unwrap();
        assemble(
            serde_json::from_value(reviews).unwrap(),
            serde_json::from_value(comments).unwrap(),
            &resolutions(threads),
            serde_json::from_value(requested).unwrap(),
            "head",
        )
    }

    #[test]
    fn github_reviews_comments_and_requests() {
        let reviews = reviews_from(
            serde_json::json!([
                {"id": 10, "user": {"login": "alice"}, "state": "CHANGES_REQUESTED",
                 "body": " Two things. ", "submitted_at": "2026-11-09T10:00:00Z", "commit_id": "old"},
                {"id": 11, "user": {"login": "bob"}, "state": "DISMISSED", "body": "",
                 "commit_id": "head"},
                {"id": 12, "user": {"login": "carol"}, "state": "APPROVED", "body": "",
                 "commit_id": "head"},
                {"id": 13, "user": {"login": "dave"}, "state": "SOMETHING_NEW"}
            ]),
            serde_json::json!([
                {"id": 40, "pull_request_review_id": 10, "user": {"login": "alice"},
                 "body": "Why this line?", "path": "src/lib.rs", "line": 7, "side": "RIGHT",
                 "diff_hunk": "@@ -1,2 +1,3 @@\n a\n+b"},
                {"id": 41, "pull_request_review_id": 10, "user": {"login": "alice"},
                 "body": "Keep this.", "path": "src/old.rs", "line": null, "original_line": 4,
                 "side": "LEFT", "diff_hunk": "@@ -3,2 +3,1 @@\n-fn old() {}"}
            ]),
            serde_json::json!({"nodes": [
                {"isResolved": true, "resolvedBy": {"login": "dev"},
                 "comments": {"nodes": [{"databaseId": 41}]}},
                {"isResolved": false, "resolvedBy": null,
                 "comments": {"nodes": [{"databaseId": 40}]}}
            ]}),
            serde_json::json!({"users": [{"login": "carol"}], "teams": [{"name": "Maintainers", "slug": "maintainers"}]}),
        );
        assert_eq!(reviews.len(), 5);
        let alice = &reviews[0];
        assert_eq!(alice.state, ReviewState::RequestChanges);
        assert!(alice.stale);
        assert_eq!(alice.body.as_deref(), Some("Two things."));
        assert_eq!(
            (alice.comments[0].line, alice.comments[0].old_side),
            (Some(7), false)
        );
        assert_eq!(alice.comments[0].resolved_by, None);
        assert_eq!(
            (alice.comments[1].line, alice.comments[1].old_side),
            (Some(4), true)
        );
        assert_eq!(alice.comments[1].resolved_by.as_deref(), Some("dev"));
        assert!(reviews[1].dismissed);
        assert!(!reviews[2].stale);
        // Carol approved, and has been asked again since.
        assert_eq!(
            (reviews[3].author.as_str(), reviews[3].state, reviews[3].id),
            ("carol", ReviewState::ReviewRequested, 13)
        );
        assert_eq!(reviews[4].author, "Maintainers");

        let report = pulls::review_report(reviews, false);
        assert_eq!(report.changes_requested_by, ["alice"]);
        assert!(report.approved_by.is_empty());
        assert_eq!(report.review_requested, ["carol", "Maintainers"]);
        assert_eq!(report.open_comments, 1);
    }
}
