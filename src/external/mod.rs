//! CI systems outside the forge, reached through their own API: Woodpecker
//! and Jenkins.
//!
//! A repository's commit statuses say where its CI runs (`ci::external_runs`
//! groups them into runs); a provider for that host fills in what statuses
//! can't: workflows and steps with timings, step logs, cancel, restart,
//! manual pipelines and agents. A provider is keyed by the CI host, not the
//! forge, so it sits beside the `Forge` trait rather than in it. Where no
//! provider answers, a run stays as its statuses describe it.

pub mod jenkins;
pub mod woodpecker;

use std::collections::BTreeMap;

use serde::Serialize;
use url::Url;

use crate::auth::{self, Attempt};
use crate::capability::{self, Availability, CiAccess};
use crate::ci::{Dispatched, Job, Run};
use crate::config::Config;
use crate::forge::ApiError;
use crate::runners::RunnerReport;
use crate::site::{Permissions, User};
pub use crate::version::CiKind;
use crate::version::Version;

use jenkins::Jenkins;
use woodpecker::Woodpecker;

/// What the probe learned about one CI system, for `forge_context`.
#[derive(Debug, Clone, Serialize)]
pub struct CiSystem {
    /// "woodpecker ci.example.com": how capabilities name it.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<CiKind>,
    pub base_url: String,
    /// `config`, or `commit status link`.
    pub found_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// Where the credential came from; never the credential itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub tried: Vec<Attempt>,
    /// Why it can't be used, in words a person can act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A CI host to try: named in the config, or linked from a status.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    host: String,
    base_url: Url,
    kind: Option<CiKind>,
    repo_id: Option<u64>,
    /// Jenkins jobs the statuses link to.
    jobs: Vec<Url>,
    found_by: &'static str,
}

/// `host[:port]` of a URL, as config sections and credentials name it.
fn host_of(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => String::new(),
    }
}

/// `url` ending in `/`, so paths join onto it.
fn as_root(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

/// Every CI host to try: each `[ci."<host>"]` in the config, then each
/// Woodpecker or Jenkins host the runs' status links point to.
fn candidates(config: &Config, runs: &[Run]) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = config
        .ci
        .iter()
        .filter_map(|(host, c)| {
            let base_url = c
                .base_url
                .clone()
                .or_else(|| Url::parse(&format!("https://{host}/")).ok())?;
            Some(Candidate {
                host: host.clone(),
                base_url: as_root(base_url),
                kind: c.kind,
                repo_id: None,
                jobs: Vec::new(),
                found_by: "config",
            })
        })
        .collect();
    let links = runs
        .iter()
        .filter(|r| r.external.is_some())
        .flat_map(|r| r.jobs.iter().filter_map(|j| j.url.as_deref()));
    for raw in links {
        let Some(linked) = linked_candidate(raw) else {
            continue;
        };
        match found.iter_mut().find(|c| c.host == linked.host) {
            Some(known) => {
                // A section that names no base URL takes the links' (its
                // scheme and path prefix).
                if config
                    .ci
                    .get(&known.host)
                    .is_some_and(|c| c.base_url.is_none())
                {
                    known.base_url = linked.base_url.clone();
                }
                known.repo_id = known.repo_id.or(linked.repo_id);
                for job in linked.jobs {
                    if !known.jobs.contains(&job) {
                        known.jobs.push(job);
                    }
                }
            }
            None => found.push(linked),
        }
    }
    found
}

/// The candidate a status link names: a Woodpecker pipeline, or a Jenkins
/// build (a link to a job alone, for a build still queued, names none).
fn linked_candidate(raw: &str) -> Option<Candidate> {
    let (base_url, repo_id, jobs) = match woodpecker::parse_link(raw) {
        Some(link) => (link.base_url, Some(link.repo_id), Vec::new()),
        None => {
            let link = jenkins::parse_link(raw).filter(|l| l.number.is_some())?;
            (link.base_url, None, vec![link.job_url])
        }
    };
    Some(Candidate {
        host: host_of(&base_url),
        base_url,
        kind: None,
        repo_id,
        jobs,
        found_by: "commit status link",
    })
}

enum Client {
    Woodpecker(Woodpecker),
    Jenkins(Jenkins),
}

/// A CI system outside the forge that answered, for the bound repository.
pub struct Provider {
    pub system: CiSystem,
    kind: CiKind,
    version: Option<Version>,
    access: CiAccess,
    client: Client,
}

/// Connects to each CI system outside the forge that the config names or
/// `runs` (a commit's runs from its statuses) link to. Systems that can't
/// be used are reported, with why.
pub async fn discover(
    runs: &[Run],
    full_name: &str,
    config: &Config,
) -> (Vec<CiSystem>, Vec<Provider>) {
    let mut systems = Vec::new();
    let mut providers = Vec::new();
    for candidate in candidates(config, runs) {
        let connected =
            tokio::time::timeout(CONNECT_TIMEOUT, connect(&candidate, full_name, config))
                .await
                .unwrap_or_else(|_| Err(unanswered(&candidate)));
        match connected {
            Ok(provider) => {
                systems.push(provider.system.clone());
                providers.push(provider);
            }
            Err(system) => systems.push(system),
        }
    }
    (systems, providers)
}

/// How long the probe waits for a CI system outside the forge. Not
/// reaching it costs only its details, so it mustn't hold up the forge's
/// tools.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// A candidate as `forge_context` reports it, before it has answered.
fn system_of(candidate: &Candidate, kind: Option<CiKind>) -> CiSystem {
    let label = kind.map_or_else(|| "ci".to_owned(), |k| k.to_string().to_lowercase());
    CiSystem {
        name: format!("{label} {}", candidate.host),
        kind,
        base_url: candidate.base_url.to_string(),
        found_by: candidate.found_by.to_owned(),
        version: None,
        user: None,
        permissions: None,
        credential: None,
        tried: Vec::new(),
        problem: None,
    }
}

fn unanswered(candidate: &Candidate) -> CiSystem {
    let mut system = system_of(candidate, candidate.kind);
    system.problem = Some(format!(
        "didn't answer within {} seconds; its statuses are read as they are",
        CONNECT_TIMEOUT.as_secs()
    ));
    system
}

async fn connect(
    candidate: &Candidate,
    full_name: &str,
    config: &Config,
) -> Result<Provider, CiSystem> {
    let lookup = config
        .ci
        .get(&candidate.host)
        .cloned()
        .unwrap_or_default()
        .credential_lookup();
    let (resolved, tried) =
        auth::resolve(&candidate.host, candidate.base_url.scheme(), &lookup).await;
    let kind = kind_of(candidate, &tried).await?;
    let mut system = system_of(candidate, Some(kind));
    system.credential = resolved.as_ref().map(|r| r.source.clone());
    system.tried = tried;
    let credential = resolved.map(|r| r.credential);
    match kind {
        CiKind::Woodpecker => connect_woodpecker(candidate, system, credential, full_name).await,
        CiKind::Jenkins => {
            let username = config
                .ci
                .get(&candidate.host)
                .and_then(|c| c.username.clone());
            connect_jenkins(candidate, system, credential, username.as_deref()).await
        }
    }
}

async fn connect_woodpecker(
    candidate: &Candidate,
    system: CiSystem,
    credential: Option<auth::Credential>,
    full_name: &str,
) -> Result<Provider, CiSystem> {
    let connected = Woodpecker::connect(
        &candidate.host,
        &candidate.base_url,
        credential,
        full_name,
        candidate.repo_id,
    )
    .await;
    connected
        .map(|c| Provider::woodpecker(system.clone(), c))
        .map_err(|e| unusable(system, e, full_name))
}

async fn connect_jenkins(
    candidate: &Candidate,
    mut system: CiSystem,
    credential: Option<auth::Credential>,
    username: Option<&str>,
) -> Result<Provider, CiSystem> {
    let Ok(credential) = jenkins::basic(credential, username) else {
        system.problem = Some(format!(
            "Jenkins takes its API token with the user it belongs to: set username in [ci.\"{}\"], or store the token as user:token",
            candidate.host
        ));
        return Err(system);
    };
    let connected = Jenkins::connect(
        &candidate.host,
        &candidate.base_url,
        credential,
        &candidate.jobs,
    )
    .await;
    connected
        .map(|c| Provider::jenkins(system.clone(), c))
        .map_err(|e| unusable(system, e, "the jobs its statuses link to"))
}

/// The kind configured for a candidate, or what it turns out to be.
async fn kind_of(candidate: &Candidate, tried: &[Attempt]) -> Result<CiKind, CiSystem> {
    if let Some(kind) = candidate.kind {
        return Ok(kind);
    }
    identify(candidate).await.map_err(|problem| CiSystem {
        tried: tried.to_vec(),
        problem: Some(problem),
        ..system_of(candidate, None)
    })
}

/// `system`, with why connecting to it failed.
fn unusable(mut system: CiSystem, e: ApiError, full_name: &str) -> CiSystem {
    let kind = system
        .kind
        .map_or_else(|| "CI".to_owned(), |k| k.to_string());
    system.problem = Some(match e {
        ApiError::NotFound { .. } if system.kind == Some(CiKind::Jenkins) => format!(
            "Jenkins at {} has none of {full_name}, or the credential can't see them",
            system.base_url
        ),
        ApiError::NotFound { .. } => format!(
            "{kind} at {} has no repository {full_name}, or the credential can't see it",
            system.base_url
        ),
        other => other.to_string(),
    });
    system
}

/// What runs at a host a status links to. The kind its links look like is
/// asked first: Jenkins refuses an anonymous `/version`.
async fn identify(candidate: &Candidate) -> Result<CiKind, String> {
    let order = if candidate.jobs.is_empty() {
        [CiKind::Woodpecker, CiKind::Jenkins]
    } else {
        [CiKind::Jenkins, CiKind::Woodpecker]
    };
    for kind in order {
        if is_kind(candidate, kind).await.map_err(|e| e.to_string())? {
            return Ok(kind);
        }
    }
    Err(format!(
        "{} isn't a CI system gitforge-mcp knows; its statuses are read as they are",
        candidate.base_url
    ))
}

/// Whether `candidate` is `kind`. Only not reaching it is an error.
async fn is_kind(candidate: &Candidate, kind: CiKind) -> Result<bool, ApiError> {
    match kind {
        CiKind::Woodpecker => match Woodpecker::version(&candidate.host, &candidate.base_url).await
        {
            Ok(v) => Ok(v.source.to_lowercase().contains("woodpecker")),
            Err(e) if e.is_unreachable() => Err(e),
            Err(_) => Ok(false),
        },
        CiKind::Jenkins => Jenkins::version(&candidate.host, &candidate.base_url)
            .await
            .map(|v| v.is_some()),
    }
}

impl Provider {
    fn woodpecker(system: CiSystem, c: woodpecker::Connected) -> Self {
        let version = Some(c.version.version.clone()).filter(|v| !v.is_empty());
        Self::new(
            system,
            CiKind::Woodpecker,
            version,
            (c.user, c.permissions),
            Client::Woodpecker(c.client),
        )
    }

    fn jenkins(system: CiSystem, c: jenkins::Connected) -> Self {
        Self::new(
            system,
            CiKind::Jenkins,
            c.version,
            (c.user, c.permissions),
            Client::Jenkins(c.client),
        )
    }

    fn new(
        mut system: CiSystem,
        kind: CiKind,
        version: Option<String>,
        (user, permissions): (Option<User>, Permissions),
        client: Client,
    ) -> Self {
        system.version = version.clone();
        system.user = user.clone();
        system.permissions = Some(permissions);
        Provider {
            kind,
            version: version.as_deref().and_then(Version::parse),
            access: CiAccess {
                permissions,
                site_admin: user.as_ref().is_some_and(|u| u.site_admin),
                signed_in: user.is_some(),
            },
            system,
            client,
        }
    }

    pub fn name(&self) -> &str {
        &self.system.name
    }

    /// Which features this system gives, as capability rows.
    pub fn capabilities(&self) -> Vec<Availability> {
        capability::evaluate_ci(&self.system.name, self.kind, self.version, self.access)
    }

    /// The pipeline or build `run` is, if it ran here: the newest its
    /// links name.
    pub fn number_of(&self, run: &Run) -> Option<u64> {
        let links = run
            .url
            .iter()
            .chain(run.jobs.iter().filter_map(|j| j.url.as_ref()));
        match &self.client {
            Client::Woodpecker(w) => links
                .filter_map(|u| woodpecker::parse_link(u))
                .filter(|l| l.base_url == *w.base_url() && l.repo_id == w.repo_id())
                .map(|l| l.number)
                .max(),
            Client::Jenkins(j) => links
                .filter_map(|u| jenkins::parse_link(u))
                .filter(|l| l.base_url == *j.base_url())
                .filter_map(|l| l.number)
                .max(),
        }
    }

    /// `run` as this system's API reports it; `None` if it has no such
    /// pipeline. Jenkins reads each of the run's builds.
    pub async fn refresh(&self, run: &Run, number: u64) -> Result<Option<Run>, ApiError> {
        let w = match &self.client {
            Client::Woodpecker(w) => w,
            Client::Jenkins(j) => return j.refresh(run).await.map(Some),
        };
        let system = run
            .external
            .clone()
            .unwrap_or_else(|| self.name().to_owned());
        Ok(w.pipeline(number)
            .await?
            .map(|p| woodpecker::to_run(&p, &woodpecker_link(w, number), &system)))
    }

    /// Pipeline `number`, named the way Woodpecker's statuses name its
    /// event ("ci/woodpecker/push"). Jenkins numbers builds per job, so a
    /// number alone names none there.
    pub async fn run_by_number(&self, number: u64) -> Result<Option<Run>, ApiError> {
        let Client::Woodpecker(w) = &self.client else {
            return Ok(None);
        };
        Ok(w.pipeline(number).await?.map(|p| {
            let event = if p.event == "pull_request" {
                "pr"
            } else {
                p.event.as_str()
            };
            woodpecker::to_run(
                &p,
                &woodpecker_link(w, number),
                &format!("ci/woodpecker/{event}"),
            )
        }))
    }

    /// The log of `job` in pipeline `number`: on Woodpecker its failed
    /// steps', or every step's when none failed; on Jenkins the build's
    /// console, cut to `tail_bytes`. Returns the text and whether it is
    /// only the end.
    pub async fn job_log(
        &self,
        number: u64,
        job: &Job,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        let unreadable = || ApiError::Unsupported {
            what: format!("reading the log of {}", job.name),
        };
        match &self.client {
            Client::Woodpecker(w) => {
                let job_id = job.job_id.ok_or_else(unreadable)?;
                w.job_log(number, job_id).await.map(|t| (t, false))
            }
            Client::Jenkins(j) => {
                let link = job
                    .url
                    .as_deref()
                    .and_then(jenkins::parse_link)
                    .filter(|l| l.number.is_some())
                    .ok_or_else(unreadable)?;
                let page = format!("{}{}/", link.job_url, link.number.unwrap_or_default());
                j.console(&page, tail_bytes).await
            }
        }
    }

    /// The builds of `run` on this Jenkins: each job's build page, and
    /// the job it links to, whose state `keep` accepts.
    fn builds_of<'r>(
        j: &Jenkins,
        run: &'r Run,
        keep: impl Fn(&Job) -> bool,
    ) -> Vec<(&'r Job, jenkins::Link, u64)> {
        run.jobs
            .iter()
            .filter(|job| keep(job))
            .filter_map(|job| {
                let link = jenkins::parse_link(job.url.as_deref()?)?;
                let number = link.number?;
                (link.base_url == *j.base_url()).then_some((job, link, number))
            })
            .collect()
    }

    /// Cancels pipeline `number` on Woodpecker, or stops each of `run`'s
    /// unfinished builds on Jenkins. Returns what was stopped, in words,
    /// and notes.
    pub async fn cancel(
        &self,
        run: &Run,
        number: u64,
    ) -> Result<(Vec<String>, Vec<String>), ApiError> {
        let j = match &self.client {
            Client::Woodpecker(w) => {
                w.cancel(number).await?;
                return Ok((vec![format!("pipeline {number}")], Vec::new()));
            }
            Client::Jenkins(j) => j,
        };
        let builds = Self::builds_of(j, run, |job| !job.state.is_final());
        let mut notes: Vec<String> = run
            .jobs
            .iter()
            .filter(|job| !job.state.is_final())
            .filter(|job| !builds.iter().any(|(b, _, _)| b.name == job.name))
            .map(|job| {
                format!(
                    "{} has no build to stop yet (still in Jenkins's queue, or not linked); cancel it in Jenkins",
                    job.name
                )
            })
            .collect();
        if builds.is_empty() {
            return Err(ApiError::Unavailable {
                reason: notes.pop().unwrap_or_else(|| {
                    "none of this run's builds is still going on Jenkins".into()
                }),
            });
        }
        let mut done = Vec::new();
        for (job, link, n) in builds {
            let page = format!("{}{n}/", link.job_url);
            let page = Url::parse(&page).map_err(|e| ApiError::Unavailable {
                reason: format!("{page}: {e}"),
            })?;
            // Each stop is its own write, sent once. One refused after
            // others went through says what did.
            if let Err(e) = j.stop(&page).await {
                return Err(partly(&done, e));
            }
            done.push(format!("{}: build {n} of {}", job.name, link.job_url));
        }
        Ok((done, notes))
    }

    /// Restarts pipeline `number` on Woodpecker, all of it, or builds each
    /// of `run`'s jobs again on Jenkins. Returns what was rerun, in words,
    /// the new pipeline's number (Woodpecker), and notes.
    pub async fn rerun(
        &self,
        run: &Run,
        number: u64,
    ) -> Result<(Vec<String>, Option<u64>, Vec<String>), ApiError> {
        let j = match &self.client {
            Client::Woodpecker(w) => {
                let new = w.restart(number).await?;
                let started = new.as_ref().map(|p| p.number);
                let done = match started {
                    Some(n) => format!("pipeline {number} restarted as pipeline {n}"),
                    None => format!("pipeline {number} restarted"),
                };
                return Ok((vec![done], started, Vec::new()));
            }
            Client::Jenkins(j) => j,
        };
        let builds = Self::builds_of(j, run, |_| true);
        if builds.is_empty() {
            return Err(ApiError::Unavailable {
                reason: "none of this run's statuses links to a Jenkins build".into(),
            });
        }
        let mut done = Vec::new();
        let mut notes = vec![
            "Jenkins has no rebuild: each job was built again with the same parameters, and a branch job builds its branch as it is now, which may be a newer commit".to_owned(),
        ];
        for (job, link, n) in builds {
            let started = match j.rebuild(&link.job_url, n).await {
                Ok(s) => s,
                Err(e) => return Err(partly(&done, e)),
            };
            if let Some(why) = &started.waiting {
                notes.push(format!("{} is still in Jenkins's queue: {why}", job.name));
            }
            done.push(format!(
                "{}: build {n} built again as {}",
                job.name,
                started.describe()
            ));
        }
        Ok((done, None, notes))
    }

    /// Starts CI on `branch` (`explicit` when asked for, not the default):
    /// a manual pipeline on Woodpecker, with `inputs` as its variables and
    /// only `workflow` if given; on Jenkins a build of the job `workflow`
    /// names (or the repository's one project), its branch job for a
    /// multibranch project, with `inputs` as its parameters. Returns what
    /// started, and notes.
    pub async fn dispatch(
        &self,
        (branch, explicit): (&str, bool),
        inputs: &serde_json::Map<String, serde_json::Value>,
        workflow: Option<&str>,
    ) -> Result<(Dispatched, Vec<String>), ApiError> {
        let variables: BTreeMap<String, String> = inputs
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), value)
            })
            .collect();
        match &self.client {
            Client::Woodpecker(w) => w
                .trigger(branch, &variables, workflow)
                .await
                .map(|d| (d, Vec::new())),
            Client::Jenkins(j) => {
                let job = j.job_named(workflow)?;
                let (started, notes) = j.dispatch(&job, branch, explicit, &variables).await?;
                Ok((
                    Dispatched {
                        run_id: None,
                        run_number: started.number,
                        url: started.url.clone(),
                    },
                    notes,
                ))
            }
        }
    }

    /// Whether dispatch takes `workflow`: on Woodpecker to run one
    /// workflow alone (3.20), on Jenkins to name the job.
    pub fn takes_workflow(&self) -> bool {
        match self.client {
            Client::Woodpecker(_) => self
                .version
                .is_none_or(|v| v >= capability::WOODPECKER_3_20),
            Client::Jenkins(_) => true,
        }
    }

    pub async fn runners(&self) -> Result<RunnerReport, ApiError> {
        match &self.client {
            Client::Woodpecker(w) => w.runners().await,
            Client::Jenkins(j) => j.runners().await,
        }
    }
}

/// `e`, after the writes in `done` went through: those can't be taken
/// back, so the error says they happened.
fn partly(done: &[String], e: ApiError) -> ApiError {
    if done.is_empty() {
        return e;
    }
    ApiError::Unavailable {
        reason: format!("{} went through, then: {e}", done.join("; ")),
    }
}

fn woodpecker_link(w: &Woodpecker, number: u64) -> woodpecker::Link {
    woodpecker::Link {
        base_url: w.base_url().clone(),
        repo_id: w.repo_id(),
        number,
    }
}

/// The provider `run` ran on, and its number there.
pub fn provider_for<'a>(providers: &'a [Provider], run: &Run) -> Option<(&'a Provider, u64)> {
    run.external.as_ref()?;
    providers
        .iter()
        .find_map(|p| p.number_of(run).map(|n| (p, n)))
}

/// Replaces each run a provider ran with that provider's own account of
/// it. A provider that can't be read leaves the run as its statuses say.
pub async fn refresh_all(providers: &[Provider], runs: &mut [Run]) {
    for run in runs.iter_mut() {
        let Some((provider, number)) = provider_for(providers, run) else {
            continue;
        };
        match provider.refresh(run, number).await {
            Ok(Some(fresh)) => *run = fresh,
            Ok(None) => {}
            Err(e) => tracing::warn!("{}: {e}", provider.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::{Source, State};

    fn status_run(urls: &[&str]) -> Run {
        Run {
            id: 0,
            number: 0,
            attempt: None,
            workflow: "ci/woodpecker/push".into(),
            title: String::new(),
            event: String::new(),
            git_ref: String::new(),
            sha: "abc".into(),
            state: State::Running,
            url: urls.first().map(|u| u.to_string()),
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            jobs: urls
                .iter()
                .map(|u| Job {
                    name: "build".into(),
                    state: State::Running,
                    job_id: None,
                    task_id: None,
                    started_at: None,
                    finished_at: None,
                    duration_seconds: None,
                    source: Source::Statuses,
                    url: Some(u.to_string()),
                    steps: Vec::new(),
                })
                .collect(),
            external: Some("ci/woodpecker/push".into()),
        }
    }

    #[test]
    fn candidates_come_from_config_and_status_links() {
        let config = Config::parse(
            "[ci.\"ci.example.com:8443\"]\nkind = \"woodpecker\"\n[ci.\"other.example.com\"]\nbase_url = \"https://other.example.com/wp\"\n",
        )
        .unwrap();
        let runs = vec![
            status_run(&[
                "https://ci.example.com:8443/repos/4/pipeline/9/1",
                "https://new.example.com/repos/7/pipeline/3/2",
            ]),
            status_run(&[
                "https://jenkins.example.com/job/x/1/display/redirect",
                "https://jenkins.example.com/job/x/2/display/redirect",
                // Queued: names no build, so leads nowhere on its own.
                "https://queued.example.com/job/y/",
            ]),
        ];
        let found = candidates(&config, &runs);
        let summary: Vec<_> = found
            .iter()
            .map(|c| {
                (
                    c.host.as_str(),
                    c.base_url.as_str(),
                    c.kind,
                    c.repo_id,
                    c.jobs.iter().map(Url::as_str).collect::<Vec<_>>(),
                    c.found_by,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "ci.example.com:8443",
                    "https://ci.example.com:8443/",
                    Some(CiKind::Woodpecker),
                    Some(4),
                    vec![],
                    "config"
                ),
                (
                    "other.example.com",
                    "https://other.example.com/wp/",
                    None,
                    None,
                    vec![],
                    "config"
                ),
                (
                    "new.example.com",
                    "https://new.example.com/",
                    None,
                    Some(7),
                    vec![],
                    "commit status link"
                ),
                (
                    "jenkins.example.com",
                    "https://jenkins.example.com/",
                    None,
                    None,
                    vec!["https://jenkins.example.com/job/x/"],
                    "commit status link"
                ),
            ]
        );
    }
}
