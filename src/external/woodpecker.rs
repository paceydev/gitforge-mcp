//! Woodpecker CI through its own REST API.
//!
//! Woodpecker posts one commit status per workflow, linking to
//! `<host>/repos/<repo id>/pipeline/<number>/<workflow pid>` (every release
//! since 1.0). The repo id and pipeline number in that link are what every
//! endpoint here takes. A pipeline is a run, its workflows are jobs (one per
//! status, as `ci::external_runs` already groups them) and their children
//! are steps.
//!
//! Field names and endpoints are as in Woodpecker 3.20's `server/model` and
//! `server/router/api.go`, unchanged since 3.0.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

use crate::auth::Credential;
use crate::ci::{Dispatched, Job, Run, Source, State, Step};
use crate::forge::http::{ApiError, Http, TokenScheme};
use crate::runners::{
    self, ActiveJob, ActiveJobs, Activity, Runner, RunnerReport, RunnerState, Scope, ScopeJobs,
    ScopeOutcome,
};
use crate::site::{Permissions, User};

/// An agent that hasn't reported in this long is offline. Agents report
/// their health every 10 seconds.
const OFFLINE_AFTER_SECONDS: i64 = 60;

/// Task labels every task carries, which say where it came from rather
/// than what it needs.
const ORIGIN_LABELS: [&str; 2] = ["repo", "org-id"];

/// Log entry types that are output (stdout and stderr), not exit codes,
/// metadata or progress.
const OUTPUT_ENTRY_TYPES: [i32; 2] = [0, 1];

/// What a status link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Woodpecker's web root, with any path prefix it is served under.
    pub base_url: Url,
    pub repo_id: u64,
    pub number: u64,
}

/// Reads `<base>/repos/<repo id>/pipeline/<number>[/<workflow pid>]`.
pub fn parse_link(raw: &str) -> Option<Link> {
    let url = Url::parse(raw).ok()?;
    let path = url.path();
    let at = path.rfind("/repos/")?;
    let mut parts = path[at + "/repos/".len()..].split('/');
    let repo_id = parts.next()?.parse().ok()?;
    if parts.next()? != "pipeline" {
        return None;
    }
    let number = parts.next()?.parse().ok()?;
    let mut base_url = url.clone();
    base_url.set_path(&format!("{}/", &path[..at]));
    base_url.set_query(None);
    base_url.set_fragment(None);
    Some(Link {
        base_url,
        repo_id,
        number,
    })
}

// ------------------------------------------------------------------- API

#[derive(Debug, Deserialize)]
pub struct VersionInfo {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    login: String,
    #[serde(default)]
    admin: bool,
}

#[derive(Debug, Deserialize)]
struct ApiRepo {
    id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pipeline {
    pub id: u64,
    pub number: u64,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default, rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub finished: i64,
    #[serde(default)]
    pub workflows: Vec<Workflow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workflow {
    pub id: u64,
    #[serde(default)]
    pub pid: u64,
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub finished: i64,
    #[serde(default)]
    pub children: Vec<ApiStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiStep {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub finished: i64,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    #[serde(default)]
    time: i64,
    /// Base64, as Go encodes a byte slice.
    #[serde(default)]
    data: String,
    #[serde(default, rename = "type")]
    kind: i32,
}

#[derive(Debug, Deserialize)]
struct Agent {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    last_contact: i64,
    #[serde(default)]
    no_schedule: bool,
    #[serde(default)]
    custom_labels: Option<BTreeMap<String, String>>,
    /// -1 for an agent every repository may use.
    #[serde(default)]
    org_id: i64,
}

#[derive(Debug, Deserialize)]
struct Task {
    #[serde(default)]
    name: String,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    agent_id: u64,
    #[serde(default)]
    repo_id: u64,
}

#[derive(Debug, Deserialize)]
struct QueueInfo {
    #[serde(default)]
    pending: Option<Vec<Task>>,
    #[serde(default)]
    running: Option<Vec<Task>>,
}

// ------------------------------------------------------------------- model

/// Woodpecker's states in this crate's terms. `killed` is a step stopped by
/// a cancel; `declined` a pipeline an approver refused, so nothing ran and
/// it must not read as a pass.
pub fn state(raw: &str) -> State {
    match raw {
        "success" => State::Success,
        "failure" | "error" => State::Failure,
        "killed" | "canceled" | "declined" => State::Cancelled,
        "skipped" => State::Skipped,
        "pending" | "created" => State::Waiting,
        "blocked" => State::Blocked,
        "running" => State::Running,
        _ => State::Unknown,
    }
}

/// A workflow's (or step's) state within its pipeline. Once a pipeline is
/// cancelled, Woodpecker calls it killed at once, while a workflow the agent
/// is still stopping can read running for minutes: the pipeline decides.
fn workflow_state(pipeline: State, raw: &str) -> State {
    match state(raw) {
        unfinished if pipeline == State::Cancelled && !unfinished.is_final() => State::Cancelled,
        other => other,
    }
}

fn time(unix: i64) -> Option<String> {
    (unix > 0)
        .then(|| DateTime::<Utc>::from_timestamp(unix, 0))
        .flatten()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn seconds(started: i64, finished: i64) -> Option<i64> {
    (started > 0 && finished >= started).then_some(finished - started)
}

fn steps(workflow: &Workflow, pipeline: State) -> Vec<Step> {
    workflow
        .children
        .iter()
        .map(|s| Step {
            name: s.name.clone(),
            state: workflow_state(pipeline, &s.state),
            duration_seconds: seconds(s.started, s.finished),
        })
        .collect()
}

/// A pipeline as a run: one job per workflow, with its steps.
pub fn to_run(pipeline: &Pipeline, link: &Link, system: &str) -> Run {
    let page = |pid: Option<u64>| {
        let mut url = format!(
            "{}repos/{}/pipeline/{}",
            link.base_url, link.repo_id, pipeline.number
        );
        if let Some(pid) = pid {
            url.push_str(&format!("/{pid}"));
        }
        url
    };
    let run_state = state(&pipeline.status);
    let jobs = pipeline
        .workflows
        .iter()
        .map(|w| {
            let job_state = workflow_state(run_state, &w.state);
            Job {
                name: w.name.clone(),
                state: job_state,
                job_id: Some(w.id),
                task_id: None,
                started_at: time(w.started),
                finished_at: time(w.finished),
                duration_seconds: seconds(w.started, w.finished),
                source: Source::Api,
                url: Some(page(Some(w.pid))),
                steps: Job::keep_steps(job_state, steps(w, run_state)),
            }
        })
        .collect();
    Run {
        id: pipeline.id,
        number: pipeline.number,
        attempt: None,
        workflow: system.to_owned(),
        title: pipeline
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        event: pipeline.event.clone(),
        git_ref: if pipeline.git_ref.is_empty() {
            pipeline.branch.clone()
        } else {
            pipeline.git_ref.clone()
        },
        sha: pipeline.commit.clone(),
        state: run_state,
        url: Some(page(None)),
        started_at: time(pipeline.started),
        finished_at: time(pipeline.finished),
        duration_seconds: seconds(pipeline.started, pipeline.finished),
        jobs,
        external: Some(system.to_owned()),
    }
}

/// The steps of workflow `job_id` whose logs to read: the failed ones, or
/// every step when none failed.
pub fn log_steps(pipeline: &Pipeline, job_id: u64) -> Vec<&ApiStep> {
    let Some(workflow) = pipeline.workflows.iter().find(|w| w.id == job_id) else {
        return Vec::new();
    };
    let failed: Vec<&ApiStep> = workflow
        .children
        .iter()
        .filter(|s| state(&s.state).is_failed())
        .collect();
    if failed.is_empty() {
        workflow.children.iter().collect()
    } else {
        failed
    }
}

/// A step's log as text, each line after the time it was written, the way
/// runner logs read elsewhere.
fn log_text(step: &ApiStep, entries: &[LogEntry]) -> String {
    use base64::Engine as _;
    let mut text = format!("--- step {} ({}) ---\n", step.name, step.state);
    for entry in entries
        .iter()
        .filter(|e| OUTPUT_ENTRY_TYPES.contains(&e.kind))
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(entry.data.as_bytes())
            .unwrap_or_else(|_| entry.data.clone().into_bytes());
        let line = String::from_utf8_lossy(&bytes);
        if let Some(at) = time(step.started + entry.time) {
            text.push_str(&at);
            text.push(' ');
        }
        text.push_str(line.trim_end_matches('\n'));
        text.push('\n');
    }
    text
}

fn runner_state(agent: &Agent, busy: &[u64], now: i64) -> RunnerState {
    if now - agent.last_contact > OFFLINE_AFTER_SECONDS || agent.no_schedule {
        RunnerState::Offline
    } else if busy.contains(&agent.id) {
        RunnerState::Busy
    } else {
        RunnerState::Idle
    }
}

fn agent_labels(agent: &Agent) -> Vec<String> {
    let mut labels = vec![
        format!("platform={}", agent.platform),
        format!("backend={}", agent.backend),
    ];
    labels.extend(
        agent
            .custom_labels
            .iter()
            .flatten()
            .map(|(k, v)| format!("{k}={v}")),
    );
    labels
}

/// What a task needs of an agent: its labels, less where it came from and
/// the ones any agent satisfies.
fn task_labels(task: &Task) -> Vec<String> {
    task.labels
        .iter()
        .flatten()
        .filter(|(k, v)| !ORIGIN_LABELS.contains(&k.as_str()) && !v.is_empty() && *v != "*")
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

fn to_runners(agents: &[Agent], busy: &[u64], now: i64) -> Vec<Runner> {
    agents
        .iter()
        .map(|a| Runner {
            id: a.id,
            name: a.name.clone(),
            state: runner_state(a, busy, now),
            labels: agent_labels(a),
            scope: if a.org_id > 0 {
                Scope::Owner
            } else {
                Scope::Instance
            },
            version: (!a.version.is_empty()).then(|| a.version.clone()),
        })
        .collect()
}

fn waiting_jobs(pending: &[Task], repo_id: u64) -> Vec<(String, Vec<String>)> {
    pending
        .iter()
        .filter(|t| t.repo_id == repo_id)
        .map(|t| (t.name.clone(), task_labels(t)))
        .collect()
}

/// Every task waiting and running on the server, each named by the
/// repository its `repo` label names, and the agent running it.
fn active_jobs(queue: &QueueInfo, agents: &[Agent]) -> Vec<ActiveJob> {
    let waiting = queue
        .pending
        .iter()
        .flatten()
        .map(|t| (t, Activity::Waiting));
    let running = queue
        .running
        .iter()
        .flatten()
        .map(|t| (t, Activity::Running));
    waiting
        .chain(running)
        .enumerate()
        .map(|(i, (t, activity))| ActiveJob {
            id: i as u64,
            name: t.name.clone(),
            repo: t
                .labels
                .as_ref()
                .and_then(|l| l.get("repo"))
                .filter(|r| !r.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("repository #{}", t.repo_id)),
            labels: task_labels(t),
            activity,
            runner: (activity == Activity::Running).then(|| {
                agents
                    .iter()
                    .find(|a| a.id == t.agent_id)
                    .map_or_else(|| format!("agent #{}", t.agent_id), |a| a.name.clone())
            }),
            queued_at: None,
        })
        .collect()
}

// ------------------------------------------------------------------ client

/// The signed-in user, when there is a credential.
async fn signed_in_user(http: &Http, base_url: &Url) -> Result<Option<User>, ApiError> {
    if !http.has_credential() {
        return Ok(None);
    }
    let u: ApiUser = http.get_json(&format!("{base_url}api/user")).await?;
    Ok(Some(User {
        login: u.login,
        site_admin: u.admin,
    }))
}

/// The repository's id: as looked up by name, else from a status link.
fn chosen_repo_id(
    looked_up: Option<ApiRepo>,
    linked: Option<u64>,
    host: &str,
    full_name: &str,
) -> Result<u64, ApiError> {
    looked_up
        .map(|r| r.id)
        .or(linked)
        .ok_or_else(|| ApiError::NotFound {
            host: host.to_owned(),
            path: format!("/api/repos/lookup/{full_name}"),
        })
}

/// The caller's permissions. Anonymous callers have no permissions entry;
/// a public repository they could look up is readable.
fn permissions(
    asked: Result<Permissions, ApiError>,
    signed_in: bool,
) -> Result<Permissions, ApiError> {
    match asked {
        Ok(p) => Ok(p),
        Err(_) if !signed_in => Ok(Permissions {
            pull: true,
            push: false,
            admin: false,
        }),
        Err(e) => Err(e),
    }
}

fn trigger_body(
    branch: &str,
    variables: &BTreeMap<String, String>,
    workflow: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "branch": branch, "variables": variables });
    if let Some(workflow) = workflow {
        body["workflow"] = workflow.into();
    }
    body
}

/// The pipeline a manual trigger started, if Woodpecker says (it answers
/// 204 when no workflow matched the branch).
fn dispatched(started: Option<serde_json::Value>, base_url: &Url, repo_id: u64) -> Dispatched {
    let pipeline: Option<Pipeline> = started.and_then(|b| serde_json::from_value(b).ok());
    Dispatched {
        run_id: pipeline.as_ref().map(|p| p.id),
        run_number: pipeline.as_ref().map(|p| p.number),
        url: pipeline
            .as_ref()
            .map(|p| format!("{base_url}repos/{repo_id}/pipeline/{}", p.number)),
    }
}

/// One Woodpecker server and one repository on it.
pub struct Woodpecker {
    http: Http,
    base_url: Url,
    repo_id: u64,
}

/// What `connect` learned.
pub struct Connected {
    pub client: Woodpecker,
    pub version: VersionInfo,
    pub user: Option<User>,
    pub permissions: Permissions,
}

impl Woodpecker {
    fn api(&self, path: &str) -> String {
        format!("{}api/repos/{}{path}", self.base_url, self.repo_id)
    }

    /// The version Woodpecker reports, which also says whether `base_url`
    /// is Woodpecker at all: its `source` names the project.
    pub async fn version(host: &str, base_url: &Url) -> Result<VersionInfo, ApiError> {
        Http::new(host, None, TokenScheme::Bearer)
            .get_json(&format!("{base_url}version"))
            .await
    }

    /// Finds `full_name` on the Woodpecker at `base_url` (falling back to
    /// `repo_id` from a status link) and what the credential may do there.
    pub async fn connect(
        host: &str,
        base_url: &Url,
        credential: Option<Credential>,
        full_name: &str,
        repo_id: Option<u64>,
    ) -> Result<Connected, ApiError> {
        let http = Http::new(host, credential, TokenScheme::Bearer);
        let version = Self::version(host, base_url).await?;
        let user = signed_in_user(&http, base_url).await?;
        let looked_up: Option<ApiRepo> = http
            .get_json_opt(&format!("{base_url}api/repos/lookup/{full_name}"))
            .await?;
        let client = Woodpecker {
            http,
            base_url: base_url.clone(),
            repo_id: chosen_repo_id(looked_up, repo_id, host, full_name)?,
        };
        let asked = client
            .http
            .get_json::<Permissions>(&client.api("/permissions"))
            .await;
        let permissions = permissions(asked, user.is_some())?;
        Ok(Connected {
            client,
            version,
            user,
            permissions,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn repo_id(&self) -> u64 {
        self.repo_id
    }

    pub async fn pipeline(&self, number: u64) -> Result<Option<Pipeline>, ApiError> {
        self.http
            .get_json_opt(&self.api(&format!("/pipelines/{number}")))
            .await
    }

    /// The logs of a workflow's failed steps (or all its steps), as one
    /// text, each step under a header.
    pub async fn job_log(&self, number: u64, job_id: u64) -> Result<String, ApiError> {
        let pipeline = self
            .pipeline(number)
            .await?
            .ok_or_else(|| ApiError::NotFound {
                host: self.http_host(),
                path: format!("pipeline {number}"),
            })?;
        let mut text = String::new();
        for step in log_steps(&pipeline, job_id) {
            // A step that never wrote anything has `null` for a log.
            let entries: Vec<LogEntry> = self
                .http
                .get_json_opt::<Option<Vec<LogEntry>>>(
                    &self.api(&format!("/logs/{number}/{}", step.id)),
                )
                .await?
                .flatten()
                .unwrap_or_default();
            text.push_str(&log_text(step, &entries));
        }
        Ok(text)
    }

    fn http_host(&self) -> String {
        self.base_url.host_str().unwrap_or_default().to_owned()
    }

    pub async fn cancel(&self, number: u64) -> Result<(), ApiError> {
        self.http
            .post_json(
                &self.api(&format!("/pipelines/{number}/cancel")),
                &serde_json::json!({}),
            )
            .await
            .map(|_| ())
    }

    /// Restarts a whole pipeline. Woodpecker starts it as a new pipeline
    /// with the next number.
    pub async fn restart(&self, number: u64) -> Result<Option<Pipeline>, ApiError> {
        let body = self
            .http
            .post_json(
                &self.api(&format!("/pipelines/{number}")),
                &serde_json::json!({}),
            )
            .await?;
        Ok(body.and_then(|b| serde_json::from_value(b).ok()))
    }

    /// Starts a manual pipeline on `branch`'s head, with `variables` in its
    /// environment, and only `workflow` (and what it depends on) if given.
    pub async fn trigger(
        &self,
        branch: &str,
        variables: &BTreeMap<String, String>,
        workflow: Option<&str>,
    ) -> Result<Dispatched, ApiError> {
        let body = trigger_body(branch, variables, workflow);
        let started = self.http.post_json(&self.api("/pipelines"), &body).await?;
        Ok(dispatched(started, &self.base_url, self.repo_id))
    }

    /// Agents and the tasks waiting for one. Both need a Woodpecker admin.
    pub async fn runners(&self) -> Result<RunnerReport, ApiError> {
        let agents_url = format!("{}api/agents", self.base_url);
        let queue_url = format!("{}api/queue/info", self.base_url);
        let agents: Vec<Agent> = self.http.get_json(&agents_url).await?;
        let queue: QueueInfo = self.http.get_json(&queue_url).await?;
        let busy: Vec<u64> = queue.running.iter().flatten().map(|t| t.agent_id).collect();
        let runners = to_runners(&agents, &busy, Utc::now().timestamp());
        let waiting = waiting_jobs(queue.pending.as_deref().unwrap_or_default(), self.repo_id);
        let scopes = vec![ScopeOutcome {
            scope: Scope::Instance,
            endpoint: "/api/agents".into(),
            outcome: "listed".into(),
            runners: runners.len(),
        }];
        let active = ActiveJobs::of(vec![ScopeJobs::listed(
            Scope::Instance,
            "/api/queue/info".into(),
            active_jobs(&queue, &agents),
        )]);
        Ok(runners::report(
            runners,
            scopes,
            Some(waiting),
            active,
            |_| false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline() -> Pipeline {
        serde_json::from_value(serde_json::json!({
            "id": 900, "number": 42, "event": "push", "status": "failure",
            "commit": "abc", "branch": "main", "ref": "refs/heads/main",
            "message": "Fix it\n\nbody", "started": 1_700_000_000, "finished": 1_700_000_090,
            "workflows": [
                { "id": 7, "pid": 1, "name": "build", "state": "success",
                  "started": 1_700_000_000, "finished": 1_700_000_030,
                  "children": [{ "id": 70, "name": "clone", "state": "success" }] },
                { "id": 8, "pid": 2, "name": "test", "state": "failure",
                  "started": 1_700_000_030, "finished": 1_700_000_090,
                  "children": [
                      { "id": 80, "name": "clone", "state": "success", "started": 1_700_000_030, "finished": 1_700_000_031 },
                      { "id": 81, "name": "unit", "state": "failure", "started": 1_700_000_031, "finished": 1_700_000_090 }
                  ] }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn links_name_the_repo_and_pipeline() {
        let link = parse_link("https://ci.example.com/ci/repos/12/pipeline/42/2?x=1").unwrap();
        assert_eq!(link.base_url.as_str(), "https://ci.example.com/ci/");
        assert_eq!((link.repo_id, link.number), (12, 42));
        assert_eq!(
            parse_link("https://ci.example.com/repos/12/pipeline/42")
                .unwrap()
                .base_url
                .as_str(),
            "https://ci.example.com/"
        );
        assert!(parse_link("https://git.example.com/acme/widgets/actions/runs/3/jobs/0").is_none());
        assert!(parse_link("https://ci.example.com/repos/x/pipeline/1").is_none());
    }

    #[test]
    fn a_pipeline_is_a_run_of_workflows_with_steps() {
        let link = parse_link("https://ci.example.com/repos/12/pipeline/42/1").unwrap();
        let run = to_run(&pipeline(), &link, "ci/woodpecker/push");
        assert_eq!((run.id, run.number, run.state), (900, 42, State::Failure));
        assert_eq!(run.title, "Fix it");
        assert_eq!(run.duration_seconds, Some(90));
        assert_eq!(run.started_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(
            run.url.as_deref(),
            Some("https://ci.example.com/repos/12/pipeline/42")
        );
        let names: Vec<_> = run
            .jobs
            .iter()
            .map(|j| (j.name.as_str(), j.state))
            .collect();
        assert_eq!(names, [("build", State::Success), ("test", State::Failure)]);
        // A passed workflow's steps say nothing; a failed one's say where.
        assert!(run.jobs[0].steps.is_empty());
        assert_eq!(run.jobs[1].steps[1].name, "unit");
        assert_eq!(run.jobs[1].steps[1].duration_seconds, Some(59));
        assert_eq!(
            run.jobs[1].url.as_deref(),
            Some("https://ci.example.com/repos/12/pipeline/42/2")
        );
        assert_eq!(run.jobs[1].source, Source::Api);
    }

    #[test]
    fn a_cancelled_pipeline_has_no_workflow_still_running() {
        let mut p = pipeline();
        p.status = "killed".into();
        p.workflows[1].state = "running".into();
        let link = parse_link("https://ci.example.com/repos/12/pipeline/42/1").unwrap();
        let run = to_run(&p, &link, "ci/woodpecker/manual");
        assert_eq!(run.state, State::Cancelled);
        assert_eq!(run.jobs[0].state, State::Success);
        assert_eq!(run.jobs[1].state, State::Cancelled);
    }

    #[test]
    fn states() {
        for (raw, want) in [
            ("killed", State::Cancelled),
            ("declined", State::Cancelled),
            ("error", State::Failure),
            ("created", State::Waiting),
            ("blocked", State::Blocked),
            ("skipped", State::Skipped),
            ("new-thing", State::Unknown),
        ] {
            assert_eq!(state(raw), want, "{raw}");
        }
    }

    #[test]
    fn logs_come_from_the_failed_steps_with_times() {
        let p = pipeline();
        let steps = log_steps(&p, 8);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "unit");
        assert_eq!(log_steps(&p, 7).len(), 1, "no failure: every step");
        assert!(log_steps(&p, 99).is_empty());
        let entries: Vec<LogEntry> = serde_json::from_value(serde_json::json!([
            { "time": 2, "data": "Z28gdGVzdAo=", "type": 0 },
            { "time": 3, "data": "", "type": 2 },
            { "time": 4, "data": "RkFJTA==", "type": 1 }
        ]))
        .unwrap();
        assert_eq!(
            log_text(steps[0], &entries),
            "--- step unit (failure) ---\n2023-11-14T22:13:53Z go test\n2023-11-14T22:13:55Z FAIL\n"
        );
    }

    #[test]
    fn agents_are_runners_and_tasks_wait_for_them() {
        let agents: Vec<Agent> = serde_json::from_value(serde_json::json!([
            { "id": 1, "name": "a", "platform": "linux/amd64", "backend": "docker",
              "last_contact": 1000, "org_id": -1, "token": "never read" },
            { "id": 2, "name": "b", "platform": "linux/arm64", "backend": "local",
              "last_contact": 1000, "org_id": 5, "custom_labels": { "gpu": "yes" } },
            { "id": 3, "name": "c", "platform": "linux/amd64", "backend": "docker",
              "last_contact": 100, "org_id": -1 }
        ]))
        .unwrap();
        let runners = to_runners(&agents, &[2], 1030);
        let states: Vec<_> = runners.iter().map(|r| (r.scope, r.state)).collect();
        assert_eq!(
            states,
            [
                (Scope::Instance, RunnerState::Idle),
                (Scope::Owner, RunnerState::Busy),
                (Scope::Instance, RunnerState::Offline)
            ]
        );
        assert!(runners[1].labels.contains(&"gpu=yes".to_owned()));
        let pending: Vec<Task> = serde_json::from_value(serde_json::json!([
            { "name": "test", "repo_id": 12,
              "labels": { "platform": "linux/arm64", "repo": "acme/widgets", "org-id": "5", "gpu": "*" } },
            { "name": "other", "repo_id": 13, "labels": {} }
        ]))
        .unwrap();
        let waiting = waiting_jobs(&pending, 12);
        assert_eq!(
            waiting,
            [("test".to_owned(), vec!["platform=linux/arm64".to_owned()])]
        );
        let running: Vec<Task> = serde_json::from_value(serde_json::json!([
            { "name": "build", "repo_id": 13, "agent_id": 2,
              "labels": { "platform": "linux/arm64", "repo": "acme/infra" } },
            { "name": "lint", "repo_id": 14, "agent_id": 2,
              "labels": { "platform": "linux/arm64", "repo": "acme/site" } }
        ]))
        .unwrap();
        let queue = QueueInfo {
            pending: Some(pending),
            running: Some(running),
        };
        let active = ActiveJobs::of(vec![ScopeJobs::listed(
            Scope::Instance,
            "/q".into(),
            active_jobs(&queue, &agents),
        )]);
        let report = runners::report(runners, Vec::new(), Some(waiting), active, |_| false);
        assert_eq!(report.waiting.as_ref().unwrap()[0].matching_busy, 1);
        let arm = report.pool(&["platform=linux/arm64".to_owned()]).unwrap();
        assert_eq!((arm.running, arm.waiting), (2, 1));
        // Agent b runs both, so it is taking several at once.
        assert!(arm.doubled_up);
        assert_eq!(report.pools[1].repos[0].repo, "repository #13");
    }
}
