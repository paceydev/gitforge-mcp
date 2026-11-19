//! GitHub and GitHub Enterprise Server.
//!
//! Most of it is REST. Two things are only in GraphQL: scheduling a merge
//! for when checks pass (and cancelling it), and whether a review thread is
//! resolved.

mod ci;
mod pulls;
mod settings;
mod write;

use serde::Deserialize;
use url::Url;

use super::http::{Http, TokenScheme};
use super::{ApiError, Forge, Permissions, RepoInfo, ServerInfo, User};
use crate::auth::Credential;
use crate::ci::{Dispatched, Job, Run};
use crate::pulls::{MergeStyle, NewPullRequest, PullRequest, Review, StateFilter, StatusChecks};
use crate::remote::RepoRef;
use crate::runners::RunnerReport;
use crate::settings::{Delivery, Done, Item, Op};
use crate::version::{ForgeKind, Version};

pub struct Github {
    http: Http,
    api: String,
    graphql: String,
    server: ServerInfo,
}

#[derive(Deserialize)]
struct MetaBody {
    installed_version: Option<String>,
}

impl Github {
    /// github.com's API is `api.github.com`; Enterprise Server's is
    /// `<base>/api/v3`, and its `/meta` reports the installed version.
    pub async fn connect(
        host: &str,
        base: &Url,
        credential: Option<Credential>,
        detected_by: &str,
    ) -> Result<Self, ApiError> {
        let http = Http::new(host, credential, TokenScheme::Bearer)
            .with_header("X-GitHub-Api-Version", "2022-11-28");
        let is_dotcom = host.eq_ignore_ascii_case("github.com");
        let root = base.as_str().trim_end_matches('/');
        let (api, graphql) = if is_dotcom {
            (
                "https://api.github.com".to_owned(),
                "https://api.github.com/graphql".to_owned(),
            )
        } else {
            (format!("{root}/api/v3"), format!("{root}/api/graphql"))
        };
        let version_raw = if is_dotcom {
            None
        } else {
            // A server that won't say is reported as such, so it isn't
            // taken for github.com, which has no version.
            Some(
                http.get_json_opt::<MetaBody>(&format!("{api}/meta"))
                    .await?
                    .and_then(|m| m.installed_version)
                    .unwrap_or_else(|| "unknown".to_owned()),
            )
        };
        Ok(Self {
            http,
            api,
            graphql,
            server: ServerInfo {
                kind: ForgeKind::Github,
                version: version_raw.as_deref().and_then(Version::parse),
                version_raw,
                detected_by: detected_by.to_owned(),
            },
        })
    }

    /// True on github.com, and on an Enterprise Server at least `release`
    /// or whose version is unknown (assumed current, as the capability
    /// table does).
    fn at_least(&self, release: Version) -> bool {
        self.server.version.is_none_or(|v| v >= release)
    }

    fn repo_api(&self, repo: &RepoRef, path: &str) -> String {
        format!("{}/repos/{}/{}{path}", self.api, repo.owner, repo.name)
    }
}

#[derive(Deserialize)]
struct UserBody {
    login: String,
    #[serde(default)]
    site_admin: bool,
}

#[derive(Deserialize)]
struct RepoBody {
    full_name: String,
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    archived: bool,
    permissions: Option<GithubPermissions>,
    /// Sent only to callers who may change the setting.
    allow_auto_merge: Option<bool>,
}

/// GitHub has five roles; `maintain` can push but not administer, and
/// `triage` cannot push, so they fold into push and pull respectively.
#[derive(Deserialize)]
struct GithubPermissions {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    maintain: bool,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    triage: bool,
    #[serde(default)]
    pull: bool,
}

impl Forge for Github {
    fn server(&self) -> &ServerInfo {
        &self.server
    }

    async fn whoami(&self) -> Result<User, ApiError> {
        let body: UserBody = self.http.get_json(&format!("{}/user", self.api)).await?;
        Ok(User {
            login: body.login,
            site_admin: body.site_admin,
        })
    }

    async fn repository(&self, repo: &RepoRef) -> Result<RepoInfo, ApiError> {
        let body: RepoBody = self.http.get_json(&self.repo_api(repo, "")).await?;
        let permissions = body.permissions.map_or(
            Permissions {
                admin: false,
                push: false,
                pull: true,
            },
            |p| Permissions {
                admin: p.admin,
                push: p.admin || p.maintain || p.push,
                pull: p.admin || p.maintain || p.push || p.triage || p.pull,
            },
        );
        Ok(RepoInfo {
            full_name: body.full_name,
            default_branch: body.default_branch,
            private: body.private,
            archived: body.archived,
            permissions,
            // Whether Actions is on takes admin to read; a repository with
            // it off simply has no runs.
            actions_enabled: None,
            auto_merge_allowed: body.allow_auto_merge,
        })
    }

    async fn runs_for_commit(&self, repo: &RepoRef, sha: &str) -> Result<Vec<Run>, ApiError> {
        self.ci_runs_for_commit(repo, sha).await
    }

    async fn run_by_number(&self, repo: &RepoRef, number: u64) -> Result<Option<Run>, ApiError> {
        self.ci_run_by_id(repo, number).await
    }

    async fn recent_runs(
        &self,
        repo: &RepoRef,
        of: crate::ci::RunsOf<'_>,
        pages: u32,
        after: Option<u64>,
    ) -> Result<Vec<Run>, ApiError> {
        self.ci_recent_runs(repo, of, pages, after).await
    }

    async fn external_runs(&self, repo: &RepoRef, sha: &str) -> Result<Vec<Run>, ApiError> {
        self.ci_external_runs(repo, sha).await
    }

    async fn has_runs(&self, repo: &RepoRef) -> Result<bool, ApiError> {
        self.ci_has_runs(repo).await
    }

    async fn jobs_of(&self, repo: &RepoRef, run: &Run) -> Result<Vec<Job>, ApiError> {
        self.ci_jobs_of(repo, run).await
    }

    async fn branch_head(&self, repo: &RepoRef, branch: &str) -> Result<Option<String>, ApiError> {
        self.ci_branch_head(repo, branch).await
    }

    async fn job_log(
        &self,
        repo: &RepoRef,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        self.ci_job_log(repo, job, tail_bytes).await
    }

    async fn runners(&self, repo: &RepoRef, user: Option<&User>) -> Result<RunnerReport, ApiError> {
        self.ci_runners(repo, user).await
    }

    async fn pull_requests(
        &self,
        repo: &RepoRef,
        state: StateFilter,
        limit: u32,
    ) -> Result<Vec<PullRequest>, ApiError> {
        self.pulls_list(repo, state, limit).await
    }

    async fn pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<PullRequest>, ApiError> {
        Ok(self
            .pulls_get_raw(repo, number)
            .await?
            .map(|p| p.into_pull_request(true)))
    }

    async fn pull_request_reviews(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<Review>, ApiError> {
        self.pulls_reviews(repo, number).await
    }

    async fn status_checks(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<StatusChecks>, ApiError> {
        self.pulls_status_checks(repo, branch).await
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        new: &NewPullRequest,
    ) -> Result<PullRequest, ApiError> {
        self.pulls_create(repo, new).await
    }

    async fn merge_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        self.pulls_merge(repo, number, style, head_sha, delete_branch)
            .await
    }

    async fn schedule_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        self.pulls_schedule_merge(repo, number, style, head_sha, delete_branch)
            .await
    }

    async fn cancel_scheduled_merge(&self, repo: &RepoRef, number: u64) -> Result<bool, ApiError> {
        self.pulls_cancel_scheduled_merge(repo, number).await
    }

    async fn cancel_run(&self, repo: &RepoRef, run: &Run) -> Result<(), ApiError> {
        self.ci_cancel(repo, run).await
    }

    async fn rerun(
        &self,
        repo: &RepoRef,
        run: &Run,
        failed_only: bool,
    ) -> Result<Vec<String>, ApiError> {
        self.ci_rerun(repo, run, failed_only).await
    }

    async fn dispatch(
        &self,
        repo: &RepoRef,
        workflow: &str,
        git_ref: &str,
        inputs: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Dispatched, ApiError> {
        self.ci_dispatch(repo, workflow, git_ref, inputs).await
    }

    async fn repo_settings(
        &self,
        repo: &RepoRef,
    ) -> Result<serde_json::Map<String, serde_json::Value>, ApiError> {
        self.settings_repo(repo).await
    }

    async fn branch_rules(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        self.settings_rules(repo).await
    }

    async fn webhooks(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        self.settings_hooks(repo).await
    }

    async fn apply_setting(&self, repo: &RepoRef, op: &Op) -> Result<Done, ApiError> {
        self.settings_apply(repo, op).await
    }

    async fn test_webhook(&self, repo: &RepoRef, id: u64) -> Result<(), ApiError> {
        self.settings_test_hook(repo, id).await
    }

    async fn webhook_deliveries(
        &self,
        repo: &RepoRef,
        id: u64,
        limit: u32,
    ) -> Result<Vec<Delivery>, ApiError> {
        self.settings_deliveries(repo, id, limit).await
    }
}
