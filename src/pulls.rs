//! Pull requests in forge-neutral terms.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: PrState,
    pub draft: bool,
    pub author: String,
    /// The branch with the changes.
    pub head: String,
    pub head_sha: String,
    /// The branch it merges into.
    pub base: String,
    /// Whether the forge says it can merge without conflicts. `None` while
    /// the forge hasn't worked it out, or once merged.
    pub mergeable: Option<bool>,
    pub labels: Vec<String>,
    pub url: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    /// The commit the merge put on the base branch, once merged: the one
    /// whose CI to watch next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    /// The description, in `get` only, cut to a readable length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Which pull requests to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
    All,
}

impl StateFilter {
    pub fn as_query(self) -> &'static str {
        match self {
            StateFilter::Open => "open",
            StateFilter::Closed => "closed",
            StateFilter::All => "all",
        }
    }
}

pub const BODY_LIMIT: usize = 4_000;

/// `body` cut at a character boundary, with a marker when cut.
pub fn cut_body(body: &str) -> String {
    cut_text(body, BODY_LIMIT)
}

/// How a pull request is merged. Which styles a repository allows is a
/// repository setting; the forge refuses the others.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum MergeStyle {
    /// A merge commit.
    #[default]
    Merge,
    /// Rebase the commits onto the base branch, no merge commit.
    Rebase,
    /// Rebase, then a merge commit.
    RebaseMerge,
    /// One squashed commit.
    Squash,
    /// Only if the base can fast-forward to the head.
    FastForwardOnly,
}

impl MergeStyle {
    /// The name Gitea and Forgejo use.
    pub fn as_forge(self) -> &'static str {
        match self {
            MergeStyle::Merge => "merge",
            MergeStyle::Rebase => "rebase",
            MergeStyle::RebaseMerge => "rebase-merge",
            MergeStyle::Squash => "squash",
            MergeStyle::FastForwardOnly => "fast-forward-only",
        }
    }
}

/// A pull request to open.
#[derive(Debug, Clone)]
pub struct NewPullRequest {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
}

/// What a branch's protection says about status checks: whether merging
/// into it waits for them, and which. A scheduled merge waits only for
/// these, so without them the forge merges at once, CI or not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatusChecks {
    /// Merging needs the head's status checks to pass.
    pub required: bool,
    /// The checks (glob patterns) that must pass; empty means all of them.
    pub contexts: Vec<String>,
}

/// What a review says about the pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    RequestChanges,
    Comment,
    /// A draft only its author can see, not yet submitted.
    Pending,
    /// Not a review: someone was asked for one and hasn't given it.
    ReviewRequested,
}

impl ReviewState {
    /// Gitea's and Forgejo's names.
    pub fn parse(state: &str) -> Option<Self> {
        Some(match state.to_ascii_uppercase().as_str() {
            "APPROVED" => ReviewState::Approved,
            "REQUEST_CHANGES" => ReviewState::RequestChanges,
            "COMMENT" => ReviewState::Comment,
            "PENDING" => ReviewState::Pending,
            "REQUEST_REVIEW" => ReviewState::ReviewRequested,
            _ => return None,
        })
    }
}

/// A review: its verdict, its summary comment, and its comments on lines
/// of the diff.
#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub id: u64,
    pub author: String,
    pub state: ReviewState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    /// Given on an older head commit than the current one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

/// A comment on one line of the diff.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// `line` is in the old version of the file: the comment is on a
    /// removed line.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub old_side: bool,
    pub author: String,
    pub body: String,
    /// The end of the diff hunk the comment is on, ending with its line.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub diff_hunk: String,
    /// Who marked the conversation resolved, if anyone has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

/// Review bodies and comments are cut to this.
pub const REVIEW_TEXT_LIMIT: usize = 1_500;
/// Lines of diff hunk kept per comment: the commented line and the few
/// before it.
pub const HUNK_LINES: usize = 5;
/// Comments listed at most, over all reviews.
pub const MAX_COMMENTS: usize = 50;

/// `text` cut at a character boundary to `limit` bytes, with a marker when
/// cut.
pub fn cut_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… {} more characters]", &text[..end], text.len() - end)
}

/// The last `HUNK_LINES` lines of a diff hunk, which end at the commented
/// line.
pub fn cut_hunk(hunk: &str) -> String {
    let lines: Vec<&str> = hunk.trim_end_matches('\n').lines().collect();
    lines[lines.len().saturating_sub(HUNK_LINES)..].join("\n")
}

/// Where a pull request's reviews stand, and the reviews to read.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewReport {
    /// Reviewers whose latest verdict, not dismissed, is an approval.
    pub approved_by: Vec<String>,
    /// Reviewers whose latest verdict, not dismissed, asks for changes.
    pub changes_requested_by: Vec<String>,
    /// Reviewers asked for a review who haven't given one since.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub review_requested: Vec<String>,
    /// Line comments not resolved, on reviews not dismissed.
    pub open_comments: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<Review>,
    /// What was left out, and how to see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted: Option<String>,
}

/// Sorts out `reviews`: each reviewer's standing verdict, and the reviews
/// still worth reading. Unless `include_resolved`, dismissed reviews,
/// resolved comments, and reviews with nothing left to read (an approval
/// without a comment, say) are left out and counted.
pub fn review_report(mut reviews: Vec<Review>, include_resolved: bool) -> ReviewReport {
    reviews.sort_by_key(|r| r.id);
    // Drafts are their author's business.
    reviews.retain(|r| r.state != ReviewState::Pending);
    let standing = standings(&reviews);
    let who = |state: ReviewState| -> Vec<String> {
        standing
            .iter()
            .filter(|(_, s)| *s == state)
            .map(|(w, _)| w.clone())
            .collect()
    };
    let open_comments = reviews
        .iter()
        .filter(|r| !r.dismissed)
        .flat_map(|r| &r.comments)
        .filter(|c| c.resolved_by.is_none())
        .count();
    let (reviews, left_out) = readable(reviews, include_resolved);
    ReviewReport {
        approved_by: who(ReviewState::Approved),
        changes_requested_by: who(ReviewState::RequestChanges),
        review_requested: who(ReviewState::ReviewRequested),
        open_comments,
        reviews,
        omitted: left_out.note(),
    }
}

/// Where each reviewer stands: their latest verdict or request. A plain
/// comment answers a request, but doesn't undo a verdict.
fn standings(reviews: &[Review]) -> Vec<(String, ReviewState)> {
    let mut standing: Vec<(String, ReviewState)> = Vec::new();
    for r in reviews.iter().filter(|r| !r.dismissed) {
        match standing.iter_mut().find(|(who, _)| *who == r.author) {
            Some(entry) if r.state == ReviewState::Comment => {
                if entry.1 == ReviewState::ReviewRequested {
                    entry.1 = ReviewState::Comment;
                }
            }
            Some(entry) => entry.1 = r.state,
            None if r.state == ReviewState::Comment => {}
            None => standing.push((r.author.clone(), r.state)),
        }
    }
    standing
}

/// The reviews worth reading, with at most `MAX_COMMENTS` line comments
/// between them, and a count of what was left out.
fn readable(reviews: Vec<Review>, include_resolved: bool) -> (Vec<Review>, LeftOut) {
    let mut left_out = LeftOut::default();
    let kept: Vec<Review> = reviews
        .into_iter()
        .filter(|r| r.state != ReviewState::ReviewRequested)
        .filter_map(|mut r| (include_resolved || left_out.keep(&mut r)).then_some(r))
        .collect();
    let shown = left_out.cap(kept);
    (shown, left_out)
}

/// What a review report leaves out.
#[derive(Debug, Default)]
struct LeftOut {
    dismissed: usize,
    resolved: usize,
    /// Reviews with nothing left to read.
    empty: usize,
    /// Comments past `MAX_COMMENTS`.
    over: usize,
}

impl LeftOut {
    /// Drops `review`'s resolved comments, and says whether anything in it
    /// is still worth reading. Counts whatever it leaves out.
    fn keep(&mut self, review: &mut Review) -> bool {
        if review.dismissed {
            self.dismissed += 1;
            return false;
        }
        let before = review.comments.len();
        review.comments.retain(|c| c.resolved_by.is_none());
        self.resolved += before - review.comments.len();
        if review.body.is_none()
            && review.comments.is_empty()
            && review.state != ReviewState::RequestChanges
        {
            self.empty += 1;
            return false;
        }
        true
    }

    /// Keeps the first `MAX_COMMENTS` line comments across `reviews`,
    /// counting the rest.
    fn cap(&mut self, mut reviews: Vec<Review>) -> Vec<Review> {
        let mut room = MAX_COMMENTS;
        for r in &mut reviews {
            if r.comments.len() > room {
                self.over += r.comments.len() - room;
                r.comments.truncate(room);
            }
            room -= r.comments.len();
        }
        reviews
    }

    /// "2 resolved comments: pass include_resolved=true to list them; 3
    /// more comments past the first 50", or `None` when nothing was left out.
    fn note(&self) -> Option<String> {
        let counted = [
            (self.dismissed, "dismissed review", "dismissed reviews"),
            (self.resolved, "resolved comment", "resolved comments"),
            (
                self.empty,
                "review with nothing to read",
                "reviews with nothing to read",
            ),
        ];
        let hidden: Vec<String> = counted
            .iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
            .collect();
        let mut omitted = Vec::new();
        if !hidden.is_empty() {
            omitted.push(format!(
                "{}: pass include_resolved=true to list them",
                hidden.join(", ")
            ));
        }
        if self.over > 0 {
            omitted.push(format!(
                "{} more comments past the first {MAX_COMMENTS}",
                self.over
            ));
        }
        (!omitted.is_empty()).then(|| omitted.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(id: u64, author: &str, state: ReviewState) -> Review {
        Review {
            id,
            author: author.into(),
            state,
            body: None,
            submitted_at: None,
            stale: false,
            dismissed: false,
            comments: vec![],
        }
    }

    fn comment(id: u64, resolved_by: Option<&str>) -> ReviewComment {
        ReviewComment {
            id,
            path: "src/lib.rs".into(),
            line: Some(id),
            old_side: false,
            author: "alice".into(),
            body: "why?".into(),
            diff_hunk: String::new(),
            resolved_by: resolved_by.map(str::to_owned),
        }
    }

    #[test]
    fn each_reviewer_stands_by_their_latest_verdict() {
        let mut dismissed = review(5, "carol", ReviewState::RequestChanges);
        dismissed.dismissed = true;
        let report = review_report(
            vec![
                review(4, "alice", ReviewState::Approved),
                review(1, "alice", ReviewState::RequestChanges),
                // A comment after an approval doesn't take it back.
                review(6, "alice", ReviewState::Comment),
                review(2, "bob", ReviewState::RequestChanges),
                dismissed,
                review(3, "dave", ReviewState::ReviewRequested),
                review(7, "erin", ReviewState::ReviewRequested),
                // ... but answers a request.
                review(8, "erin", ReviewState::Comment),
                review(9, "frank", ReviewState::Pending),
            ],
            false,
        );
        assert_eq!(report.approved_by, ["alice"]);
        assert_eq!(report.changes_requested_by, ["bob"]);
        assert_eq!(report.review_requested, ["dave"]);
    }

    #[test]
    fn by_default_only_what_is_open_is_listed() {
        let mut changes = review(1, "alice", ReviewState::RequestChanges);
        changes.comments = vec![comment(10, None), comment(11, Some("dev"))];
        let mut all_resolved = review(2, "bob", ReviewState::Comment);
        all_resolved.comments = vec![comment(12, Some("dev"))];
        let mut dismissed = review(3, "carol", ReviewState::RequestChanges);
        dismissed.dismissed = true;
        dismissed.comments = vec![comment(13, None)];
        let approval = review(4, "dave", ReviewState::Approved);
        let mut said_something = review(5, "erin", ReviewState::Approved);
        said_something.body = Some("Nice.".into());
        let all = vec![changes, all_resolved, dismissed, approval, said_something];

        let open = review_report(all.clone(), false);
        assert_eq!(open.open_comments, 1);
        let ids: Vec<u64> = open.reviews.iter().map(|r| r.id).collect();
        assert_eq!(ids, [1, 5]);
        assert_eq!(open.reviews[0].comments.len(), 1);
        assert_eq!(
            open.omitted.as_deref(),
            Some(
                "1 dismissed review, 2 resolved comments, 2 reviews with nothing to read: pass \
include_resolved=true to list them"
            )
        );

        let everything = review_report(all, true);
        assert_eq!(everything.open_comments, 1);
        assert_eq!(everything.reviews.len(), 5);
        assert_eq!(everything.omitted, None);
    }

    #[test]
    fn long_threads_are_capped() {
        let mut busy = review(1, "alice", ReviewState::Comment);
        busy.comments = (0..MAX_COMMENTS as u64 + 7)
            .map(|i| comment(i, None))
            .collect();
        let report = review_report(vec![busy], false);
        assert_eq!(report.open_comments, MAX_COMMENTS + 7);
        assert_eq!(report.reviews[0].comments.len(), MAX_COMMENTS);
        assert_eq!(
            report.omitted.as_deref(),
            Some("7 more comments past the first 50")
        );
    }

    #[test]
    fn hunks_keep_their_last_lines() {
        assert_eq!(cut_hunk("@@ -1 +1 @@\n+a\n"), "@@ -1 +1 @@\n+a");
        let long = "@@ -1,9 +1,9 @@\n 1\n 2\n 3\n 4\n 5\n 6\n+7";
        assert_eq!(cut_hunk(long), " 3\n 4\n 5\n 6\n+7");
        assert_eq!(cut_hunk(""), "");
        assert!(cut_text(&"é".repeat(1000), 1_501).ends_with("more characters]"));
    }
}
