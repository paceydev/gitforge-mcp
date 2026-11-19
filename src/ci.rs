//! CI runs and jobs in forge-neutral terms, and the rules for turning them
//! into a verdict an agent can act on.
//!
//! The rules encode mistakes that a status check makes easily and reports
//! confidently:
//!
//! - **The run decides whether a commit is finished, not its newest job.**
//!   Jobs finish out of order, and a job a runner hasn't taken yet may not
//!   be listed anywhere.
//! - **Skipped is not a result.** A commit whose jobs were all skipped ran
//!   nothing, which is not a pass.
//! - **A commit status's state is coarse.** A skipped job reports `success`
//!   and a cancelled one `failure`; the description says which.
//! - **A run's id is not its number.** Endpoints take the id; people and web
//!   pages use the number. Passing one for the other succeeds and returns an
//!   unrelated run.

use chrono::{DateTime, FixedOffset};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Queued, waiting for a runner.
    Waiting,
    /// Waiting on another job (`needs:`) or an approval.
    Blocked,
    Running,
    Success,
    Failure,
    Cancelled,
    Skipped,
    Unknown,
}

impl State {
    pub fn is_final(self) -> bool {
        matches!(
            self,
            State::Success | State::Failure | State::Cancelled | State::Skipped
        )
    }

    pub fn is_failed(self) -> bool {
        matches!(self, State::Failure | State::Cancelled)
    }

    /// Forgejo's own vocabulary, used by its runs, jobs and tasks.
    pub fn from_forgejo(raw: &str) -> Self {
        match raw {
            "success" => State::Success,
            "failure" => State::Failure,
            "cancelled" => State::Cancelled,
            "skipped" => State::Skipped,
            "waiting" => State::Waiting,
            "running" => State::Running,
            "blocked" => State::Blocked,
            _ => State::Unknown,
        }
    }

    /// Gitea's GitHub-style pair: a status (`queued`, `in_progress`,
    /// `completed`, ...) and, once completed, a conclusion.
    pub fn from_status_conclusion(status: &str, conclusion: Option<&str>) -> Self {
        match conclusion.filter(|c| !c.is_empty()) {
            Some("success" | "neutral") => return State::Success,
            Some("failure" | "timed_out" | "startup_failure" | "action_required") => {
                return State::Failure;
            }
            Some("cancelled") => return State::Cancelled,
            Some("skipped") => return State::Skipped,
            _ => {}
        }
        match status {
            "queued" | "pending" | "requested" => State::Waiting,
            "waiting" | "blocked" => State::Blocked,
            "in_progress" | "running" => State::Running,
            // Some versions report Forgejo-style words here too.
            other => State::from_forgejo(other),
        }
    }

    /// A commit status, read by its description first. The state alone
    /// can't tell skipped from success or cancelled from failure.
    pub fn from_commit_status(state: &str, description: &str) -> Self {
        if let Some(&(_, by_description)) = STATUS_DESCRIPTIONS
            .iter()
            .find(|(start, _)| description.starts_with(start))
        {
            return by_description;
        }
        match state {
            // Gitea's "warning": passed, with something to look at.
            "success" | "warning" => State::Success,
            "failure" | "error" => State::Failure,
            "pending" => State::Running,
            _ => State::Unknown,
        }
    }
}

/// How a commit status's description starts, for each state Actions
/// reports there.
const STATUS_DESCRIPTIONS: &[(&str, State)] = &[
    ("Waiting to run", State::Waiting),
    ("Blocked by required conditions", State::Blocked),
    ("Has started running", State::Running),
    ("Successful in", State::Success),
    ("Failing after", State::Failure),
    ("Has been cancelled", State::Cancelled),
    ("Has been skipped", State::Skipped),
];

/// Where a job's details came from, so a caller can judge how exact they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The forge's jobs endpoint.
    Jobs,
    /// Commit statuses (Forgejo before 16 has no jobs endpoint).
    Statuses,
    /// Commit statuses, corrected by the runner task that took the job.
    Task,
    /// Not listed anywhere yet; inferred from the run still going.
    Run,
    /// The API of the CI system outside the forge that ran it (Woodpecker).
    Api,
    /// GitHub check runs, posted by an app other than Actions.
    Checks,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub name: String,
    pub state: State,
    /// The id the job and log endpoints take, where the forge has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    /// The runner task that ran it, once a runner has taken it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
    pub source: Source,
    /// Where to see the job, for CI outside the forge: its page on that
    /// system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The job's steps, in order, while it runs or once it failed; empty
    /// for jobs that passed or were skipped, and on forges that don't
    /// report steps.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

/// One step of a job.
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
}

impl Job {
    /// Steps worth reporting: a passed or skipped job's steps add bulk and
    /// say nothing.
    pub fn keep_steps(state: State, steps: Vec<Step>) -> Vec<Step> {
        if matches!(state, State::Success | State::Skipped) {
            Vec::new()
        } else {
            steps
        }
    }
}

/// One workflow's runs for one event on one branch: the runs a run's
/// history is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunsOf<'a> {
    pub workflow: &'a str,
    pub event: &'a str,
    pub branch: &'a str,
}

impl<'a> RunsOf<'a> {
    pub fn run(run: &'a Run) -> Self {
        RunsOf {
            workflow: &run.workflow,
            event: &run.event,
            branch: &run.git_ref,
        }
    }

    /// Whether `run` is one of them.
    pub fn includes(&self, run: &Run) -> bool {
        run.workflow == self.workflow && run.event == self.event && run.git_ref == self.branch
    }
}

impl std::fmt::Display for RunsOf<'_> {
    /// "push runs of ci.yml on main".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} runs of {} on {}",
            self.event, self.workflow, self.branch
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    /// The internal id that run endpoints take.
    pub id: u64,
    /// The number people see in the web UI and in URLs. On GitHub that's
    /// the id: its `run_number` counts per workflow, so it repeats.
    pub number: u64,
    /// GitHub: which attempt this is, when the run has been rerun. The jobs
    /// are each job's latest attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    pub workflow: String,
    pub title: String,
    pub event: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
    pub jobs: Vec<Job>,
    /// The CI system that reported the run through commit statuses
    /// ("ci/woodpecker/push"), for CI outside the forge. Such a run has no
    /// id, logs or controls here unless gitforge-mcp reaches that system's
    /// own API (Woodpecker), which then fills in its jobs, steps and number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
}

impl Run {
    /// Whether anything in the run is still to finish, so cancelling means
    /// something.
    pub fn is_cancellable(&self) -> bool {
        !self.state.is_final() || self.jobs.iter().any(|j| !j.state.is_final())
    }

    /// A run still going whose listed jobs are all final has jobs nobody
    /// lists yet (no runner has taken them). Say so with a placeholder, so
    /// the commit can't look finished.
    pub fn add_unlisted_placeholder(&mut self) {
        if !self.state.is_final() && self.jobs.iter().all(|j| j.state.is_final()) {
            self.jobs.push(Job {
                name: "(jobs not yet listed)".into(),
                state: if self.state == State::Unknown {
                    State::Waiting
                } else {
                    self.state
                },
                job_id: None,
                task_id: None,
                started_at: None,
                finished_at: None,
                duration_seconds: None,
                source: Source::Run,
                url: None,
                steps: Vec::new(),
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every job finished, none failed, and at least one actually ran.
    Success,
    /// Every job finished and at least one failed or was cancelled.
    Failure,
    /// A job has already failed; others are still going.
    Failing,
    /// Not finished, nothing failed so far.
    Pending,
    /// Every job was skipped: nothing ran, which is not a pass.
    NothingRan,
    /// No CI run exists for this commit (yet).
    NoRuns,
}

pub fn verdict(runs: &[Run]) -> Verdict {
    if runs.is_empty() {
        return Verdict::NoRuns;
    }
    let jobs: Vec<&Job> = runs.iter().flat_map(|r| &r.jobs).collect();
    let unfinished =
        runs.iter().any(|r| !r.state.is_final()) || jobs.iter().any(|j| !j.state.is_final());
    let failed = jobs.iter().any(|j| j.state.is_failed())
        || runs
            .iter()
            .any(|r| r.jobs.is_empty() && r.state.is_failed());
    match (unfinished, failed) {
        (true, true) => Verdict::Failing,
        (true, false) => Verdict::Pending,
        (false, true) => Verdict::Failure,
        (false, false) if !jobs.is_empty() && jobs.iter().all(|j| j.state == State::Skipped) => {
            Verdict::NothingRan
        }
        (false, false) => Verdict::Success,
    }
}

/// "3 jobs: 2 success, 1 skipped" and similar.
pub fn summarize(runs: &[Run]) -> String {
    let jobs: Vec<&Job> = runs.iter().flat_map(|r| &r.jobs).collect();
    if runs.is_empty() {
        return "no CI runs for this commit".into();
    }
    let order = [
        State::Failure,
        State::Cancelled,
        State::Running,
        State::Waiting,
        State::Blocked,
        State::Unknown,
        State::Success,
        State::Skipped,
    ];
    let parts: Vec<String> = order
        .iter()
        .filter_map(|s| {
            let n = jobs.iter().filter(|j| j.state == *s).count();
            (n > 0).then(|| match s {
                // A skipped job looks like a pass in a count, and on some
                // branches a skip is a problem: say which ones they were.
                State::Skipped => format!(
                    "{n} skipped ({})",
                    named_or_grouped(jobs.iter().filter(|j| j.state == *s).map(|j| &j.name))
                ),
                _ => format!("{n} {}", state_word(*s)),
            })
        })
        .collect();
    let runs_part = if runs.len() == 1 {
        "1 run".to_owned()
    } else {
        format!("{} runs", runs.len())
    };
    format!("{runs_part}, {} jobs: {}", jobs.len(), parts.join(", "))
}

/// Removes the jobs `keep` rejects from `runs`, and says what was removed:
/// "26 success, 24 skipped", or empty if nothing was.
pub fn drop_jobs(runs: &mut [Run], keep: impl Fn(&Job) -> bool) -> String {
    let mut dropped: Vec<State> = Vec::new();
    for run in runs.iter_mut() {
        run.jobs.retain(|j| {
            let kept = keep(j);
            if !kept {
                dropped.push(j.state);
            }
            kept
        });
    }
    let order = [
        State::Failure,
        State::Cancelled,
        State::Running,
        State::Waiting,
        State::Blocked,
        State::Unknown,
        State::Success,
        State::Skipped,
    ];
    order
        .iter()
        .filter_map(|s| {
            let n = dropped.iter().filter(|d| *d == s).count();
            (n > 0).then(|| format!("{n} {}", state_word(*s)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where CI has got to, for someone waiting: how many jobs finished, which
/// are running and at which step, which failed and where, and which are
/// still to start. "2 of 5 jobs finished (2 success); running: test (step 3
/// of 7: cargo test); waiting: deploy, publish".
pub fn progress(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "no CI runs for this commit yet".into();
    }
    let jobs: Vec<&Job> = runs.iter().flat_map(|r| &r.jobs).collect();
    let finished: Vec<&&Job> = jobs.iter().filter(|j| j.state.is_final()).collect();
    let counts: Vec<String> = [
        State::Success,
        State::Failure,
        State::Cancelled,
        State::Skipped,
    ]
    .iter()
    .filter_map(|s| {
        let n = finished.iter().filter(|j| j.state == *s).count();
        (n > 0).then(|| format!("{n} {}", state_word(*s)))
    })
    .collect();
    let mut parts = vec![if counts.is_empty() {
        format!("0 of {} jobs finished", jobs.len())
    } else {
        format!(
            "{} of {} jobs finished ({})",
            finished.len(),
            jobs.len(),
            counts.join(", ")
        )
    }];
    let named = |label: &str, describe: &dyn Fn(&Job) -> String, pick: &dyn Fn(State) -> bool| {
        let picked: Vec<String> = jobs
            .iter()
            .filter(|j| pick(j.state))
            .map(|j| describe(j))
            .collect();
        (!picked.is_empty()).then(|| format!("{label}: {}", listed(&picked)))
    };
    parts.extend(named("failed", &failed_at, &State::is_failed));
    parts.extend(named("running", &running_at, &|s| s == State::Running));
    parts.extend(named("waiting", &|j: &Job| j.name.clone(), &|s| {
        matches!(s, State::Waiting | State::Blocked | State::Unknown)
    }));
    parts.join("; ")
}

/// A running job and the step it's on.
fn running_at(job: &Job) -> String {
    let current = job
        .steps
        .iter()
        .position(|s| s.state == State::Running)
        .or_else(|| job.steps.iter().position(|s| !s.state.is_final()));
    match current {
        Some(i) => format!(
            "{} (step {} of {}: {})",
            job.name,
            i + 1,
            job.steps.len(),
            job.steps[i].name
        ),
        None => job.name.clone(),
    }
}

/// A failed job and the step that failed.
fn failed_at(job: &Job) -> String {
    match job.steps.iter().position(|s| s.state.is_failed()) {
        Some(i) => format!("{} (at step {}: {})", job.name, i + 1, job.steps[i].name),
        None => job.name.clone(),
    }
}

/// Job names, all of them when there are few. When there are many, the
/// ones sharing a first word are grouped, biggest group first: "23 prod *,
/// 2 web *, docs". Big workflows name jobs by environment or area, so the
/// first word is what tells a skip by design from one that isn't.
fn named_or_grouped<'a>(names: impl Iterator<Item = &'a String>) -> String {
    const IN_FULL: usize = 10;
    let names: Vec<&String> = names.collect();
    if names.len() <= IN_FULL {
        return names
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
    }
    // Groups in order of first appearance, then stably by size.
    let mut groups: Vec<(&str, Vec<&String>)> = Vec::new();
    for name in names {
        let key = first_word(name);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(name),
            None => groups.push((key, vec![name])),
        }
    }
    groups.sort_by_key(|(_, members)| std::cmp::Reverse(members.len()));
    let shown: Vec<String> = groups
        .iter()
        .map(|(key, members)| match members.as_slice() {
            [one] => (*one).clone(),
            [first, ..] if members.iter().all(|m| m == first) => {
                format!("{} {first}", members.len())
            }
            _ => format!("{} {key} *", members.len()),
        })
        .collect();
    if shown.len() <= IN_FULL {
        shown.join(", ")
    } else {
        format!(
            "{} and {} more",
            shown[..IN_FULL].join(", "),
            shown.len() - IN_FULL
        )
    }
}

/// A job name's first word: up to a space, slash, colon or bracket.
fn first_word(name: &str) -> &str {
    let end = name
        .find(|c: char| c.is_whitespace() || matches!(c, '/' | ':' | '(' | '['))
        .unwrap_or(name.len());
    if end == 0 { name } else { &name[..end] }
}

/// Up to five names, then how many more.
fn listed(names: &[String]) -> String {
    const SHOWN: usize = 5;
    if names.len() <= SHOWN {
        names.join(", ")
    } else {
        format!(
            "{} and {} more",
            names[..SHOWN].join(", "),
            names.len() - SHOWN
        )
    }
}

fn state_word(s: State) -> &'static str {
    match s {
        State::Waiting => "waiting",
        State::Blocked => "blocked",
        State::Running => "running",
        State::Success => "success",
        State::Failure => "failure",
        State::Cancelled => "cancelled",
        State::Skipped => "skipped",
        State::Unknown => "unknown",
    }
}

/// Seconds between two RFC 3339 timestamps, when both parse.
pub fn seconds_between(start: Option<&str>, end: Option<&str>) -> Option<i64> {
    let start = parse_time(start?)?;
    let end = parse_time(end?)?;
    let secs = (end - start).num_seconds();
    (secs >= 0).then_some(secs)
}

pub fn parse_time(raw: &str) -> Option<DateTime<FixedOffset>> {
    // Forges report never-set times as the zero time; treat those as absent.
    if raw.is_empty() || raw.starts_with("0001-01-01") || raw.starts_with("1970-01-01") {
        return None;
    }
    DateTime::parse_from_rfc3339(raw).ok()
}

/// A run a dispatch started, as far as the forge says. Gitea before 1.26
/// says nothing, so every field can be missing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Dispatched {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A commit status as Gitea and Forgejo report it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CommitStatus {
    pub id: u64,
    #[serde(default, alias = "state")]
    pub status: String,
    #[serde(default)]
    pub target_url: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// The run number and job index an Actions commit status points at:
/// `.../actions/runs/<number>/jobs/<index>`.
pub fn status_target(url: &str) -> Option<(u64, u64)> {
    let tail = &url[url.rfind("/actions/runs/")? + "/actions/runs/".len()..];
    let (run, rest) = tail.split_once("/jobs/")?;
    let index = rest.split(['/', '?', '#']).next()?;
    Some((run.parse().ok()?, index.parse().ok()?))
}

/// The job name in an Actions status context, `<workflow> / <job> (<event>)`.
/// A matrix job's name has parentheses of its own, so the event is the last
/// parenthesised group.
pub fn status_job_name(context: &str) -> String {
    let without_event = match context.rsplit_once(" (") {
        Some((head, tail)) if tail.ends_with(')') && !tail.contains('(') => head,
        _ => context,
    };
    match without_event.split_once(" / ") {
        Some((_, job)) => job.to_owned(),
        None => without_event.to_owned(),
    }
}

/// One job per status target for run `number`, each in its newest state.
/// Statuses are append-only: every change adds one, several can share a
/// second, so the highest id per target is current.
pub fn jobs_from_statuses(statuses: &[CommitStatus], number: u64) -> Vec<Job> {
    let mut latest: std::collections::BTreeMap<u64, &CommitStatus> = Default::default();
    for s in statuses {
        let Some((run, index)) = status_target(&s.target_url) else {
            continue;
        };
        if run != number {
            continue;
        }
        match latest.get(&index) {
            Some(existing) if existing.id >= s.id => {}
            _ => {
                latest.insert(index, s);
            }
        }
    }
    latest
        .values()
        .map(|s| Job {
            name: status_job_name(&s.context),
            state: State::from_commit_status(&s.status, &s.description),
            job_id: None,
            task_id: None,
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            source: Source::Statuses,
            url: None,
            steps: Vec::new(),
        })
        .collect()
}

/// Runs for CI outside the forge (Woodpecker, Jenkins, Drone…), from a
/// commit's statuses: one run per CI system, one job per status context, in
/// its newest state. Statuses that Actions posts for its own jobs are left
/// out; the forge's runs cover those.
///
/// A context names its system and job, "ci/woodpecker/push/build": the job
/// is the last segment and the system the rest. A context with no `/` is
/// grouped by the host its link points to.
pub fn external_runs(statuses: &[CommitStatus], sha: &str) -> Vec<Run> {
    // Newest status per context, and when that context first appeared.
    let mut latest: std::collections::BTreeMap<&str, (&CommitStatus, Option<&str>)> =
        Default::default();
    for s in statuses
        .iter()
        .filter(|s| status_target(&s.target_url).is_none())
    {
        let entry = latest
            .entry(&s.context)
            .or_insert((s, s.created_at.as_deref()));
        if s.id > entry.0.id {
            entry.0 = s;
        }
        if let Some(created) = s.created_at.as_deref()
            && entry
                .1
                .is_none_or(|first| parse_time(created) < parse_time(first))
        {
            entry.1 = Some(created);
        }
    }
    let mut systems: std::collections::BTreeMap<String, Vec<Job>> = Default::default();
    let mut links: std::collections::BTreeMap<String, String> = Default::default();
    for (context, (status, first)) in latest {
        let (system, name) = match context.rsplit_once('/') {
            Some((system, name)) if !system.is_empty() && !name.is_empty() => {
                (system.to_owned(), name.to_owned())
            }
            _ => (link_host(&status.target_url), context.to_owned()),
        };
        let state = State::from_commit_status(&status.status, &status.description);
        let started_at = first.map(str::to_owned);
        let finished_at = state
            .is_final()
            .then(|| {
                status
                    .updated_at
                    .clone()
                    .or_else(|| status.created_at.clone())
            })
            .flatten();
        let url = (!status.target_url.is_empty()).then(|| status.target_url.clone());
        links
            .entry(system.clone())
            .or_insert_with(|| status.target_url.clone());
        systems.entry(system).or_default().push(Job {
            name,
            state,
            job_id: None,
            task_id: None,
            duration_seconds: seconds_between(started_at.as_deref(), finished_at.as_deref()),
            started_at,
            finished_at,
            source: Source::Statuses,
            url,
            steps: Vec::new(),
        });
    }
    systems
        .into_iter()
        .map(|(system, jobs)| {
            let url = links.get(&system).filter(|u| !u.is_empty()).cloned();
            external_run(system, jobs, url, sha)
        })
        .collect()
}

/// One run of CI outside the forge, from its jobs.
pub fn external_run(system: String, jobs: Vec<Job>, url: Option<String>, sha: &str) -> Run {
    let mut run = Run {
        id: 0,
        number: 0,
        attempt: None,
        workflow: system.clone(),
        title: String::new(),
        event: String::new(),
        git_ref: String::new(),
        sha: sha.to_owned(),
        state: State::Unknown,
        url,
        duration_seconds: None,
        started_at: None,
        finished_at: None,
        jobs,
        external: Some(system),
    };
    settle(&mut run);
    run
}

/// Sets a run outside the forge's state and times from its jobs: unfinished
/// while any job is (running if one runs, else waiting), else failed if one failed,
/// skipped if all were, else passed.
pub fn settle(run: &mut Run) {
    let jobs = &run.jobs;
    run.state = if jobs.iter().any(|j| !j.state.is_final()) {
        if jobs.iter().any(|j| j.state == State::Running) {
            State::Running
        } else {
            State::Waiting
        }
    } else if jobs.iter().any(|j| j.state.is_failed()) {
        State::Failure
    } else if jobs.iter().all(|j| j.state == State::Skipped) {
        State::Skipped
    } else {
        State::Success
    };
    run.started_at = jobs
        .iter()
        .filter_map(|j| j.started_at.clone())
        .min_by_key(|t| parse_time(t));
    run.finished_at = run
        .state
        .is_final()
        .then(|| {
            jobs.iter()
                .filter_map(|j| j.finished_at.clone())
                .max_by_key(|t| parse_time(t))
        })
        .flatten();
    run.duration_seconds = seconds_between(run.started_at.as_deref(), run.finished_at.as_deref());
}

/// The jobs of `run` that failed or were cancelled, for rerunning them, or
/// why there are none.
pub fn failed_jobs(run: &Run) -> Result<Vec<&Job>, String> {
    let failed: Vec<&Job> = run.jobs.iter().filter(|j| j.state.is_failed()).collect();
    if failed.is_empty() {
        Err(format!(
            "run {} has no failed or cancelled jobs to rerun",
            run.number
        ))
    } else {
        Ok(failed)
    }
}

/// The host a status links to, to name a CI system whose contexts don't.
fn link_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "external CI".to_owned())
}

/// A runner task: a job a runner has taken.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Task {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub run_number: u64,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub status: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Corrects jobs read from statuses with the task that ran each one: its
/// id, its exact state, and approximate times (taken → last update). A
/// task for the run that no status mentions (statuses can lag, or be
/// missing) adds its job.
pub fn overlay_tasks(jobs: &mut Vec<Job>, tasks: &[Task], number: u64) {
    for job in jobs.iter_mut() {
        let task = tasks
            .iter()
            .filter(|t| t.run_number == number && t.name == job.name)
            .max_by_key(|t| t.id);
        if let Some(task) = task {
            apply_task(job, task);
            if job.source == Source::Statuses {
                job.source = Source::Task;
            }
        }
    }
    let mut unlisted: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.run_number == number && !jobs.iter().any(|j| j.name == t.name))
        .collect();
    // The newest attempt of each job.
    unlisted.sort_by_key(|t| std::cmp::Reverse(t.id));
    unlisted.dedup_by(|a, b| a.name == b.name);
    for task in unlisted.into_iter().rev() {
        let mut job = Job {
            name: task.name.clone(),
            state: State::Unknown,
            job_id: None,
            task_id: None,
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            source: Source::Task,
            url: None,
            steps: Vec::new(),
        };
        apply_task(&mut job, task);
        jobs.push(job);
    }
}

/// Fills in what a task knows about a job that lacks it.
pub fn apply_task(job: &mut Job, task: &Task) {
    job.task_id = Some(task.id);
    let state = State::from_forgejo(&task.status);
    if state != State::Unknown {
        job.state = state;
    }
    if job.started_at.is_none() {
        job.started_at = task.created_at.clone();
    }
    if job.state.is_final() && job.finished_at.is_none() {
        job.finished_at = task.updated_at.clone();
    }
    if job.duration_seconds.is_none() && job.state.is_final() {
        job.duration_seconds =
            seconds_between(job.started_at.as_deref(), job.finished_at.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, state: State) -> Job {
        Job {
            name: name.into(),
            state,
            job_id: None,
            task_id: None,
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            source: Source::Jobs,
            url: None,
            steps: Vec::new(),
        }
    }

    fn run(state: State, jobs: Vec<Job>) -> Run {
        Run {
            attempt: None,
            id: 10,
            number: 3,
            workflow: "ci.yml".into(),
            title: "t".into(),
            event: "push".into(),
            git_ref: "main".into(),
            sha: "a".repeat(40),
            state,
            url: None,
            started_at: None,
            finished_at: None,
            duration_seconds: None,
            jobs,
            external: None,
        }
    }

    #[test]
    fn verdicts() {
        use State::*;
        assert_eq!(verdict(&[]), Verdict::NoRuns);
        assert_eq!(
            verdict(&[run(Success, vec![job("a", Success), job("b", Skipped)])]),
            Verdict::Success
        );
        assert_eq!(
            verdict(&[run(Skipped, vec![job("a", Skipped), job("b", Skipped)])]),
            Verdict::NothingRan
        );
        assert_eq!(
            verdict(&[run(Failure, vec![job("a", Success), job("b", Cancelled)])]),
            Verdict::Failure
        );
        assert_eq!(
            verdict(&[run(Running, vec![job("a", Failure), job("b", Running)])]),
            Verdict::Failing
        );
        assert_eq!(
            verdict(&[run(Running, vec![job("a", Success), job("b", Waiting)])]),
            Verdict::Pending
        );
    }

    #[test]
    fn a_running_run_with_only_finished_jobs_is_not_finished() {
        // The 2026-10-02 false pass: one job green, the other not listed.
        let mut r = run(State::Waiting, vec![job("frontend", State::Success)]);
        r.add_unlisted_placeholder();
        assert_eq!(r.jobs.len(), 2);
        assert_eq!(r.jobs[1].state, State::Waiting);
        assert_eq!(verdict(&[r]), Verdict::Pending);
    }

    #[test]
    fn a_second_run_still_going_keeps_the_commit_pending() {
        let a = run(State::Success, vec![job("a", State::Success)]);
        let b = run(State::Running, vec![job("a", State::Running)]);
        assert_eq!(verdict(&[a, b]), Verdict::Pending);
    }

    #[test]
    fn status_descriptions_beat_status_states() {
        assert_eq!(
            State::from_commit_status("success", "Has been skipped"),
            State::Skipped
        );
        assert_eq!(
            State::from_commit_status("failure", "Has been cancelled"),
            State::Cancelled
        );
        assert_eq!(
            State::from_commit_status("pending", "Blocked by required conditions"),
            State::Blocked
        );
        assert_eq!(
            State::from_commit_status("success", "custom"),
            State::Success
        );
    }

    #[test]
    fn gitea_status_and_conclusion() {
        assert_eq!(
            State::from_status_conclusion("completed", Some("skipped")),
            State::Skipped
        );
        assert_eq!(
            State::from_status_conclusion("queued", None),
            State::Waiting
        );
        assert_eq!(
            State::from_status_conclusion("in_progress", Some("")),
            State::Running
        );
        assert_eq!(
            State::from_status_conclusion("running", None),
            State::Running
        );
    }

    #[test]
    fn status_targets_and_names() {
        assert_eq!(
            status_target("/acme/widgets/actions/runs/2/jobs/1"),
            Some((2, 1))
        );
        assert_eq!(
            status_target("https://git.example.com/acme/widgets/actions/runs/12/jobs/0"),
            Some((12, 0))
        );
        assert_eq!(status_target("https://ci.example.com/build/5"), None);
        assert_eq!(status_job_name("ci / contract (push)"), "contract");
        assert_eq!(
            status_job_name("ci / test (ubuntu, 1.89) (pull_request)"),
            "test (ubuntu, 1.89)"
        );
        assert_eq!(status_job_name("lint"), "lint");
    }

    fn status(id: u64, url: &str, desc: &str, ctx: &str) -> CommitStatus {
        CommitStatus {
            id,
            status: "pending".into(),
            target_url: url.into(),
            description: desc.into(),
            context: ctx.into(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn newest_status_per_job_wins_by_id() {
        let statuses = vec![
            status(
                7,
                "/a/w/actions/runs/2/jobs/1",
                "Successful in 18m9s",
                "ci / contract (push)",
            ),
            status(
                6,
                "/a/w/actions/runs/2/jobs/1",
                "Has started running",
                "ci / contract (push)",
            ),
            status(
                4,
                "/a/w/actions/runs/2/jobs/0",
                "Successful in 5m9s",
                "ci / check (push)",
            ),
            status(
                1,
                "/a/w/actions/runs/2/jobs/0",
                "Waiting to run",
                "ci / check (push)",
            ),
            status(
                9,
                "/a/w/actions/runs/3/jobs/0",
                "Waiting to run",
                "ci / check (push)",
            ),
        ];
        let jobs = jobs_from_statuses(&statuses, 2);
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            (jobs[0].name.as_str(), jobs[0].state),
            ("check", State::Success)
        );
        assert_eq!(
            (jobs[1].name.as_str(), jobs[1].state),
            ("contract", State::Success)
        );
    }

    #[test]
    fn tasks_correct_status_jobs() {
        let mut jobs = vec![Job {
            source: Source::Statuses,
            url: None,
            steps: Vec::new(),
            ..job("macos", State::Running)
        }];
        let tasks = vec![Task {
            id: 975,
            name: "macos".into(),
            run_number: 3,
            head_sha: String::new(),
            status: "failure".into(),
            created_at: Some("2026-10-02T13:00:00+10:00".into()),
            updated_at: Some("2026-10-02T13:07:30+10:00".into()),
        }];
        overlay_tasks(&mut jobs, &tasks, 3);
        assert_eq!(jobs[0].task_id, Some(975));
        assert_eq!(jobs[0].state, State::Failure);
        assert_eq!(jobs[0].duration_seconds, Some(450));
        assert_eq!(jobs[0].source, Source::Task);
    }

    #[test]
    fn a_task_no_status_mentions_adds_its_job() {
        let mut jobs = Vec::new();
        let task = |id, status: &str| Task {
            id,
            name: "frontend".into(),
            run_number: 20,
            head_sha: String::new(),
            status: status.into(),
            created_at: None,
            updated_at: None,
        };
        overlay_tasks(&mut jobs, &[task(970, "failure"), task(974, "success")], 20);
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (jobs[0].state, jobs[0].task_id),
            (State::Success, Some(974))
        );
    }

    #[test]
    fn zero_times_are_absent() {
        assert!(parse_time("0001-01-01T00:00:00Z").is_none());
        assert_eq!(
            seconds_between(
                Some("2026-11-03T20:20:24+11:00"),
                Some("2026-11-03T20:44:11+11:00")
            ),
            Some(1427)
        );
    }

    #[test]
    fn summaries() {
        let r = run(
            State::Failure,
            vec![
                job("a", State::Success),
                job("b", State::Failure),
                job("c", State::Skipped),
            ],
        );
        assert_eq!(
            summarize(&[r]),
            "1 run, 3 jobs: 1 failure, 1 success, 1 skipped (c)"
        );
    }

    #[test]
    fn many_skipped_jobs_are_grouped_by_first_word() {
        let mut jobs: Vec<Job> = (0..23)
            .map(|i| job(&format!("prod deploy {i}"), State::Skipped))
            .collect();
        jobs.push(job("web lint", State::Skipped));
        jobs.push(job("web: test", State::Skipped));
        jobs.push(job("docs", State::Skipped));
        jobs.push(job("api test", State::Success));
        assert_eq!(
            summarize(&[run(State::Success, jobs)]),
            "1 run, 27 jobs: 1 success, 26 skipped (23 prod *, 2 web *, docs)"
        );
    }

    #[test]
    fn a_few_skipped_jobs_are_named() {
        let jobs = vec![
            job("prod migrations", State::Skipped),
            job("stg migrations", State::Skipped),
            job("test", State::Success),
        ];
        assert_eq!(
            summarize(&[run(State::Success, jobs)]),
            "1 run, 3 jobs: 1 success, 2 skipped (prod migrations, stg migrations)"
        );
    }

    #[test]
    fn skipped_groups_past_ten_are_counted() {
        let mut jobs: Vec<Job> = (0..12)
            .map(|i| job(&format!("area{i} check"), State::Skipped))
            .collect();
        jobs.push(job("area0 build", State::Skipped));
        assert_eq!(
            summarize(&[run(State::Success, jobs)]),
            "1 run, 13 jobs: 13 skipped (2 area0 *, area1 check, area2 check, area3 check, \
             area4 check, area5 check, area6 check, area7 check, area8 check, area9 check \
             and 2 more)"
        );
    }

    #[test]
    fn same_named_skipped_jobs_are_counted_by_name() {
        let jobs: Vec<Job> = (0..11).map(|_| job("build", State::Skipped)).collect();
        assert_eq!(
            summarize(&[run(State::Skipped, jobs)]),
            "1 run, 11 jobs: 11 skipped (11 build)"
        );
    }

    fn with_steps(mut job: Job, steps: &[(&str, State)]) -> Job {
        job.steps = steps
            .iter()
            .map(|(name, state)| Step {
                name: (*name).into(),
                state: *state,
                duration_seconds: None,
            })
            .collect();
        job
    }

    #[test]
    fn progress_names_the_step_each_running_job_is_on() {
        use State::*;
        let r = run(
            Running,
            vec![
                job("lint", Success),
                job("docs", Skipped),
                with_steps(
                    job("test", Running),
                    &[
                        ("Set up job", Success),
                        ("Build", Success),
                        ("cargo test", Running),
                        ("Upload", Waiting),
                    ],
                ),
                job("deploy", Blocked),
            ],
        );
        assert_eq!(
            progress(&[r]),
            "2 of 4 jobs finished (1 success, 1 skipped); running: test (step 3 of 4: cargo test); waiting: deploy"
        );
    }

    #[test]
    fn progress_says_where_a_job_failed() {
        use State::*;
        let r = run(
            Failure,
            vec![
                with_steps(
                    job("build", Failure),
                    &[
                        ("Set up job", Success),
                        ("Compile", Failure),
                        ("Upload", Skipped),
                    ],
                ),
                job("notify", Failure),
            ],
        );
        assert_eq!(
            progress(&[r]),
            "2 of 2 jobs finished (2 failure); failed: build (at step 2: Compile), notify"
        );
    }

    #[test]
    fn progress_lists_at_most_five_names() {
        let jobs = (1..=7)
            .map(|i| job(&format!("j{i}"), State::Waiting))
            .collect();
        assert_eq!(
            progress(&[run(State::Waiting, jobs)]),
            "0 of 7 jobs finished; waiting: j1, j2, j3, j4, j5 and 2 more"
        );
        assert_eq!(progress(&[]), "no CI runs for this commit yet");
    }

    #[test]
    fn passed_and_skipped_jobs_drop_their_steps() {
        let steps = || {
            vec![Step {
                name: "s".into(),
                state: State::Success,
                duration_seconds: None,
            }]
        };
        assert!(Job::keep_steps(State::Success, steps()).is_empty());
        assert!(Job::keep_steps(State::Skipped, steps()).is_empty());
        assert_eq!(Job::keep_steps(State::Running, steps()).len(), 1);
        assert_eq!(Job::keep_steps(State::Failure, steps()).len(), 1);
    }

    fn external(id: u64, state: &str, ctx: &str, url: &str, at: &str) -> CommitStatus {
        CommitStatus {
            id,
            status: state.into(),
            target_url: url.into(),
            description: String::new(),
            context: ctx.into(),
            created_at: Some(at.into()),
            updated_at: Some(at.into()),
        }
    }

    #[test]
    fn external_ci_becomes_one_run_per_system() {
        let wp = "https://ci.example.com/repos/1/pipeline/9";
        let statuses = vec![
            // Woodpecker: pending, then the outcome, per step.
            external(
                1,
                "pending",
                "ci/woodpecker/push/build",
                wp,
                "2026-11-09T10:00:00Z",
            ),
            external(
                2,
                "pending",
                "ci/woodpecker/push/lint",
                wp,
                "2026-11-09T10:00:00Z",
            ),
            external(
                5,
                "success",
                "ci/woodpecker/push/build",
                wp,
                "2026-11-09T10:04:00Z",
            ),
            external(
                6,
                "failure",
                "ci/woodpecker/push/lint",
                wp,
                "2026-11-09T10:02:00Z",
            ),
            // Jenkins, still going.
            external(
                7,
                "pending",
                "continuous-integration/jenkins/branch",
                "https://jenkins.example.com/job/widgets/job/main/3/",
                "2026-11-09T10:01:00Z",
            ),
            // A context with no system in it: grouped by its link's host.
            external(
                8,
                "warning",
                "audit",
                "https://audit.example.com/r/1",
                "2026-11-09T10:03:00Z",
            ),
            // Actions' own status for its job: covered by the forge's runs.
            external(
                9,
                "success",
                "ci / test (push)",
                "https://git.example.com/acme/widgets/actions/runs/4/jobs/0",
                "2026-11-09T10:00:00Z",
            ),
        ];
        let runs = external_runs(&statuses, "abc");
        let by: std::collections::BTreeMap<&str, &Run> = runs
            .iter()
            .map(|r| (r.external.as_deref().unwrap(), r))
            .collect();
        assert_eq!(by.len(), 3, "{runs:?}");

        let wp_run = by["ci/woodpecker/push"];
        assert_eq!(wp_run.state, State::Failure);
        assert_eq!(wp_run.url.as_deref(), Some(wp));
        let jobs: Vec<(&str, State, Option<i64>)> = wp_run
            .jobs
            .iter()
            .map(|j| (j.name.as_str(), j.state, j.duration_seconds))
            .collect();
        assert_eq!(
            jobs,
            [
                ("build", State::Success, Some(240)),
                ("lint", State::Failure, Some(120))
            ]
        );
        assert_eq!(wp_run.duration_seconds, Some(240));

        assert_eq!(by["continuous-integration/jenkins"].state, State::Running);
        let audit = by["audit.example.com"];
        assert_eq!(audit.state, State::Success, "warning passes");
        assert_eq!(audit.jobs[0].name, "audit");

        // All three together: Jenkins is still going, Woodpecker failed.
        assert_eq!(verdict(&runs), Verdict::Failing);
    }
}
