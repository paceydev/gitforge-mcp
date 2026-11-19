//! Forge drivers behind one trait.
//!
//! The trait speaks in forge-neutral terms (a server, a user, a repository
//! and what the caller may do there), and GitHub implements it alongside
//! Gitea and Forgejo from the start, so the shape is not Gitea's with GitHub
//! bolted on later.

pub mod gitea;
pub mod github;
pub mod http;
pub(crate) mod shapes;

use std::future::Future;

use url::Url;

pub use http::ApiError;

use crate::auth::Credential;
use crate::ci::{Dispatched, Job, Run, RunsOf};
use crate::pulls::{MergeStyle, NewPullRequest, PullRequest, Review, StateFilter, StatusChecks};
use crate::remote::RepoRef;
use crate::runners::{self, Runner, RunnerReport, Scope, ScopeOutcome};
use crate::settings::{Delivery, Done, Item, Op};
pub use crate::site::{Permissions, RepoInfo, ServerInfo, User};
use crate::version::ForgeKind;

/// One forge, reached with one credential.
pub trait Forge: Send + Sync {
    fn server(&self) -> &ServerInfo;

    /// The account the credential belongs to.
    fn whoami(&self) -> impl Future<Output = Result<User, ApiError>> + Send;

    fn repository(&self, repo: &RepoRef)
    -> impl Future<Output = Result<RepoInfo, ApiError>> + Send;

    /// Every CI run for a commit (full 40-character sha), newest first, each
    /// with its jobs.
    fn runs_for_commit(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> impl Future<Output = Result<Vec<Run>, ApiError>> + Send;

    /// The run people know as number `number`, with its jobs.
    fn run_by_number(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> impl Future<Output = Result<Option<Run>, ApiError>> + Send;

    /// Recent runs of a workflow for an event on a branch, newest first,
    /// without their jobs: the history estimates come from. Reads at most
    /// `pages` pages of the run list, and with `after`, stops after the
    /// first page that reaches a run with that id or a lower one (ids grow
    /// over time on every forge), so a refresh reads only what's new.
    fn recent_runs(
        &self,
        repo: &RepoRef,
        of: RunsOf<'_>,
        pages: u32,
        after: Option<u64>,
    ) -> impl Future<Output = Result<Vec<Run>, ApiError>> + Send;

    /// Runs of CI outside the forge for a commit, from its commit statuses
    /// (see `ci::external_runs`).
    fn external_runs(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> impl Future<Output = Result<Vec<Run>, ApiError>> + Send;

    /// Whether the repository has ever had a CI run.
    fn has_runs(&self, repo: &RepoRef) -> impl Future<Output = Result<bool, ApiError>> + Send;

    /// The jobs of `run`, as `runs_for_commit` would list them.
    fn jobs_of(
        &self,
        repo: &RepoRef,
        run: &Run,
    ) -> impl Future<Output = Result<Vec<Job>, ApiError>> + Send;

    /// The commit a branch points at on the forge.
    fn branch_head(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> + Send;

    /// A job's log, or its last `tail_bytes` where the forge can serve just
    /// the end. Returns the text and whether it is only the end.
    fn job_log(
        &self,
        repo: &RepoRef,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> impl Future<Output = Result<(String, bool), ApiError>> + Send;

    /// Runners in every scope the credential can see, and the jobs waiting
    /// for one.
    fn runners(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> impl Future<Output = Result<RunnerReport, ApiError>> + Send;

    fn pull_requests(
        &self,
        repo: &RepoRef,
        state: StateFilter,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<PullRequest>, ApiError>> + Send;

    fn pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> impl Future<Output = Result<Option<PullRequest>, ApiError>> + Send;

    /// Every review of a pull request, with its line comments.
    fn pull_request_reviews(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> impl Future<Output = Result<Vec<Review>, ApiError>> + Send;

    /// What merging into `branch` requires of status checks. `None` if the
    /// branch doesn't exist.
    fn status_checks(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> impl Future<Output = Result<Option<StatusChecks>, ApiError>> + Send;

    // Writes. Callers check the capability first; a driver may assume the
    // forge version supports the call.

    fn create_pull_request(
        &self,
        repo: &RepoRef,
        new: &NewPullRequest,
    ) -> impl Future<Output = Result<PullRequest, ApiError>> + Send;

    /// Merges only if the head is still `head_sha`: the forge refuses if the
    /// branch moved after the caller looked at it.
    fn merge_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// Schedules the merge for when the base branch's required checks pass
    /// on the head. The forge merges whatever the head is by then; it
    /// checks `head_sha` only if it merges at once.
    fn schedule_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// Cancels a scheduled merge; `false` if none was scheduled.
    fn cancel_scheduled_merge(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send;

    fn cancel_run(
        &self,
        repo: &RepoRef,
        run: &Run,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// Reruns a whole run, or only its failed and cancelled jobs. Returns
    /// what was rerun, in words.
    fn rerun(
        &self,
        repo: &RepoRef,
        run: &Run,
        failed_only: bool,
    ) -> impl Future<Output = Result<Vec<String>, ApiError>> + Send;

    fn dispatch(
        &self,
        repo: &RepoRef,
        workflow: &str,
        git_ref: &str,
        inputs: &serde_json::Map<String, serde_json::Value>,
    ) -> impl Future<Output = Result<Dispatched, ApiError>> + Send;

    // Repository settings, under the names in `settings::fields`. Reading
    // them needs admin access; callers check the capability first.

    /// General and merge settings.
    fn repo_settings(
        &self,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<serde_json::Map<String, serde_json::Value>, ApiError>> + Send;

    fn branch_rules(
        &self,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<Vec<Item>, ApiError>> + Send;

    /// Webhooks, without their secrets.
    fn webhooks(&self, repo: &RepoRef) -> impl Future<Output = Result<Vec<Item>, ApiError>> + Send;

    /// Runs one settings operation. Sent once, never retried.
    fn apply_setting(
        &self,
        repo: &RepoRef,
        op: &Op,
    ) -> impl Future<Output = Result<Done, ApiError>> + Send;

    /// Asks the forge to send the webhook a test delivery.
    fn test_webhook(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// A webhook's recent deliveries, newest first.
    fn webhook_deliveries(
        &self,
        repo: &RepoRef,
        id: u64,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<Delivery>, ApiError>> + Send;
}

/// Calls the same method on whichever driver this is.
macro_rules! delegate {
    ($self:ident, $f:ident => $call:expr) => {
        match $self {
            AnyForge::Gitea($f) => $call.await,
            AnyForge::Github($f) => $call.await,
        }
    };
}

/// A driver for whichever forge a host turned out to be.
pub enum AnyForge {
    Gitea(gitea::GiteaFamily),
    Github(github::Github),
}

impl Forge for AnyForge {
    fn server(&self) -> &ServerInfo {
        match self {
            AnyForge::Gitea(f) => f.server(),
            AnyForge::Github(f) => f.server(),
        }
    }

    async fn whoami(&self) -> Result<User, ApiError> {
        delegate!(self, f => f.whoami())
    }

    async fn repository(&self, repo: &RepoRef) -> Result<RepoInfo, ApiError> {
        delegate!(self, f => f.repository(repo))
    }

    async fn runs_for_commit(&self, repo: &RepoRef, sha: &str) -> Result<Vec<Run>, ApiError> {
        delegate!(self, f => f.runs_for_commit(repo, sha))
    }

    async fn run_by_number(&self, repo: &RepoRef, number: u64) -> Result<Option<Run>, ApiError> {
        delegate!(self, f => f.run_by_number(repo, number))
    }

    async fn recent_runs(
        &self,
        repo: &RepoRef,
        of: RunsOf<'_>,
        pages: u32,
        after: Option<u64>,
    ) -> Result<Vec<Run>, ApiError> {
        delegate!(self, f => f.recent_runs(repo, of, pages, after))
    }

    async fn external_runs(&self, repo: &RepoRef, sha: &str) -> Result<Vec<Run>, ApiError> {
        delegate!(self, f => f.external_runs(repo, sha))
    }

    async fn has_runs(&self, repo: &RepoRef) -> Result<bool, ApiError> {
        delegate!(self, f => f.has_runs(repo))
    }

    async fn jobs_of(&self, repo: &RepoRef, run: &Run) -> Result<Vec<Job>, ApiError> {
        delegate!(self, f => f.jobs_of(repo, run))
    }

    async fn branch_head(&self, repo: &RepoRef, branch: &str) -> Result<Option<String>, ApiError> {
        delegate!(self, f => f.branch_head(repo, branch))
    }

    async fn job_log(
        &self,
        repo: &RepoRef,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        delegate!(self, f => f.job_log(repo, job, tail_bytes))
    }

    async fn runners(&self, repo: &RepoRef, user: Option<&User>) -> Result<RunnerReport, ApiError> {
        delegate!(self, f => f.runners(repo, user))
    }

    async fn pull_requests(
        &self,
        repo: &RepoRef,
        state: StateFilter,
        limit: u32,
    ) -> Result<Vec<PullRequest>, ApiError> {
        delegate!(self, f => f.pull_requests(repo, state, limit))
    }

    async fn pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<PullRequest>, ApiError> {
        delegate!(self, f => f.pull_request(repo, number))
    }

    async fn pull_request_reviews(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<Review>, ApiError> {
        delegate!(self, f => f.pull_request_reviews(repo, number))
    }

    async fn status_checks(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<StatusChecks>, ApiError> {
        delegate!(self, f => f.status_checks(repo, branch))
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        new: &NewPullRequest,
    ) -> Result<PullRequest, ApiError> {
        delegate!(self, f => f.create_pull_request(repo, new))
    }

    async fn merge_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        delegate!(self, f => f.merge_pull_request(repo, number, style, head_sha, delete_branch))
    }

    async fn schedule_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        delegate!(self, f => f.schedule_merge(repo, number, style, head_sha, delete_branch))
    }

    async fn cancel_scheduled_merge(&self, repo: &RepoRef, number: u64) -> Result<bool, ApiError> {
        delegate!(self, f => f.cancel_scheduled_merge(repo, number))
    }

    async fn cancel_run(&self, repo: &RepoRef, run: &Run) -> Result<(), ApiError> {
        delegate!(self, f => f.cancel_run(repo, run))
    }

    async fn rerun(
        &self,
        repo: &RepoRef,
        run: &Run,
        failed_only: bool,
    ) -> Result<Vec<String>, ApiError> {
        delegate!(self, f => f.rerun(repo, run, failed_only))
    }

    async fn dispatch(
        &self,
        repo: &RepoRef,
        workflow: &str,
        git_ref: &str,
        inputs: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Dispatched, ApiError> {
        delegate!(self, f => f.dispatch(repo, workflow, git_ref, inputs))
    }

    async fn repo_settings(
        &self,
        repo: &RepoRef,
    ) -> Result<serde_json::Map<String, serde_json::Value>, ApiError> {
        delegate!(self, f => f.repo_settings(repo))
    }

    async fn branch_rules(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        delegate!(self, f => f.branch_rules(repo))
    }

    async fn webhooks(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        delegate!(self, f => f.webhooks(repo))
    }

    async fn apply_setting(&self, repo: &RepoRef, op: &Op) -> Result<Done, ApiError> {
        delegate!(self, f => f.apply_setting(repo, op))
    }

    async fn test_webhook(&self, repo: &RepoRef, id: u64) -> Result<(), ApiError> {
        delegate!(self, f => f.test_webhook(repo, id))
    }

    async fn webhook_deliveries(
        &self,
        repo: &RepoRef,
        id: u64,
        limit: u32,
    ) -> Result<Vec<Delivery>, ApiError> {
        delegate!(self, f => f.webhook_deliveries(repo, id, limit))
    }
}

/// Works out what runs at `base_url` and returns a driver for it.
///
/// github.com is recognised by name. Otherwise Forgejo answers
/// `/api/forgejo/v1/version` and Gitea doesn't (404), so that endpoint is
/// tried first and `/api/v1/version` second. Both requests carry the
/// credential, because an instance that requires sign-in refuses even the
/// version endpoint anonymously. `kind` from the config skips detection.
pub async fn connect(
    host: &str,
    base_url: &Url,
    kind: Option<ForgeKind>,
    credential: Option<Credential>,
) -> Result<AnyForge, ApiError> {
    let is_github_com = host.eq_ignore_ascii_case("github.com");
    match kind {
        Some(ForgeKind::Github) => github::Github::connect(host, base_url, credential, "config")
            .await
            .map(AnyForge::Github),
        None if is_github_com => github::Github::connect(host, base_url, credential, "host name")
            .await
            .map(AnyForge::Github),
        Some(k) => gitea::GiteaFamily::connect_as(host, base_url, k, credential)
            .await
            .map(AnyForge::Gitea),
        None => gitea::GiteaFamily::detect(host, base_url, credential)
            .await
            .map(AnyForge::Gitea),
    }
}

/// Lists the runners in each scope, and what each scope said: listed,
/// `not_permitted` and why, or `not_found` where the endpoint is missing.
/// A scope given as `Err` is reported with that outcome, unasked. `url`
/// turns an endpoint into the URL to ask.
pub(crate) async fn list_runners(
    http: &http::Http,
    url: impl Fn(&str) -> String,
    scopes: impl IntoIterator<Item = (Scope, Result<String, String>)>,
    not_found: &str,
) -> Result<(Vec<Runner>, Vec<ScopeOutcome>), ApiError> {
    let mut runners: Vec<Runner> = Vec::new();
    let mut outcomes = Vec::new();
    for (scope, path) in scopes {
        let (endpoint, outcome, found) = match path {
            Err(outcome) => (String::new(), outcome, Vec::new()),
            Ok(path) => {
                let (outcome, found) = ask_scope(http, &url(&path), scope, not_found).await?;
                (path, outcome, found)
            }
        };
        let count = found.len();
        for r in found {
            if !runners.iter().any(|x| x.id == r.id) {
                runners.push(r);
            }
        }
        outcomes.push(ScopeOutcome {
            scope,
            endpoint,
            outcome,
            runners: count,
        });
    }
    Ok((runners, outcomes))
}

/// One scope's runners, and what asking said.
async fn ask_scope(
    http: &http::Http,
    url: &str,
    scope: Scope,
    not_found: &str,
) -> Result<(String, Vec<Runner>), ApiError> {
    match http.get_json::<serde_json::Value>(url).await {
        Ok(body) => Ok(("listed".to_owned(), runners::parse_runners(&body, scope))),
        Err(ApiError::Refused { hint, .. }) => Ok((format!("not_permitted: {hint}"), Vec::new())),
        Err(ApiError::NotFound { .. }) => Ok((not_found.to_owned(), Vec::new())),
        Err(e) => Err(e),
    }
}

/// `base` with `path` appended, keeping any path prefix the instance is
/// served under.
pub(crate) fn join(base: &Url, path: &str) -> String {
    format!("{}{}", base.as_str().trim_end_matches('/'), path)
}

/// `s` as one URL path segment: everything but letters, digits and `-._~`
/// percent-encoded, so a rule pattern like `release/*` stays one segment.
pub(crate) fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Whether a page of runs (newest first) reaches back to run `after`, so
/// the pages after it hold nothing new.
pub(crate) fn reaches(runs: &[Run], after: Option<u64>) -> bool {
    after.is_some_and(|after| runs.iter().any(|r| r.id <= after))
}
