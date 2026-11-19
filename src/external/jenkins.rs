//! Jenkins through its JSON API: builds, stages, console, nodes, and
//! stopping, starting and rebuilding builds.
//!
//! Jenkins posts one commit status per build. The Gitea plugin names it
//! `<owner>/<repo>/pipeline/<head|pr-<base>|tag>`, GitHub Branch Source
//! `continuous-integration/jenkins/<branch|pr-merge|pr-head>`; both link to
//! the build through display-url-api:
//! `<root>/job/<folder>/job/<project>/job/<branch>/<number>/display/redirect`.
//! A build is one job here (`ci::external_runs` already makes one per
//! status), and a Pipeline build's stages are its steps where Pipeline:
//! Stage View's `wfapi` is installed.
//!
//! Every endpoint used (`api/json`, `consoleText`, `whoAmI`, `computer`,
//! `queue`, `crumbIssuer`, a build's `stop`, a job's `build` and
//! `buildWithParameters`) has been in core since 1.x; `wfapi/describe` is
//! the pipeline-rest-api plugin's. Shapes are as Jenkins 2.580 serves them.
//!
//! Writes are form POSTs, as Jenkins's own pages send them, carrying the
//! CSRF crumb and the session cookie it is tied to. A request made with an
//! API token needs no crumb (2.96 on), but one made with a password does.
//! Core has no rebuild: a rebuild is a new build of the same job with the
//! same parameters.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

use crate::auth::Credential;
use crate::ci::{self, Job, Run, Source, State, Step};
use crate::forge::http::{ApiError, FormReply, Http, TokenScheme};
use crate::runners::{self, Runner, RunnerReport, RunnerState, Scope, ScopeOutcome, WaitingJob};
use crate::site::{Permissions, User};

/// The header every Jenkins response carries, with its version.
const VERSION_HEADER: &str = "X-Jenkins";

/// The most of a console log read when a tail isn't asked for, and the
/// least read for a tail.
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
const MIN_TAIL_LOG_BYTES: usize = 64 * 1024;

const MILLIS_PER_SECOND: i64 = 1000;

/// The status Jenkins refuses an anonymous caller with.
const FORBIDDEN: u16 = 403;

/// The label every built-in node carries, naming it rather than what it
/// offers.
const BUILT_IN_LABEL: &str = "built-in";

/// How often, and how far apart, a build just asked for is looked for in
/// the queue before it is reported as waiting there.
const QUEUE_CHECKS: usize = 5;
const QUEUE_CHECK_PAUSE: std::time::Duration = std::time::Duration::from_millis(1000);

/// What a status link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Jenkins's web root, with any path prefix it is served under.
    pub base_url: Url,
    /// The job (`<root>/job/a/job/b/`), ending in `/`.
    pub job_url: Url,
    /// The build. A status for a build still in the queue links to the job
    /// alone.
    pub number: Option<u64>,
}

/// Reads a classic Jenkins link: `job/<name>` pairs after the root, then
/// the build number and anything after it (`display/redirect`, `console`).
pub fn parse_link(raw: &str) -> Option<Link> {
    let url = Url::parse(raw).ok()?;
    let segments: Vec<&str> = url.path_segments()?.collect();
    let first = segments.iter().position(|s| *s == "job")?;
    let mut at = first;
    let mut job_path = String::new();
    while segments.get(at) == Some(&"job") {
        let name = segments.get(at + 1).filter(|n| !n.is_empty())?;
        job_path.push_str(&format!("job/{name}/"));
        at += 2;
    }
    let number = segments.get(at).and_then(|s| s.parse::<u64>().ok());
    let mut base_url = url.clone();
    base_url.set_query(None);
    base_url.set_fragment(None);
    let root: String = segments[..first].iter().map(|s| format!("{s}/")).collect();
    base_url.set_path(&format!("/{root}"));
    let job_url = base_url.join(&job_path).ok()?;
    Some(Link {
        base_url,
        job_url,
        number,
    })
}

/// The project a job belongs to: a multibranch job's parent
/// (`job/acme/job/widgets/` for `job/acme/job/widgets/job/main/`), or the
/// job itself when it isn't in a folder.
fn project_of(job_url: &Url) -> Url {
    let path = job_url.path().trim_end_matches('/');
    match path.rsplit_once("/job/") {
        Some((parent, _)) if parent.contains("/job/") => {
            let mut url = job_url.clone();
            url.set_path(&format!("{parent}/"));
            url
        }
        _ => job_url.clone(),
    }
}

// ------------------------------------------------------------------ API types

#[derive(Debug, Clone, Deserialize)]
pub struct WhoAmI {
    pub name: String,
    #[serde(default)]
    pub anonymous: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    pub number: u64,
    /// `SUCCESS`, `UNSTABLE`, `FAILURE`, `NOT_BUILT`, `ABORTED`; null while
    /// it runs.
    pub result: Option<String>,
    #[serde(default)]
    pub building: bool,
    /// Start, in milliseconds since the epoch.
    #[serde(default)]
    pub timestamp: i64,
    /// Milliseconds; 0 while it runs.
    #[serde(default)]
    pub duration: i64,
}

/// `wfapi/describe`: a Pipeline build's stages.
#[derive(Debug, Clone, Deserialize)]
pub struct Describe {
    #[serde(default)]
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stage {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub duration_millis: i64,
    pub error: Option<StageError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StageError {
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Computer {
    pub display_name: String,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub assigned_labels: Vec<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ComputerSet {
    #[serde(default)]
    computer: Vec<Computer>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueItem {
    #[serde(default)]
    pub why: Option<String>,
    pub task: QueueTask,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueTask {
    #[serde(default)]
    pub name: String,
    /// Relative to the root: `job/a/1/` for a Pipeline build waiting for a
    /// node, `job/a/` for a build not started.
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Queue {
    #[serde(default)]
    items: Vec<QueueItem>,
}

/// `crumbIssuer/api/json`: the CSRF token a POST carries.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CrumbReply {
    crumb: String,
    crumb_request_field: String,
}

/// A job, or a folder of them (a multibranch project's branches).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JobInfo {
    /// Absent on a folder; false on a disabled job.
    #[serde(default)]
    pub buildable: Option<bool>,
    /// Present on a folder, even an empty one.
    #[serde(default)]
    pub jobs: Option<Vec<ChildJob>>,
    #[serde(default)]
    pub property: Vec<JobProperty>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildJob {
    /// A branch job's name is its branch, `/` encoded: `feature%2Fx`.
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProperty {
    #[serde(default)]
    pub parameter_definitions: Vec<ParameterDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParameterDefinition {
    pub name: String,
}

impl JobInfo {
    fn parameter_names(&self) -> Vec<String> {
        self.property
            .iter()
            .flat_map(|p| p.parameter_definitions.iter().map(|d| d.name.clone()))
            .collect()
    }
}

impl ChildJob {
    /// Whether this is the job for `branch` in a multibranch project.
    fn is_branch(&self, branch: &str) -> bool {
        self.name == branch
            || self.display_name.as_deref() == Some(branch)
            || self.name.replace("%2F", "/").replace("%2f", "/") == branch
    }

    /// The last `job/<name>/` of its URL, encoded as Jenkins encodes it.
    fn path_segment(&self) -> Option<&str> {
        self.url
            .trim_end_matches('/')
            .rsplit_once("/job/")
            .map(|(_, segment)| segment)
            .filter(|s| !s.is_empty())
    }
}

/// A build's parameters, from its `ParametersAction`.
#[derive(Debug, Clone, Default, Deserialize)]
struct BuildActions {
    #[serde(default)]
    actions: Vec<BuildAction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BuildAction {
    #[serde(default)]
    parameters: Vec<BuildParameter>,
}

#[derive(Debug, Clone, Deserialize)]
struct BuildParameter {
    name: String,
    /// Absent for a password or file parameter, whose value Jenkins
    /// doesn't show.
    #[serde(default)]
    value: Option<serde_json::Value>,
}

/// `queue/item/<id>/api/json`: a build asked for, until it starts.
#[derive(Debug, Clone, Default, Deserialize)]
struct QueuedBuild {
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    executable: Option<Executable>,
}

#[derive(Debug, Clone, Deserialize)]
struct Executable {
    number: u64,
}

/// What asking Jenkins for a build gave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Started {
    /// The job it is a build of.
    pub job_url: String,
    /// The build, once it left the queue.
    pub number: Option<u64>,
    /// Its page, or the queue item's while it waits.
    pub url: Option<String>,
    /// Why it is still in the queue.
    pub waiting: Option<String>,
}

impl Started {
    /// In words: "build 8 of <job>", or "queued for <job>: <why>".
    pub fn describe(&self) -> String {
        match (self.number, &self.waiting) {
            (Some(n), _) => format!("build {n} of {}", self.job_url),
            (None, Some(why)) => format!("queued for {} ({why})", self.job_url),
            (None, None) => format!("queued for {}", self.job_url),
        }
    }
}

/// A parameter value as a form sends it.
fn form_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The queue item a `Location` header names, by its id: Jenkins builds
/// the header from its configured root URL, which needn't be the one it
/// was reached at.
fn queue_item_id(location: &str) -> Option<u64> {
    let (_, rest) = location.split_once("/queue/item/")?;
    rest.trim_end_matches('/').parse().ok()
}

/// A refused write, saying which permission it most likely lacked.
fn refused_write(e: ApiError, permission: &str) -> ApiError {
    match e {
        ApiError::Refused { host, status, .. } if status == FORBIDDEN => ApiError::Refused {
            host,
            status,
            hint: format!(
                "the Jenkins user lacks {permission} on this job, or Jenkins's CSRF protection refused the request"
            ),
        },
        other => other,
    }
}

// ------------------------------------------------------------------ mapping

/// A build's result. Unstable (tests failed) is a failure, as both status
/// plugins report it.
pub fn state(build: &Build) -> State {
    if build.building {
        return State::Running;
    }
    match build.result.as_deref() {
        Some("SUCCESS") => State::Success,
        Some("UNSTABLE" | "FAILURE") => State::Failure,
        Some("ABORTED") => State::Cancelled,
        Some("NOT_BUILT") => State::Skipped,
        _ => State::Unknown,
    }
}

fn stage_state(status: &str) -> State {
    match status {
        "SUCCESS" => State::Success,
        "FAILED" | "UNSTABLE" => State::Failure,
        "ABORTED" => State::Cancelled,
        "NOT_EXECUTED" => State::Skipped,
        "IN_PROGRESS" => State::Running,
        "PAUSED_PENDING_INPUT" => State::Blocked,
        "QUEUED" => State::Waiting,
        _ => State::Unknown,
    }
}

/// A build's stages as steps. Stage View reports a stage skipped after an
/// earlier failure as failed, with that failure's error; it is reported
/// skipped here, so only the stage that failed reads as failed.
fn steps(describe: &Describe) -> Vec<Step> {
    let mut failures: Vec<&StageError> = Vec::new();
    describe
        .stages
        .iter()
        .map(|s| {
            let mut state = stage_state(&s.status);
            if let Some(error) = &s.error
                && state == State::Failure
            {
                if failures.contains(&error) {
                    state = State::Skipped;
                } else {
                    failures.push(error);
                }
            }
            Step {
                name: s.name.clone(),
                state,
                duration_seconds: (s.duration_millis > 0)
                    .then_some(s.duration_millis / MILLIS_PER_SECOND),
            }
        })
        .collect()
}

fn time_of(millis: i64) -> Option<String> {
    (millis > 0)
        .then(|| DateTime::<Utc>::from_timestamp_millis(millis))
        .flatten()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// `job` (from a status) as the build it links to reports it. A build
/// paused for input reads as blocked.
pub fn to_job(job: &Job, page: &Url, build: &Build, describe: Option<&Describe>) -> Job {
    let steps = describe.map(steps).unwrap_or_default();
    let state = match state(build) {
        State::Running if steps.iter().any(|s| s.state == State::Blocked) => State::Blocked,
        other => other,
    };
    let finished = state.is_final();
    Job {
        name: job.name.clone(),
        state,
        job_id: Some(build.number),
        task_id: None,
        started_at: time_of(build.timestamp),
        finished_at: finished
            .then(|| time_of(build.timestamp + build.duration))
            .flatten(),
        duration_seconds: finished.then_some(build.duration / MILLIS_PER_SECOND),
        source: Source::Api,
        url: Some(page.to_string()),
        steps: Job::keep_steps(state, steps),
    }
}

/// The label expression a queue item waits for, from its reason: "Waiting
/// for next available executor on ‘linux’", "‘Jenkins’ doesn’t have label
/// ‘gpu’". `&&` splits it into labels a node must all have.
fn wanted_labels(why: &str) -> Vec<String> {
    if !(why.contains("label") || why.contains("executor on")) {
        return Vec::new();
    }
    let Some(expr) = why.rsplit('‘').next().and_then(|s| s.split('’').next()) else {
        return Vec::new();
    };
    expr.split("&&")
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A node's labels, less the one naming the node itself.
fn node_labels(c: &Computer) -> Vec<String> {
    c.assigned_labels
        .iter()
        .map(|l| l.name.clone())
        .filter(|l| l != BUILT_IN_LABEL && !l.eq_ignore_ascii_case(&c.display_name))
        .collect()
}

fn to_runners(computers: &[Computer]) -> Vec<Runner> {
    computers
        .iter()
        .zip(1..)
        .map(|(c, id)| Runner {
            id,
            name: c.display_name.clone(),
            state: if c.offline {
                RunnerState::Offline
            } else if c.idle {
                RunnerState::Idle
            } else {
                RunnerState::Busy
            },
            labels: node_labels(c),
            scope: Scope::Instance,
            version: None,
        })
        .collect()
}

/// Queue items for `projects` (all of them when none is known).
fn waiting_jobs(
    items: &[QueueItem],
    base_url: &Url,
    projects: &[Url],
    runners: &[Runner],
) -> Vec<WaitingJob> {
    items
        .iter()
        .filter(|i| {
            let url = base_url.join(&i.task.url).map(|u| u.to_string());
            projects.is_empty()
                || url.is_ok_and(|u| projects.iter().any(|p| u.starts_with(p.as_str())))
        })
        .map(|i| {
            let labels = wanted_labels(i.why.as_deref().unwrap_or_default());
            let (idle, busy, offline) = runners::matching(runners, &labels);
            WaitingJob {
                name: i.task.name.clone(),
                labels,
                matching_idle: idle,
                matching_busy: busy,
                matching_offline: offline,
                hosted: false,
            }
        })
        .collect()
}

/// A token with no user to go with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedsUser;

/// A Jenkins credential as Basic auth: a token stored as `user:token`, or
/// a token with the configured user.
pub fn basic(
    credential: Option<Credential>,
    username: Option<&str>,
) -> Result<Option<Credential>, NeedsUser> {
    match credential {
        Some(Credential::Token(token)) => {
            let (user, secret) = match token.expose().split_once(':') {
                Some((u, t)) => (u.to_owned(), t.to_owned()),
                None => (
                    username.ok_or(NeedsUser)?.to_owned(),
                    token.expose().to_owned(),
                ),
            };
            Ok(Some(Credential::Basic {
                username: user,
                password: crate::secret::Secret::new(secret),
            }))
        }
        other => Ok(other),
    }
}

// ------------------------------------------------------------------ client

/// One Jenkins server, and the jobs this repository's statuses link to.
pub struct Jenkins {
    http: Http,
    base_url: Url,
    /// Projects (multibranch parents, or lone jobs) this repository builds
    /// in: what the queue is filtered to.
    projects: Vec<Url>,
}

/// What `connect` learned.
pub struct Connected {
    pub client: Jenkins,
    pub version: Option<String>,
    pub user: Option<User>,
    pub permissions: Permissions,
}

impl Jenkins {
    /// The version a server at `base_url` reports in `X-Jenkins`; `None`
    /// if it isn't Jenkins. Jenkins sends it even when it refuses.
    pub async fn version(host: &str, base_url: &Url) -> Result<Option<String>, ApiError> {
        Http::new(host, None, TokenScheme::Token)
            .header_of(&format!("{base_url}login"), VERSION_HEADER)
            .await
    }

    /// Signs in to the Jenkins at `base_url` and checks the credential can
    /// read `jobs` (the first of them; the instance when there are none).
    pub async fn connect(
        host: &str,
        base_url: &Url,
        credential: Option<Credential>,
        jobs: &[Url],
    ) -> Result<Connected, ApiError> {
        // Jenkins takes its API token as Basic auth with its user, which
        // `Bearer` would turn into a bearer token.
        let http = Http::new(host, credential, TokenScheme::Token);
        let version = http
            .header_of(&format!("{base_url}login"), VERSION_HEADER)
            .await?;
        let who: WhoAmI = http
            .get_json(&format!("{base_url}whoAmI/api/json"))
            .await
            .map_err(|e| anonymous_refusal(e, http.has_credential()))?;
        let probe = jobs.first().map_or_else(
            || format!("{base_url}api/json?tree=mode"),
            |j| format!("{j}api/json?tree=name"),
        );
        http.get_json::<serde_json::Value>(&probe)
            .await
            .map_err(|e| anonymous_refusal(e, http.has_credential()))?;
        let mut projects: Vec<Url> = jobs.iter().map(project_of).collect();
        projects.dedup();
        Ok(Connected {
            client: Jenkins {
                http,
                base_url: base_url.clone(),
                projects,
            },
            version,
            user: (!who.anonymous).then_some(User {
                login: who.name,
                site_admin: false,
            }),
            permissions: Permissions {
                pull: true,
                push: false,
                admin: false,
            },
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Build `number` of the job at `job_url`, with its stages where
    /// Stage View serves them; `None` if there is no such build.
    pub async fn build(
        &self,
        job_url: &Url,
        number: u64,
    ) -> Result<Option<(Build, Option<Describe>)>, ApiError> {
        let page = format!("{job_url}{number}/");
        let Some(build) = self
            .http
            .get_json_opt::<Build>(&format!(
                "{page}api/json?tree=number,result,building,timestamp,duration"
            ))
            .await?
        else {
            return Ok(None);
        };
        // Freestyle builds, and servers without Stage View, have no stages.
        let describe = self
            .http
            .get_json_opt::<Describe>(&format!("{page}wfapi/describe"))
            .await
            .ok()
            .flatten();
        Ok(Some((build, describe)))
    }

    /// `run` with each job whose status links to a build here replaced by
    /// what that build reports. A running build whose node is still in the
    /// queue reads as waiting.
    pub async fn refresh(&self, run: &Run) -> Result<Run, ApiError> {
        let mut fresh = run.clone();
        for job in fresh.jobs.iter_mut() {
            let Some((link, number)) = job
                .url
                .as_deref()
                .and_then(parse_link)
                .filter(|l| l.base_url == self.base_url)
                .and_then(|l| l.number.map(|n| (l, n)))
            else {
                continue;
            };
            if let Some((build, describe)) = self.build(&link.job_url, number).await? {
                let page = link
                    .job_url
                    .join(&format!("{number}/"))
                    .unwrap_or(link.job_url);
                *job = to_job(job, &page, &build, describe.as_ref());
            }
        }
        if fresh.jobs.iter().any(|j| j.state == State::Running) {
            self.mark_queued(&mut fresh.jobs).await;
        }
        ci::settle(&mut fresh);
        Ok(fresh)
    }

    /// Running jobs whose build has a node block still in the queue are
    /// waiting, not running. The queue not answering changes nothing.
    async fn mark_queued(&self, jobs: &mut [Job]) {
        let Ok(queue) = self.queue().await else {
            return;
        };
        let waiting: Vec<String> = queue
            .iter()
            .filter_map(|i| self.base_url.join(&i.task.url).ok())
            .map(|u| u.to_string())
            .collect();
        for job in jobs.iter_mut().filter(|j| {
            j.state == State::Running && j.url.as_ref().is_some_and(|u| waiting.contains(u))
        }) {
            job.state = State::Waiting;
        }
    }

    async fn queue(&self) -> Result<Vec<QueueItem>, ApiError> {
        let queue: Queue = self
            .http
            .get_json(&format!(
                "{}queue/api/json?tree=items[why,task[name,url]]",
                self.base_url
            ))
            .await?;
        Ok(queue.items)
    }

    /// The console log of the build at `page`: its end when `tail_bytes`
    /// is given. Jenkins serves it whole, so it is cut here. Returns the
    /// text and whether it is only the end.
    pub async fn console(
        &self,
        page: &str,
        tail_bytes: Option<u64>,
    ) -> Result<(String, bool), ApiError> {
        let max = tail_bytes.map_or(MAX_LOG_BYTES, |n| (n as usize).max(MIN_TAIL_LOG_BYTES));
        self.http
            .get_text_tail(&format!("{page}consoleText"), None, max)
            .await
    }

    /// The nodes, and the queue items for this repository's projects.
    pub async fn runners(&self) -> Result<RunnerReport, ApiError> {
        let set: ComputerSet = self
            .http
            .get_json(&format!(
                "{}computer/api/json?tree=computer[displayName,offline,idle,assignedLabels[name]]",
                self.base_url
            ))
            .await?;
        let runners = to_runners(&set.computer);
        let items = self.queue().await?;
        let waiting = waiting_jobs(&items, &self.base_url, &self.projects, &runners);
        let scopes = vec![ScopeOutcome {
            scope: Scope::Instance,
            endpoint: "/computer/api/json".into(),
            outcome: "listed".into(),
            runners: runners.len(),
        }];
        Ok(RunnerReport {
            summary: runners::summarize(&runners, &scopes, Some(&waiting), &[]),
            runners,
            scopes,
            waiting: Some(waiting),
            pools: Vec::new(),
            job_scopes: Vec::new(),
            active: Vec::new(),
        })
    }

    // -------------------------------------------------------------- writes

    /// The projects this repository builds in, as the statuses name them.
    pub fn projects(&self) -> &[Url] {
        &self.projects
    }

    /// POSTs `form` to `url` with the CSRF crumb, once. `permission` names
    /// what a refusal most likely lacks.
    async fn post(
        &self,
        url: &str,
        form: &[(String, String)],
        permission: &str,
    ) -> Result<FormReply, ApiError> {
        // Asking for the crumb is a read; only the POST is a write.
        let crumb = self
            .http
            .get_json_opt_with_cookies::<CrumbReply>(&format!(
                "{}crumbIssuer/api/json",
                self.base_url
            ))
            .await?;
        let mut headers = Vec::new();
        if let Some((crumb, cookie)) = crumb {
            headers.push((crumb.crumb_request_field, crumb.crumb));
            if let Some(cookie) = cookie {
                headers.push(("Cookie".to_owned(), cookie));
            }
        }
        self.http
            .post_form(url, form, &headers)
            .await
            .map_err(|e| refused_write(e, permission))
    }

    /// Stops the build at `page` (`<job>/<number>/`). A Pipeline build
    /// still waiting in the queue for a node stops too.
    pub async fn stop(&self, page: &Url) -> Result<(), ApiError> {
        self.post(&format!("{page}stop"), &[], "Job/Cancel")
            .await
            .map(|_| ())
    }

    /// The job or folder at `job_url`.
    pub async fn job(&self, job_url: &Url) -> Result<JobInfo, ApiError> {
        self.http
            .get_json(&format!(
                "{job_url}api/json?tree=buildable,jobs[name,displayName,url],property[parameterDefinitions[name]]"
            ))
            .await
    }

    /// Starts a build of the job at `job_url` with `params`, which must be
    /// parameters it defines (Jenkins ignores any others without a word),
    /// and follows it through the queue for a while.
    pub async fn start(
        &self,
        job_url: &Url,
        params: &BTreeMap<String, String>,
    ) -> Result<Started, ApiError> {
        let job = self.job(job_url).await?;
        let refuse = |reason: String| Err(ApiError::Unavailable { reason });
        if job.jobs.is_some() {
            return refuse(format!("{job_url} is a folder of jobs; name one in it"));
        }
        if job.buildable == Some(false) {
            return refuse(format!("{job_url} is disabled in Jenkins"));
        }
        let defined = job.parameter_names();
        if defined.is_empty() && !params.is_empty() {
            return refuse(format!("{job_url} takes no parameters; leave out `inputs`"));
        }
        let unknown: Vec<&str> = params
            .keys()
            .filter(|k| !defined.contains(k))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return refuse(format!(
                "{job_url} has no parameter {}; it takes {}",
                unknown.join(", "),
                defined.join(", ")
            ));
        }
        // `delay` in the query, so it can't clash with a parameter.
        let endpoint = if defined.is_empty() {
            "build"
        } else {
            "buildWithParameters"
        };
        let form: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let reply = self
            .post(
                &format!("{job_url}{endpoint}?delay=0sec"),
                &form,
                "Job/Build",
            )
            .await?;
        Ok(self
            .follow(job_url, reply.location.as_deref().and_then(queue_item_id))
            .await)
    }

    /// A build just asked for, through the queue to its number. Failing to
    /// read the queue only leaves it unknown: the build was asked for.
    async fn follow(&self, job_url: &Url, item: Option<u64>) -> Started {
        let mut started = Started {
            job_url: job_url.to_string(),
            ..Started::default()
        };
        let Some(id) = item else {
            return started;
        };
        let item_url = format!("{}queue/item/{id}/", self.base_url);
        started.url = Some(item_url.clone());
        for check in 0..QUEUE_CHECKS {
            if check > 0 {
                tokio::time::sleep(QUEUE_CHECK_PAUSE).await;
            }
            let Ok(Some(queued)) = self
                .http
                .get_json_opt::<QueuedBuild>(&format!(
                    "{item_url}api/json?tree=cancelled,why,executable[number]"
                ))
                .await
            else {
                break;
            };
            if let Some(build) = queued.executable {
                started.number = Some(build.number);
                started.url = Some(format!("{job_url}{}/", build.number));
                started.waiting = None;
                break;
            }
            if queued.cancelled {
                started.waiting = Some("cancelled in the queue".into());
                break;
            }
            started.waiting = queued.why;
        }
        started
    }

    /// Builds the job of build `number` at `job_url` again, with the
    /// parameters that build had. Core Jenkins has no rebuild, so this is
    /// a new build; a branch job builds its branch as it is now.
    pub async fn rebuild(&self, job_url: &Url, number: u64) -> Result<Started, ApiError> {
        let had: BuildActions = self
            .http
            .get_json(&format!(
                "{job_url}{number}/api/json?tree=actions[parameters[name,value]]"
            ))
            .await?;
        let mut params = BTreeMap::new();
        for p in had.actions.iter().flat_map(|a| &a.parameters) {
            let Some(value) = p.value.as_ref().and_then(form_value) else {
                return Err(ApiError::Unavailable {
                    reason: format!(
                        "build {number} of {job_url} had a parameter ({}) whose value Jenkins doesn't show, a password or a file; rebuild it in Jenkins",
                        p.name
                    ),
                });
            };
            params.insert(p.name.clone(), value);
        }
        self.start(job_url, &params).await
    }

    /// The job `name` names: a URL on this Jenkins, a path of job names
    /// (`acme/widgets`), or, when `None`, the one project this repository's
    /// statuses link to.
    pub fn job_named(&self, name: Option<&str>) -> Result<Url, ApiError> {
        let refuse = |reason: String| Err(ApiError::Unavailable { reason });
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return match self.projects.as_slice() {
                [one] => Ok(one.clone()),
                [] => refuse(
                    "no Jenkins job is known for this repository; name it with `workflow`, as folder/job".into(),
                ),
                many => refuse(format!(
                    "this repository builds in more than one Jenkins job; name one with `workflow`: {}",
                    many.iter().map(Url::as_str).collect::<Vec<_>>().join(", ")
                )),
            };
        };
        if name.contains("://") {
            return match parse_link(name) {
                Some(link) if link.base_url == self.base_url => Ok(link.job_url),
                _ => refuse(format!("{name} isn't a job on {}", self.base_url)),
            };
        }
        let path: String = name
            .split('/')
            .filter(|s| !s.is_empty() && *s != "job")
            .map(|s| format!("job/{s}/"))
            .collect();
        self.base_url
            .join(&path)
            .or_else(|_| refuse(format!("{name} isn't a job name")))
    }

    /// Starts a build on `branch` of `job_url`: its branch job when it is
    /// a multibranch project. A lone job builds what it is set up to, so
    /// an `explicit` branch is refused there. Returns what started, and
    /// notes.
    pub async fn dispatch(
        &self,
        job_url: &Url,
        branch: &str,
        explicit: bool,
        params: &BTreeMap<String, String>,
    ) -> Result<(Started, Vec<String>), ApiError> {
        let job = self.job(job_url).await?;
        let mut notes = Vec::new();
        let target = match &job.jobs {
            Some(children) => {
                let child = children.iter().find(|c| c.is_branch(branch));
                let segment = child.and_then(ChildJob::path_segment).ok_or_else(|| {
                    ApiError::Unavailable {
                        reason: format!(
                            "{job_url} has no job for branch {branch}; Jenkins makes one once it has scanned the branch"
                        ),
                    }
                })?;
                job_url
                    .join(&format!("job/{segment}/"))
                    .map_err(|e| ApiError::Unavailable {
                        reason: format!("{job_url}: {e}"),
                    })?
            }
            None if explicit => {
                return Err(ApiError::Unavailable {
                    reason: format!(
                        "{job_url} isn't a multibranch project: it builds what it is set up to, so `ref` can't choose a branch; leave `ref` out"
                    ),
                });
            }
            None => {
                notes.push(format!(
                    "{job_url} builds what it is set up to; `ref` doesn't apply"
                ));
                job_url.clone()
            }
        };
        let started = self.start(&target, params).await?;
        if let Some(why) = &started.waiting {
            notes.push(format!("still in Jenkins's queue: {why}"));
        }
        Ok((started, notes))
    }
}

/// Jenkins answers an anonymous caller it won't let read with 403, which
/// reads as a credential lacking access; there is no credential at all.
fn anonymous_refusal(e: ApiError, has_credential: bool) -> ApiError {
    match e {
        ApiError::Refused { host, status, .. } if status == FORBIDDEN && !has_credential => {
            ApiError::Refused {
                host,
                status,
                hint: "no credential was found for this host, and Jenkins doesn't let anonymous users read".into(),
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(raw: &str) -> Option<(String, String, Option<u64>)> {
        parse_link(raw).map(|l| (l.base_url.to_string(), l.job_url.to_string(), l.number))
    }

    #[test]
    fn links_name_root_job_and_build() {
        assert_eq!(
            link("https://ci.example.com/job/acme/job/widgets/job/main/42/display/redirect"),
            Some((
                "https://ci.example.com/".into(),
                "https://ci.example.com/job/acme/job/widgets/job/main/".into(),
                Some(42)
            ))
        );
        assert_eq!(
            link("https://ci.example.com:8443/jenkins/job/nightly/7/console?x=1"),
            Some((
                "https://ci.example.com:8443/jenkins/".into(),
                "https://ci.example.com:8443/jenkins/job/nightly/".into(),
                Some(7)
            ))
        );
        // A queued build links to its job.
        assert_eq!(
            link("https://ci.example.com/job/acme/job/widgets/job/PR-3/"),
            Some((
                "https://ci.example.com/".into(),
                "https://ci.example.com/job/acme/job/widgets/job/PR-3/".into(),
                None
            ))
        );
        // Encoded branch names stay encoded.
        assert_eq!(
            link("https://ci.example.com/job/widgets/job/feature%252Fx/3/").map(|l| l.1),
            Some("https://ci.example.com/job/widgets/job/feature%252Fx/".into())
        );
        assert_eq!(link("https://ci.example.com/repos/4/pipeline/9/1"), None);
        assert_eq!(link("https://ci.example.com/job/"), None);
    }

    #[test]
    fn a_project_is_a_jobs_folder() {
        let project = |u: &str| project_of(&Url::parse(u).unwrap()).to_string();
        assert_eq!(
            project("https://ci.example.com/job/acme/job/widgets/job/main/"),
            "https://ci.example.com/job/acme/job/widgets/"
        );
        assert_eq!(
            project("https://ci.example.com/jenkins/job/nightly/"),
            "https://ci.example.com/jenkins/job/nightly/"
        );
    }

    fn build(result: Option<&str>, building: bool) -> Build {
        Build {
            number: 5,
            result: result.map(str::to_owned),
            building,
            timestamp: 1_700_000_000_000,
            duration: if building { 0 } else { 90_500 },
        }
    }

    fn status_job() -> Job {
        Job {
            name: "head".into(),
            state: State::Running,
            job_id: None,
            task_id: None,
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            source: Source::Statuses,
            url: Some("https://ci.example.com/job/w/job/main/5/display/redirect".into()),
            steps: Vec::new(),
        }
    }

    #[test]
    fn results_map_to_states() {
        let cases = [
            (Some("SUCCESS"), false, State::Success),
            (Some("UNSTABLE"), false, State::Failure),
            (Some("FAILURE"), false, State::Failure),
            (Some("ABORTED"), false, State::Cancelled),
            (Some("NOT_BUILT"), false, State::Skipped),
            (None, true, State::Running),
            (None, false, State::Unknown),
        ];
        for (result, building, want) in cases {
            assert_eq!(state(&build(result, building)), want, "{result:?}");
        }
    }

    #[test]
    fn a_build_becomes_a_job_with_its_stages() {
        let describe: Describe = serde_json::from_value(serde_json::json!({
            "stages": [
                { "name": "Build", "status": "SUCCESS", "durationMillis": 95 },
                { "name": "Test", "status": "FAILED", "durationMillis": 2195,
                  "error": { "message": "boom", "type": "hudson.AbortException" } },
                { "name": "Deploy", "status": "FAILED", "durationMillis": 69,
                  "error": { "message": "boom", "type": "hudson.AbortException" } }
            ]
        }))
        .unwrap();
        let page = Url::parse("https://ci.example.com/job/w/job/main/5/").unwrap();
        let job = to_job(
            &status_job(),
            &page,
            &build(Some("FAILURE"), false),
            Some(&describe),
        );
        assert_eq!(job.name, "head");
        assert_eq!(job.state, State::Failure);
        assert_eq!(job.job_id, Some(5));
        assert_eq!(job.source, Source::Api);
        assert_eq!(job.started_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(job.finished_at.as_deref(), Some("2023-11-14T22:14:50Z"));
        assert_eq!(job.duration_seconds, Some(90));
        assert_eq!(job.url.as_deref(), Some(page.as_str()));
        let steps: Vec<_> = job
            .steps
            .iter()
            .map(|s| (s.name.as_str(), s.state, s.duration_seconds))
            .collect();
        assert_eq!(
            steps,
            [
                ("Build", State::Success, Some(0)),
                ("Test", State::Failure, Some(2)),
                ("Deploy", State::Skipped, Some(0)),
            ]
        );
    }

    #[test]
    fn a_build_waiting_for_input_is_blocked() {
        let describe: Describe = serde_json::from_value(serde_json::json!({
            "stages": [{ "name": "Approve", "status": "PAUSED_PENDING_INPUT" }]
        }))
        .unwrap();
        let page = Url::parse("https://ci.example.com/job/w/job/main/5/").unwrap();
        let job = to_job(&status_job(), &page, &build(None, true), Some(&describe));
        assert_eq!(job.state, State::Blocked);
        assert_eq!(job.finished_at, None);
    }

    #[test]
    fn queue_reasons_name_labels() {
        assert_eq!(
            wanted_labels("Waiting for next available executor on ‘linux && docker’"),
            ["linux", "docker"]
        );
        assert_eq!(wanted_labels("‘Jenkins’ doesn’t have label ‘gpu’"), ["gpu"]);
        assert!(wanted_labels("Waiting for next available executor").is_empty());
        assert!(wanted_labels("Build #3 is already in progress (ETA: 1 min)").is_empty());
    }

    #[test]
    fn nodes_and_queue_become_runners_and_waiting_jobs() {
        let set: ComputerSet = serde_json::from_value(serde_json::json!({
            "computer": [
                { "displayName": "Built-In Node", "idle": false, "offline": false,
                  "assignedLabels": [{ "name": "built-in" }, { "name": "linux" }, { "name": "docker" }] },
                { "displayName": "arm-1", "idle": true, "offline": true,
                  "assignedLabels": [{ "name": "arm-1" }, { "name": "arm" }] }
            ]
        }))
        .unwrap();
        let runners = to_runners(&set.computer);
        let summary: Vec<_> = runners
            .iter()
            .map(|r| (r.id, r.name.as_str(), r.state, r.labels.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    1,
                    "Built-In Node",
                    RunnerState::Busy,
                    vec!["linux".to_owned(), "docker".to_owned()]
                ),
                (2, "arm-1", RunnerState::Offline, vec!["arm".to_owned()]),
            ]
        );
        let queue: Queue = serde_json::from_value(serde_json::json!({
            "items": [
                { "why": "‘Jenkins’ doesn’t have label ‘arm’",
                  "task": { "name": "part of widgets » main #4", "url": "job/acme/job/widgets/job/main/4/" } },
                { "why": "Waiting for next available executor",
                  "task": { "name": "other", "url": "job/other/" } }
            ]
        }))
        .unwrap();
        let base = Url::parse("https://ci.example.com/").unwrap();
        let projects = [Url::parse("https://ci.example.com/job/acme/job/widgets/").unwrap()];
        let waiting = waiting_jobs(&queue.items, &base, &projects, &runners);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].name, "part of widgets » main #4");
        assert_eq!(waiting[0].labels, ["arm"]);
        assert_eq!(waiting[0].matching_offline, 1);
        assert_eq!(waiting_jobs(&queue.items, &base, &[], &runners).len(), 2);
    }

    #[test]
    fn a_branch_job_is_found_by_its_branch() {
        let child = |name: &str, display: Option<&str>, url: &str| ChildJob {
            name: name.into(),
            display_name: display.map(str::to_owned),
            url: url.into(),
        };
        let feature = child(
            "feature%2Fx",
            Some("feature/x"),
            "https://jenkins.example.com/job/acme/job/widgets/job/feature%252Fx/",
        );
        assert!(feature.is_branch("feature/x"));
        assert!(feature.is_branch("feature%2Fx"));
        assert!(!feature.is_branch("feature"));
        assert_eq!(feature.path_segment(), Some("feature%252Fx"));
        // Older Jenkins leaves out the display name.
        let bare = child(
            "release%2F2",
            None,
            "https://ci.example.com/job/w/job/release%252F2/",
        );
        assert!(bare.is_branch("release/2"));
        assert_eq!(child("main", None, "").path_segment(), None);
    }

    #[test]
    fn queue_items_and_parameter_values() {
        assert_eq!(
            queue_item_id("https://jenkins.example.com/queue/item/12/"),
            Some(12)
        );
        assert_eq!(
            queue_item_id("http://localhost:8080/jenkins/queue/item/7"),
            Some(7)
        );
        assert_eq!(queue_item_id("https://jenkins.example.com/job/x/"), None);
        assert_eq!(
            form_value(&serde_json::json!("a b")).as_deref(),
            Some("a b")
        );
        assert_eq!(
            form_value(&serde_json::json!(true)).as_deref(),
            Some("true")
        );
        assert_eq!(form_value(&serde_json::json!(3)).as_deref(), Some("3"));
        assert_eq!(form_value(&serde_json::Value::Null), None);
        let started = Started {
            job_url: "https://ci.example.com/job/w/".into(),
            waiting: Some("Waiting for next available executor".into()),
            ..Started::default()
        };
        assert_eq!(
            started.describe(),
            "queued for https://ci.example.com/job/w/ (Waiting for next available executor)"
        );
    }

    #[test]
    fn a_job_is_named_by_path_url_or_the_one_project() {
        let jenkins = |projects: &[&str]| Jenkins {
            http: Http::new("ci.example.com", None, TokenScheme::Token),
            base_url: Url::parse("https://ci.example.com/jenkins/").unwrap(),
            projects: projects.iter().map(|p| Url::parse(p).unwrap()).collect(),
        };
        let one = jenkins(&["https://ci.example.com/jenkins/job/acme/job/widgets/"]);
        let named = |j: &Jenkins, n: Option<&str>| j.job_named(n).map(|u| u.to_string());
        assert_eq!(
            named(&one, None).unwrap(),
            "https://ci.example.com/jenkins/job/acme/job/widgets/"
        );
        assert_eq!(
            named(&one, Some("acme/deploy")).unwrap(),
            "https://ci.example.com/jenkins/job/acme/job/deploy/"
        );
        assert_eq!(
            named(&one, Some("job/acme/job/deploy/")).unwrap(),
            "https://ci.example.com/jenkins/job/acme/job/deploy/"
        );
        assert_eq!(
            named(&one, Some("https://ci.example.com/jenkins/job/nightly/3/")).unwrap(),
            "https://ci.example.com/jenkins/job/nightly/"
        );
        assert!(named(&one, Some("https://other.example.com/job/nightly/")).is_err());
        assert!(
            named(&jenkins(&[]), None)
                .unwrap_err()
                .to_string()
                .contains("name it with `workflow`")
        );
    }

    #[test]
    fn a_token_needs_a_user() {
        use crate::secret::Secret;
        let token = |t: &str| Some(Credential::Token(Secret::new(t)));
        let user_of = |c: Option<Credential>| match c {
            Some(Credential::Basic { username, password }) => {
                Some((username, password.expose().to_owned()))
            }
            _ => None,
        };
        assert_eq!(
            basic(token("dev:abc"), None).map(user_of),
            Ok(Some(("dev".into(), "abc".into())))
        );
        assert_eq!(
            basic(token("abc"), Some("dev")).map(user_of),
            Ok(Some(("dev".into(), "abc".into())))
        );
        assert_eq!(basic(token("abc"), None).map(user_of), Err(NeedsUser));
        assert_eq!(basic(None, None).map(user_of), Ok(None));
    }
}
