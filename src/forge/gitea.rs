//! Gitea and Forgejo. Forgejo forked from Gitea and keeps its `/api/v1`, so
//! one driver serves both; where they differ, the difference is keyed on the
//! server's kind and version, never guessed from a failed call.

mod ci;
mod queue;
mod settings;
mod write;

use serde::Deserialize;
use url::Url;

use super::http::{Http, TokenScheme};
use super::{ApiError, Forge, Permissions, RepoInfo, ServerInfo, User, join};
use crate::auth::Credential;
use crate::ci::{Dispatched, Job, Run};
use crate::pulls::{MergeStyle, NewPullRequest, PullRequest, Review, StateFilter, StatusChecks};
use crate::remote::RepoRef;
use crate::runners::RunnerReport;
use crate::settings::{Delivery, Done, Item, Op};
use crate::version::{ForgeKind, Version};

const FORGEJO_VERSION: &str = "/api/forgejo/v1/version";
const GITEA_VERSION: &str = "/api/v1/version";

pub struct GiteaFamily {
    http: Http,
    base: Url,
    server: ServerInfo,
}

#[derive(Deserialize)]
struct VersionBody {
    version: String,
}

impl GiteaFamily {
    /// Tells Gitea from Forgejo by asking. See [`super::connect`].
    pub async fn detect(
        host: &str,
        base: &Url,
        credential: Option<Credential>,
    ) -> Result<Self, ApiError> {
        let http = Http::new(host, credential, TokenScheme::Token);
        if let Some(body) = http
            .get_json_opt::<VersionBody>(&join(base, FORGEJO_VERSION))
            .await?
        {
            return Ok(Self::new(
                http,
                base,
                ForgeKind::Forgejo,
                body.version,
                FORGEJO_VERSION,
            ));
        }
        match http
            .get_json_opt::<VersionBody>(&join(base, GITEA_VERSION))
            .await?
        {
            // Forgejo releases from before the Forgejo-only endpoint existed
            // still mark themselves with a +gitea-<version> suffix.
            Some(body) if body.version.contains("+gitea-") => Ok(Self::new(
                http,
                base,
                ForgeKind::Forgejo,
                body.version,
                "/api/v1/version (+gitea suffix)",
            )),
            Some(body) => Ok(Self::new(
                http,
                base,
                ForgeKind::Gitea,
                body.version,
                GITEA_VERSION,
            )),
            None => Err(ApiError::NotFound {
                host: host.to_owned(),
                path: format!(
                    "{GITEA_VERSION} or {FORGEJO_VERSION}: it doesn't look like Gitea, Forgejo or GitHub; set `kind` for this host in the config file if it is one"
                ),
            }),
        }
    }

    /// Takes the kind from the config file and only reads the version.
    pub async fn connect_as(
        host: &str,
        base: &Url,
        kind: ForgeKind,
        credential: Option<Credential>,
    ) -> Result<Self, ApiError> {
        let http = Http::new(host, credential, TokenScheme::Token);
        let path = if kind == ForgeKind::Forgejo {
            FORGEJO_VERSION
        } else {
            GITEA_VERSION
        };
        let version = http
            .get_json_opt::<VersionBody>(&join(base, path))
            .await?
            .map(|b| b.version);
        let mut forge = Self::new(http, base, kind, version.unwrap_or_default(), "config");
        if forge.server.version_raw.as_deref() == Some("") {
            forge.server.version_raw = None;
        }
        Ok(forge)
    }

    fn new(http: Http, base: &Url, kind: ForgeKind, raw: String, detected_by: &str) -> Self {
        Self {
            http,
            base: base.clone(),
            server: ServerInfo {
                kind,
                version: Version::parse(&raw),
                version_raw: Some(raw),
                detected_by: detected_by.to_owned(),
            },
        }
    }

    fn api(&self, path: &str) -> String {
        join(&self.base, &format!("/api/v1{path}"))
    }
}

#[derive(Deserialize)]
struct UserBody {
    login: String,
    #[serde(default)]
    is_admin: bool,
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
    #[serde(default)]
    permissions: Option<Permissions>,
    has_actions: Option<bool>,
}

impl Forge for GiteaFamily {
    fn server(&self) -> &ServerInfo {
        &self.server
    }

    async fn whoami(&self) -> Result<User, ApiError> {
        let body: UserBody = self.http.get_json(&self.api("/user")).await?;
        Ok(User {
            login: body.login,
            site_admin: body.is_admin,
        })
    }

    async fn repository(&self, repo: &RepoRef) -> Result<RepoInfo, ApiError> {
        let body: RepoBody = self
            .http
            .get_json(&self.api(&format!("/repos/{}/{}", repo.owner, repo.name)))
            .await?;
        Ok(RepoInfo {
            full_name: body.full_name,
            default_branch: body.default_branch,
            private: body.private,
            archived: body.archived,
            // Anonymous reads of a public repository carry no permissions
            // block: reading is all an anonymous caller can do.
            permissions: body.permissions.unwrap_or(Permissions {
                admin: false,
                push: false,
                pull: true,
            }),
            actions_enabled: body.has_actions,
            auto_merge_allowed: None,
        })
    }

    async fn runs_for_commit(&self, repo: &RepoRef, sha: &str) -> Result<Vec<Run>, ApiError> {
        self.ci_runs_for_commit(repo, sha).await
    }

    async fn run_by_number(&self, repo: &RepoRef, number: u64) -> Result<Option<Run>, ApiError> {
        self.ci_run_by_number(repo, number).await
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
        self.pulls_get(repo, number).await
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
        self.pulls_merge(repo, number, style, head_sha, delete_branch, false)
            .await
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

    async fn schedule_merge(
        &self,
        repo: &RepoRef,
        number: u64,
        style: MergeStyle,
        head_sha: &str,
        delete_branch: bool,
    ) -> Result<(), ApiError> {
        self.pulls_merge(repo, number, style, head_sha, delete_branch, true)
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

    /// Gitea and Forgejo keep a webhook's deliveries, but their API doesn't
    /// list them; the capability table never offers this here.
    async fn webhook_deliveries(
        &self,
        _repo: &RepoRef,
        _id: u64,
        _limit: u32,
    ) -> Result<Vec<Delivery>, ApiError> {
        Err(ApiError::Unsupported {
            what: "listing webhook deliveries".into(),
        })
    }
}
