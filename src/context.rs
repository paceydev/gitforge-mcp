//! Probing the bound repository: which forge, which version, who the
//! credential belongs to, what it may do, and so which tools to offer.

use std::path::PathBuf;

use serde::Serialize;

use crate::auth::{self, Attempt};
use crate::cache::{Cache, CachedProbe, now_unix};
use crate::capability::{Capabilities, Feature};
use crate::ci::Run;
use crate::config::Config;
use crate::external::{self, CiSystem, Provider};
use crate::forge::{self, AnyForge, ApiError, Forge};
use crate::remote::{self, Binding, RepoRef};
use crate::site::{RepoInfo, ServerInfo, User};

/// Where to look for the repository, and which config file to read.
#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub remote: Option<String>,
    pub config_path: Option<PathBuf>,
    /// Offer no writes, whatever the credential allows.
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Probed just now.
    Live,
    /// The forge is unreachable; this is the last successful probe.
    Cached,
    /// The forge is unreachable and has never been probed from here.
    Unreachable,
    /// The forge answered, but with an error (no access, no such repo).
    Error,
    /// No repository to bind to: not a git checkout, or no usable remote.
    Unbound,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialReport {
    /// Where the credential came from; never the credential itself.
    pub source: Option<String>,
    pub tried: Vec<Attempt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedReport {
    pub probed_at_unix: u64,
    pub age: String,
}

/// Everything `forge_context` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub status: Status,
    pub binding: Option<Binding>,
    pub forge: Option<ServerInfo>,
    pub user: Option<User>,
    pub credential: Option<CredentialReport>,
    pub repository: Option<RepoInfo>,
    pub capabilities: Option<Capabilities>,
    /// CI systems outside the forge that this repository uses and
    /// gitforge-mcp reaches through their own API (Woodpecker).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ci_systems: Vec<CiSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<CachedReport>,
    /// What went wrong, in words a person can act on.
    pub problems: Vec<String>,
    /// The tools offered as a result. Filled in by the server.
    pub tools: Vec<String>,
}

impl Report {
    fn empty(status: Status) -> Self {
        Self {
            status,
            binding: None,
            forge: None,
            user: None,
            credential: None,
            repository: None,
            capabilities: None,
            ci_systems: Vec::new(),
            cached: None,
            problems: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// A live connection to the bound repository's forge, for tools to use.
pub struct Session {
    pub forge: AnyForge,
    pub repo: RepoRef,
    pub info: RepoInfo,
    pub dir: PathBuf,
    pub user: Option<User>,
    pub capabilities: Capabilities,
    /// CI systems outside the forge, reached through their own API.
    pub providers: Vec<Provider>,
}

/// Probes the forge. The session is there only when the forge answered
/// (`Status::Live`).
pub async fn probe(options: &Options, cache: &Cache) -> (Report, Option<Session>) {
    let mut problems = Vec::new();
    let config = match options.config_path.as_deref() {
        Some(path) => Config::load(path).unwrap_or_else(|e| {
            problems.push(format!("config file ignored: {e}"));
            Config::default()
        }),
        None => Config::default(),
    };

    let binding = match remote::bind(&options.dir, options.remote.as_deref()).await {
        Ok(b) => b,
        Err(e) => {
            let mut report = Report::empty(Status::Unbound);
            problems.push(e.to_string());
            report.problems = problems;
            return (report, None);
        }
    };
    let repo_ref = binding.repo.clone();
    let dir = binding.dir.clone();
    let host_config = config.host(&repo_ref.host);
    let base_url = host_config
        .base_url
        .clone()
        .unwrap_or_else(|| repo_ref.base_url.clone());

    let (resolved, tried) = auth::resolve(&repo_ref.host, base_url.scheme(), &host_config).await;
    let credential_report = CredentialReport {
        source: resolved.as_ref().map(|r| r.source.clone()),
        tried,
    };
    if resolved.is_none() {
        problems.push(format!(
            "no credential found for {}: continuing anonymously; run `gitforge-mcp auth set {}` or see the README",
            repo_ref.host, repo_ref.host
        ));
    }
    let credential = resolved.map(|r| r.credential);
    let has_credential = credential.is_some();

    let mut report = Report::empty(Status::Live);
    report.binding = Some(binding);
    report.credential = Some(credential_report);

    let key = Cache::key(&repo_ref.host, &repo_ref.full_name());
    let forge = match forge::connect(&repo_ref.host, &base_url, host_config.kind, credential).await
    {
        Ok(f) => f,
        Err(e) => {
            return (
                fail(report, problems, e, cache, &key, options.read_only),
                None,
            );
        }
    };
    report.forge = Some(forge.server().clone());

    if has_credential {
        match forge.whoami().await {
            Ok(user) => report.user = Some(user),
            Err(ApiError::Refused { status: 403, hint, .. }) => problems.push(format!(
                "could not read the signed-in user ({hint}). A scoped token needs read:user for this; only the login, owner runners and instance runners depend on it"
            )),
            Err(e) => problems.push(format!("could not read the signed-in user: {e}")),
        }
    }

    let repo = match forge.repository(&repo_ref).await {
        Ok(r) => r,
        Err(ApiError::NotFound { host, .. }) => {
            report.status = Status::Error;
            problems.push(format!(
                "{host} has no repository {}, or the credential cannot see it",
                repo_ref.full_name()
            ));
            report.problems = problems;
            return (report, None);
        }
        Err(e) => {
            return (
                fail(report, problems, e, cache, &key, options.read_only),
                None,
            );
        }
    };
    let (capabilities, ci_systems, providers) =
        capabilities_with_ci(&forge, &repo_ref, &repo, &config, options.read_only).await;
    report.ci_systems = ci_systems;
    report.capabilities = Some(capabilities.clone());
    cache.put(
        &key,
        CachedProbe {
            server: forge.server().clone(),
            user: report.user.clone(),
            repo: repo.clone(),
            probed_at_unix: now_unix(),
        },
    );
    report.repository = Some(repo.clone());
    report.problems = problems;
    let session = Session {
        forge,
        repo: repo_ref,
        info: repo,
        dir,
        user: report.user.clone(),
        capabilities,
        providers,
    };
    (report, Some(session))
}

/// What the forge gives, and each CI system outside it that the repository
/// uses: the capabilities, the systems as reported, and those that answered.
async fn capabilities_with_ci(
    forge: &AnyForge,
    repo_ref: &RepoRef,
    repo: &RepoInfo,
    config: &Config,
    read_only: bool,
) -> (Capabilities, Vec<CiSystem>, Vec<Provider>) {
    let forge_caps = Capabilities::evaluate(forge.server(), repo);
    let runs = if forge_caps.is_available(Feature::CiStatuses) {
        default_branch_runs(forge, repo_ref, &repo.default_branch).await
    } else {
        Vec::new()
    };
    let (systems, providers) = external::discover(&runs, &repo_ref.full_name(), config).await;
    let all = providers
        .iter()
        .fold(forge_caps, |caps, p| caps.with(p.capabilities()));
    let caps = if read_only { all.read_only() } else { all };
    (caps, systems, providers)
}

/// The CI outside the forge on the default branch's head: where its links
/// point says which systems the repository uses.
async fn default_branch_runs(forge: &AnyForge, repo: &RepoRef, branch: &str) -> Vec<Run> {
    let Ok(Some(head)) = forge.branch_head(repo, branch).await else {
        return Vec::new();
    };
    forge.external_runs(repo, &head).await.unwrap_or_default()
}

/// An unreachable forge falls back to its last probe; any other failure is
/// reported as it is.
fn fail(
    mut report: Report,
    mut problems: Vec<String>,
    error: ApiError,
    cache: &Cache,
    key: &str,
    read_only: bool,
) -> Report {
    problems.push(error.to_string());
    if !error.is_unreachable() {
        report.status = Status::Error;
        report.problems = problems;
        return report;
    }
    match cache.get(key) {
        Some(cached) => {
            report.status = Status::Cached;
            let caps = Capabilities::evaluate(&cached.server, &cached.repo);
            report.capabilities = Some(if read_only { caps.read_only() } else { caps });
            report.forge = Some(cached.server);
            report.user = cached.user;
            report.repository = Some(cached.repo);
            report.cached = Some(CachedReport {
                probed_at_unix: cached.probed_at_unix,
                age: describe_age(now_unix().saturating_sub(cached.probed_at_unix)),
            });
            problems.push(
                "showing the last successful probe; tools that call the forge will fail until it is reachable"
                    .into(),
            );
        }
        None => report.status = Status::Unreachable,
    }
    report.problems = problems;
    report
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

fn describe_age(seconds: u64) -> String {
    match seconds {
        0..MINUTE => format!("{seconds} seconds"),
        MINUTE..HOUR => format!("{} minutes", seconds / MINUTE),
        HOUR..DAY => format!("{} hours", seconds / HOUR),
        _ => format!("{} days", seconds / DAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(describe_age(5), "5 seconds");
        assert_eq!(describe_age(125), "2 minutes");
        assert_eq!(describe_age(7300), "2 hours");
        assert_eq!(describe_age(200_000), "2 days");
    }
}
