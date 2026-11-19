//! What each tool does, given a live session. `server.rs` routes and gates;
//! this module does the work and shapes the results.

// qual:allow(coupling, max_instability=0.9) reason: "the top layer: it uses every model module and nothing but server.rs uses it"

use regex::RegexBuilder;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::capability::Feature;
use crate::ci::{self, Run, Verdict};
use crate::context::Session;
use crate::estimate::{self, Eta};
use crate::external::{self, Provider};
use crate::forge::{ApiError, Forge};
use crate::logs::{self, Cut};
use crate::pulls::{
    self, MergeStyle, NewPullRequest, PrState, PullRequest, StateFilter, StatusChecks,
};

/// A failure the agent should read: returned as a tool error, not a
/// protocol error, so the message reaches it.
#[derive(Debug)]
pub struct ToolError(pub String);

impl From<ApiError> for ToolError {
    fn from(e: ApiError) -> Self {
        ToolError(e.to_string())
    }
}

type ToolResult = Result<Value, ToolError>;

fn to_value<T: Serialize>(v: &T) -> ToolResult {
    serde_json::to_value(v).map_err(|e| ToolError(e.to_string()))
}

// ---------------------------------------------------------------- selection

/// Which commit or run a CI tool looks at. At most one may be given; with
/// none, the checkout's HEAD.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct Target {
    /// A commit sha, full or abbreviated. Defaults to the checkout's HEAD.
    pub sha: Option<String>,
    /// A branch on the forge: its latest commit is used.
    pub branch: Option<String>,
    /// A run number, as in the forge's run URLs (on GitHub, the run id).
    /// Where the forge runs no CI of its own, the pipeline number on the CI
    /// system outside it (Woodpecker).
    pub run: Option<u64>,
}

struct Selected {
    /// The commit, when the selection was by commit or branch.
    sha: Option<String>,
    /// How the selection was made, for the reply.
    label: String,
    runs: Vec<Run>,
    notes: Vec<String>,
}

async fn select(session: &Session, target: &Target) -> Result<Selected, ToolError> {
    let given = [
        target.sha.is_some(),
        target.branch.is_some(),
        target.run.is_some(),
    ]
    .iter()
    .filter(|g| **g)
    .count();
    if given > 1 {
        return Err(ToolError("give at most one of sha, branch and run".into()));
    }
    let mut notes = Vec::new();
    if let Some(number) = target.run {
        let run = run_by_number(session, number).await?.ok_or_else(|| {
            ToolError(format!(
                "no run number {number} in {}",
                session.repo.full_name()
            ))
        })?;
        return Ok(Selected {
            sha: Some(run.sha.clone()),
            label: format!("run {number}"),
            runs: vec![run],
            notes,
        });
    }
    // The checkout's branch, when HEAD is the target and on one.
    let mut local_branch = None;
    let (sha, label) = if let Some(branch) = &target.branch {
        let sha = session
            .forge
            .branch_head(&session.repo, branch)
            .await?
            .ok_or_else(|| ToolError(format!("no branch {branch} on the forge")))?;
        (sha, format!("branch {branch}"))
    } else if let Some(sha) = &target.sha {
        (
            full_sha(session, sha).await?,
            format!("commit {}", short(sha)),
        )
    } else {
        let sha = local_git(session, &["rev-parse", "HEAD"])
            .await
            .ok_or_else(|| ToolError("the checkout has no commits".into()))?;
        let branch = local_git(session, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_default();
        let label = if branch.is_empty() || branch == "HEAD" {
            "HEAD".to_owned()
        } else {
            local_branch = Some(branch.clone());
            format!("HEAD of {branch}")
        };
        (sha, label)
    };
    let runs = runs_for(session, &sha).await?;
    if target.branch.is_none() && target.sha.is_none() {
        // No runs on HEAD usually means the checkout and the forge disagree
        // about the branch; say how, which beats a bare "no runs".
        let hint = match &local_branch {
            Some(branch) if runs.is_empty() => behind_or_ahead(session, &sha, branch).await,
            _ => None,
        };
        match hint {
            Some(hint) => notes.push(hint),
            None => {
                let pushed = local_git(session, &["branch", "-r", "--contains", "HEAD"])
                    .await
                    .is_some_and(|out| !out.trim().is_empty());
                if !pushed {
                    notes.push(
                        "HEAD is not on any remote-tracking branch: it may not have been pushed (or `git fetch` hasn't run since)"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(Selected {
        sha: Some(sha),
        label,
        runs,
        notes,
    })
}

/// Why HEAD of `branch` has no runs, when the reason is that the forge's
/// copy of the branch is somewhere else: not pushed, behind, ahead or
/// diverged. `None` when they agree or the forge can't say.
async fn behind_or_ahead(session: &Session, head: &str, branch: &str) -> Option<String> {
    // The branch on the forge is the upstream's, which may be named
    // differently from the local one.
    let forge_branch = local_git(session, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .await
        .and_then(|up| up.split_once('/').map(|(_, name)| name.to_owned()))
        .unwrap_or_else(|| branch.to_owned());
    let forge_head = match session
        .forge
        .branch_head(&session.repo, &forge_branch)
        .await
    {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return Some(format!(
                "there is no branch {forge_branch} on the forge: CI runs once it's pushed"
            ));
        }
        Err(_) => return None,
    };
    if forge_head.eq_ignore_ascii_case(head) {
        return None;
    }
    let at = format!("{forge_branch} on the forge ({})", short(&forge_head));
    let fetched = local_git(
        session,
        &["cat-file", "-e", &format!("{forge_head}^{{commit}}")],
    )
    .await
    .is_some();
    let relation = if fetched {
        let ahead = commits_in(session, &format!("{forge_head}..{head}")).await;
        let behind = commits_in(session, &format!("{head}..{forge_head}")).await;
        match (ahead, behind) {
            (0, behind) => format!("HEAD is {} behind {at}", commits(behind)),
            (ahead, 0) => {
                return Some(format!(
                    "HEAD is {} ahead of {at}: CI runs once they're pushed",
                    commits(ahead)
                ));
            }
            (ahead, behind) => format!(
                "HEAD and {at} have diverged: {} here aren't pushed, and {} there aren't here",
                commits(ahead),
                commits(behind)
            ),
        }
    } else {
        format!("{at} has commits this checkout hasn't fetched, so HEAD is behind it")
    };
    let ci = match runs_for(session, &forge_head).await {
        Ok(runs) if runs.is_empty() => {
            return Some(format!(
                "{relation}. It has no runs either: no workflow may run on {forge_branch}"
            ));
        }
        Ok(runs) => format!(
            "CI on {forge_branch} there is {} ({})",
            verdict_name(ci::verdict(&runs)),
            ci::summarize(&runs)
        ),
        Err(_) => format!("CI on {forge_branch} there couldn't be read"),
    };
    Some(format!(
        "{relation}. {ci}: pass branch=\"{forge_branch}\" to look at it"
    ))
}

async fn commits_in(session: &Session, range: &str) -> u64 {
    local_git(session, &["rev-list", "--count", range])
        .await
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn commits(n: u64) -> String {
    if n == 1 {
        "1 commit".to_owned()
    } else {
        format!("{n} commits")
    }
}

fn verdict_name(verdict: Verdict) -> String {
    serde_json::to_value(verdict)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether this repository's CI can be read at all: the forge's own runs,
/// or CI outside it through commit statuses.
fn reads_ci(session: &Session) -> bool {
    session.capabilities.is_available(Feature::CiRuns)
        || session.capabilities.is_available(Feature::CiStatuses)
}

/// Every run for `sha`: the forge's own (Actions) runs, then CI outside the
/// forge (Woodpecker, Jenkins…) as reported through commit statuses.
async fn runs_for(session: &Session, sha: &str) -> Result<Vec<Run>, ApiError> {
    let mut runs = if session.capabilities.is_available(Feature::CiRuns) {
        session.forge.runs_for_commit(&session.repo, sha).await?
    } else {
        Vec::new()
    };
    if session.capabilities.is_available(Feature::CiStatuses) {
        let mut external = session.forge.external_runs(&session.repo, sha).await?;
        external::refresh_all(&session.providers, &mut external).await;
        runs.extend(external);
    }
    Ok(runs)
}

/// Run `number`: the forge's own, or, where the forge runs no CI of its
/// own, the pipeline of that number on a CI system outside it.
async fn run_by_number(session: &Session, number: u64) -> Result<Option<Run>, ApiError> {
    if session.capabilities.available_on(Feature::CiRuns, None) || session.providers.is_empty() {
        return session.forge.run_by_number(&session.repo, number).await;
    }
    for provider in &session.providers {
        if let Some(run) = provider.run_by_number(number).await? {
            return Ok(Some(run));
        }
    }
    Ok(None)
}

/// The CI system outside the forge that `run` ran on and its number there,
/// when that system gives `feature`.
fn provider_with<'a>(
    session: &'a Session,
    run: &Run,
    feature: Feature,
) -> Option<(&'a Provider, u64)> {
    external::provider_for(&session.providers, run)
        .filter(|(p, _)| session.capabilities.available_on(feature, Some(p.name())))
}

/// Whether `feature` is available for `run`: from the forge for its own
/// runs, from the CI system outside it for the rest.
fn available_for(session: &Session, run: &Run, feature: Feature) -> bool {
    match &run.external {
        None => session.capabilities.available_on(feature, None),
        Some(_) => provider_with(session, run, feature).is_some(),
    }
}

/// A full SHA-1 in hex.
const FULL_SHA_LEN: usize = 40;
/// The shortest SHA prefix trusted to name a commit: git's default
/// abbreviation.
const MIN_SHA_PREFIX: usize = 7;
/// Pull requests listed when not told how many.
const DEFAULT_PR_LIMIT: u32 = 20;
/// How often, and how far apart, a merge is retried while the forge is
/// still working out whether the pull request can merge.
const MERGE_RECHECKS: usize = 4;
const MERGE_RECHECK_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// The run filters on Gitea and Forgejo match only a full sha; an
/// abbreviated one silently matches nothing. Expand it locally.
async fn full_sha(session: &Session, sha: &str) -> Result<String, ToolError> {
    let sha = sha.trim();
    if sha.len() == FULL_SHA_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(sha.to_ascii_lowercase());
    }
    local_git(
        session,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{sha}^{{commit}}"),
        ],
    )
    .await
    .ok_or_else(|| {
        ToolError(format!(
            "{sha} isn't a commit in the local checkout; give the full 40-character sha"
        ))
    })
}

async fn local_git(session: &Session, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&session.dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

// ---------------------------------------------------------------- estimates

/// Each workflow's stored history, read once per call; keyed by workflow,
/// event and branch.
type Histories = std::collections::HashMap<(String, String, String), estimate::Series>;

/// Forecasts for the runs that haven't finished, with what their waiting
/// jobs wait for where no runner is free, and what else is on the runners
/// of a run that has gone on longer than usual.
async fn estimates(session: &Session, runs: &[Run], histories: &mut Histories) -> Vec<Eta> {
    let now = chrono::Utc::now();
    let mut unfinished = Vec::new();
    // CI outside the forge has no run history to learn from here.
    for run in runs
        .iter()
        .filter(|r| r.external.is_none())
        .filter(|r| r.is_cancellable())
    {
        let of = ci::RunsOf::run(run);
        let key = (run.workflow.clone(), run.event.clone(), run.git_ref.clone());
        if !histories.contains_key(&key) {
            let series = estimate::series(session, of).await;
            histories.insert(key.clone(), series);
        }
        let known = estimate::KnownJobs::of(run);
        let history = histories
            .get(&key)
            .and_then(|series| estimate::History::of(series, Some(&known), of));
        let overdue = estimate::overdue(run, history.as_ref(), now);
        unfinished.push((run, history, overdue));
    }
    let waiting = unfinished
        .iter()
        .flat_map(|(r, ..)| &r.jobs)
        .any(|j| j.state == ci::State::Waiting && j.source != ci::Source::Run);
    let overdue = unfinished.iter().any(|(.., o)| *o);
    let report = queue_report(session, waiting || overdue).await;
    let repo = session.repo.full_name();
    let mut etas = Vec::new();
    for (run, history, overdue) in unfinished {
        let queue = report
            .as_ref()
            .map(|r| estimate::queue_waits(run, r, &repo))
            .unwrap_or_default();
        let contention = report
            .as_ref()
            .filter(|_| overdue)
            .and_then(|r| estimate::contention(run, r, &repo));
        etas.extend(Eta::of(run, history.as_ref(), queue, contention, now));
    }
    etas
}

/// The runner report, when it's `wanted` (a job waits for a runner, or a
/// run has gone on longer than usual) and the credential may list runners.
async fn queue_report(session: &Session, wanted: bool) -> Option<crate::runners::RunnerReport> {
    let allowed = [Feature::CiQueue, Feature::CiRunners]
        .into_iter()
        .all(|f| session.capabilities.available_on(f, None));
    if !(wanted && allowed) {
        return None;
    }
    estimate::runner_report(session).await
}

/// "a; b" for one summary, "ci.yml: a; lint.yml: b" for several.
fn per_workflow<'a>(each: impl ExactSizeIterator<Item = (&'a str, &'a str)>) -> String {
    let single = each.len() == 1;
    each.map(|(workflow, summary)| {
        if single {
            summary.to_owned()
        } else {
            format!("{workflow}: {summary}")
        }
    })
    .collect::<Vec<_>>()
    .join("; ")
}

/// The progress line with each forecast after it.
fn with_etas(progress: String, etas: &[Eta]) -> String {
    if etas.is_empty() {
        return progress;
    }
    let each = etas
        .iter()
        .map(|e| (e.workflow.as_str(), e.summary.as_str()));
    format!("{progress}; {}", per_workflow(each))
}

/// The workflows that ran on `branch`'s head on the forge.
async fn head_workflows(session: &Session, branch: &str) -> Result<Vec<String>, ToolError> {
    let Some(head) = session.forge.branch_head(&session.repo, branch).await? else {
        return Ok(Vec::new());
    };
    let mut workflows: Vec<String> = Vec::new();
    for run in session.forge.runs_for_commit(&session.repo, &head).await? {
        if run.external.is_none() && !workflows.contains(&run.workflow) {
            workflows.push(run.workflow);
        }
    }
    Ok(workflows)
}

/// How well estimates would have done on a workflow's history: each
/// passing run predicted from the passing runs before it, first from the
/// whole workflow and then from the runs with the same job set, and each
/// failed run's time to its first failure from the failed runs before it.
/// For the `backtest` command.
pub async fn backtest(
    session: &Session,
    workflow: Option<&str>,
    event: &str,
    branch: Option<&str>,
    pages: u32,
) -> ToolResult {
    let branch = branch.unwrap_or(&session.info.default_branch).to_owned();
    let workflow = match workflow {
        Some(w) => w.to_owned(),
        None => head_workflows(session, &branch)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ToolError(format!("no runs on {branch}'s head: pass --workflow")))?,
    };
    let of = ci::RunsOf {
        workflow: &workflow,
        event,
        branch: &branch,
    };
    let runs = session
        .forge
        .recent_runs(&session.repo, of, pages, None)
        .await?;
    // Every run read, its jobs too: the stored series keeps fewer.
    let mut series = estimate::Series::default();
    series.merge_all(&runs);
    let ids: Vec<u64> = series.runs.iter().map(|r| r.id).collect();
    estimate::read_jobs(session, &mut series, &ids, of).await;
    let mut result = estimate::backtest_series(&series);
    result["workflow"] = workflow.into();
    result["event"] = event.into();
    result["branch"] = branch.into();
    result["runs_read"] = runs.len().into();
    Ok(result)
}

// ---------------------------------------------------------------- ci_estimate

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CiEstimateParams {
    /// The branch to estimate for. Defaults to the checkout's branch; one
    /// with too little history of its own uses the default branch's.
    pub branch: Option<String>,
    /// The event: `push` (default), `pull_request`, and so on.
    pub event: Option<String>,
    /// One workflow file (`ci.yml`). Defaults to every workflow that ran
    /// on the branch's head, or the default branch's.
    pub workflow: Option<String>,
}

pub async fn ci_estimate(session: &Session, params: &CiEstimateParams) -> ToolResult {
    let default_branch = session.info.default_branch.as_str();
    let branch = match &params.branch {
        Some(b) => b.clone(),
        None => local_git(session, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .filter(|b| b != "HEAD")
            .unwrap_or_else(|| default_branch.to_owned()),
    };
    let event = params.event.as_deref().unwrap_or("push");
    let workflows = match &params.workflow {
        Some(w) => vec![w.clone()],
        None => estimate_workflows(session, &branch, default_branch).await?,
    };
    let mut outlooks = Vec::new();
    let mut notes = Vec::new();
    for workflow in &workflows {
        match outlook(session, workflow, event, [&branch, default_branch]).await {
            Some(o) => outlooks.push(o),
            None => notes.push(format!(
                "{workflow}: too few finished {event} runs on {branch} or {default_branch} to estimate"
            )),
        }
    }
    if workflows.is_empty() {
        notes.push(format!(
            "no runs on the heads of {branch} or {default_branch} to find workflows from: pass workflow"
        ));
    }
    let each = outlooks
        .iter()
        .map(|o| (o.workflow.as_str(), o.summary.as_str()));
    let summary = if outlooks.is_empty() {
        "no estimate: too little history".to_owned()
    } else {
        per_workflow(each)
    };
    Ok(json!({
        "summary": summary,
        "branch": branch,
        "event": event,
        "estimates": outlooks,
        "notes": notes,
    }))
}

/// The workflows that ran on the heads of `branch` and the default branch.
async fn estimate_workflows(
    session: &Session,
    branch: &str,
    default_branch: &str,
) -> Result<Vec<String>, ToolError> {
    let mut found = head_workflows(session, branch).await?;
    if branch != default_branch {
        for w in head_workflows(session, default_branch).await? {
            if !found.contains(&w) {
                found.push(w);
            }
        }
    }
    Ok(found)
}

/// The outlook for `workflow` from the first of `branches` with enough
/// history.
async fn outlook(
    session: &Session,
    workflow: &str,
    event: &str,
    branches: [&str; 2],
) -> Option<estimate::Outlook> {
    let mut tried = Vec::new();
    for branch in branches {
        if tried.contains(&branch) {
            continue;
        }
        tried.push(branch);
        let of = ci::RunsOf {
            workflow,
            event,
            branch,
        };
        let series = estimate::series(session, of).await;
        if let Some(outlook) = estimate::Outlook::of(&series, of) {
            return Some(outlook);
        }
    }
    None
}

// ---------------------------------------------------------------- ci_status

/// Which jobs a CI result lists. A big workflow has dozens of jobs that
/// passed or were skipped, which cost tokens and say nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobsShown {
    /// Jobs that failed, were cancelled, or haven't finished; the rest are
    /// counted in `jobs_omitted`.
    #[default]
    Attention,
    /// Every job.
    All,
    /// None: the verdict, summary and progress line only.
    None,
}

impl JobsShown {
    /// Trims `runs` to the jobs to show; what was left out, if anything,
    /// for the reply.
    fn apply(self, runs: &mut [Run]) -> Option<String> {
        let omitted = match self {
            JobsShown::All => return None,
            JobsShown::None => ci::drop_jobs(runs, |_| false),
            JobsShown::Attention => ci::drop_jobs(runs, |j| {
                !matches!(j.state, ci::State::Success | ci::State::Skipped)
            }),
        };
        (!omitted.is_empty()).then(|| format!("{omitted}: pass jobs=\"all\" to list them"))
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CiStatusParams {
    #[serde(flatten)]
    pub target: Target,
    /// Which jobs to list: `attention` (default: failed, cancelled and
    /// unfinished ones), `all`, or `none`.
    #[serde(default)]
    pub jobs: JobsShown,
}

pub async fn ci_status(session: &Session, params: &CiStatusParams) -> ToolResult {
    let selected = select(session, &params.target).await?;
    let verdict = ci::verdict(&selected.runs);
    let mut notes = selected.notes;
    if verdict == Verdict::NothingRan {
        notes.push("every job was skipped: nothing ran, so this is not a pass".into());
    }
    if selected
        .runs
        .iter()
        .filter(|r| r.external.is_none())
        .flat_map(|r| &r.jobs)
        .any(|j| matches!(j.source, ci::Source::Statuses | ci::Source::Task))
    {
        notes.push(
            "this forge version has no jobs endpoint: jobs come from commit statuses, corrected by runner tasks where a runner has taken them"
                .into(),
        );
    }
    let etas = estimates(session, &selected.runs, &mut Histories::new()).await;
    let (summary, progress) = (
        ci::summarize(&selected.runs),
        with_etas(ci::progress(&selected.runs), &etas),
    );
    let mut runs = selected.runs;
    let omitted = params.jobs.apply(&mut runs);
    Ok(json!({
        "verdict": verdict,
        "summary": summary,
        "progress": progress,
        "estimates": etas,
        "target": selected.label,
        "commit": selected.sha,
        "notes": notes,
        "jobs_omitted": omitted,
        "runs": runs,
    }))
}

// ---------------------------------------------------------------- ci_logs

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CiLogsParams {
    #[serde(flatten)]
    pub target: Target,
    /// Only jobs whose name contains this text (case-insensitive).
    pub job: Option<String>,
    /// Include jobs that didn't fail. By default only failed and cancelled
    /// jobs are read.
    #[serde(default)]
    pub all: bool,
    /// Lines from the end of each log. Default 80; 0 reads the whole log
    /// (still capped at about 60,000 characters, keeping the end).
    pub tail: Option<usize>,
    /// A regular expression: keep only matching lines, before `tail`
    /// applies. Case-insensitive.
    pub grep: Option<String>,
    /// Keep the runner's timestamp on each line.
    #[serde(default)]
    pub timestamps: bool,
}

const DEFAULT_TAIL: usize = 80;
const MAX_JOBS: usize = 6;

/// A log read for a tail is fetched from its end: about this many bytes a
/// line, and never less than `MIN_TAIL_BYTES`.
const TAIL_BYTES_PER_LINE: u64 = 400;
const MIN_TAIL_BYTES: u64 = 32 * 1024;

pub async fn ci_logs(session: &Session, params: &CiLogsParams) -> ToolResult {
    let how = Cut {
        tail: match params.tail {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(DEFAULT_TAIL),
        },
        grep: grep_pattern(params.grep.as_deref())?,
        timestamps: params.timestamps,
    };
    let selected = select(session, &params.target).await?;
    let needle = params.job.as_deref().map(str::to_lowercase);
    let readable = |run: &Run| available_for(session, run, Feature::CiLogs);
    let Picked {
        mut chosen,
        elsewhere,
    } = pick_jobs(&selected.runs, needle.as_deref(), params.all, &readable);
    let mut notes = selected.notes;
    if !elsewhere.is_empty() {
        notes.push(format!(
            "logs for CI outside the forge are on that system, not here: {}",
            elsewhere.join("; ")
        ));
    }
    if chosen.is_empty() {
        notes.push(nothing_to_read(&selected.runs, needle.is_some()));
        return Ok(json!({
            "target": selected.label,
            "commit": selected.sha,
            "jobs": [],
            "notes": notes,
        }));
    }
    if chosen.len() > MAX_JOBS {
        notes.push(format!(
            "{} jobs matched; showing the first {MAX_JOBS}. Narrow with job=<name>",
            chosen.len()
        ));
        chosen.truncate(MAX_JOBS);
    }
    let mut jobs = Vec::new();
    for (run, job) in chosen {
        jobs.push(read_log(session, run, job, &how).await?);
    }
    Ok(json!({
        "target": selected.label,
        "commit": selected.sha,
        "jobs": jobs,
        "notes": notes,
    }))
}

fn grep_pattern(grep: Option<&str>) -> Result<Option<regex::Regex>, ToolError> {
    grep.map(|g| {
        RegexBuilder::new(g)
            .case_insensitive(true)
            .build()
            .map_err(|e| ToolError(format!("grep is not a valid regular expression: {e}")))
    })
    .transpose()
}

/// The jobs whose logs ci_logs reads, and a link for each wanted job whose
/// log is on a system outside the forge.
struct Picked<'a> {
    chosen: Vec<(&'a Run, &'a ci::Job)>,
    elsewhere: Vec<String>,
}

/// Jobs named like `needle`; without one, the failed jobs, or every job
/// when `all`. A job only inferred from its run has no log to read, and a
/// run whose logs can't be read here (`readable`) gets a link instead.
fn pick_jobs<'a>(
    runs: &'a [Run],
    needle: Option<&str>,
    all: bool,
    readable: &dyn Fn(&Run) -> bool,
) -> Picked<'a> {
    let mut picked = Picked {
        chosen: Vec::new(),
        elsewhere: Vec::new(),
    };
    for run in runs {
        let here = run.external.is_none() || readable(run);
        for job in run
            .jobs
            .iter()
            .filter(|j| j.source != ci::Source::Run && wanted(j, needle, all))
        {
            match &run.external {
                Some(system) if !here => {
                    picked.elsewhere.push(format!(
                        "{system} {}: {}",
                        job.name,
                        job.url.as_deref().unwrap_or("no link")
                    ));
                }
                _ => picked.chosen.push((run, job)),
            }
        }
    }
    picked
}

fn wanted(job: &ci::Job, needle: Option<&str>, all: bool) -> bool {
    match needle {
        Some(n) => job.name.to_lowercase().contains(n),
        None => all || job.state.is_failed(),
    }
}

/// Why ci_logs has no log to show.
fn nothing_to_read(runs: &[Run], by_name: bool) -> String {
    if runs.is_empty() {
        "there are no CI runs for this commit".to_owned()
    } else if by_name {
        "no job's name matches `job`".to_owned()
    } else {
        format!(
            "no job failed ({}); pass all=true or job=<name> to read others",
            ci::summarize(runs)
        )
    }
}

/// One job's log, cut as asked; a log the forge won't give becomes an
/// error in its place rather than failing the whole call.
async fn read_log(session: &Session, run: &Run, job: &ci::Job, how: &Cut) -> ToolResult {
    // A tail needs only the end of the log; a grep needs all of it.
    let tail_bytes = match (&how.grep, how.tail) {
        (None, Some(n)) => Some((n as u64 * TAIL_BYTES_PER_LINE).max(MIN_TAIL_BYTES)),
        _ => None,
    };
    let fetched = match provider_with(session, run, Feature::CiLogs) {
        Some((provider, number)) => provider.job_log(number, job, tail_bytes).await,
        None => session.forge.job_log(&session.repo, job, tail_bytes).await,
    };
    let log = match fetched {
        Ok((text, partial)) => to_value(&logs::cut(&text, how, partial))?,
        Err(e) => json!({ "error": e.to_string() }),
    };
    Ok(json!({
        "run": run.number,
        "name": job.name,
        "state": job.state,
        "job_id": job.job_id,
        "log": log,
    }))
}

// ---------------------------------------------------------------- ci_runners

/// The forge's runners, where it lists them, and the agents of each CI
/// system outside it that lists them, under `ci_systems`.
pub async fn ci_runners(session: &Session) -> ToolResult {
    let mut reply = if session.capabilities.available_on(Feature::CiRunners, None) {
        let report = session
            .forge
            .runners(&session.repo, session.user.as_ref())
            .await?;
        to_value(&report)?
    } else {
        json!({})
    };
    let mut systems = Vec::new();
    let mut summaries = Vec::new();
    for provider in session.providers.iter().filter(|p| {
        session
            .capabilities
            .available_on(Feature::CiRunners, Some(p.name()))
    }) {
        let mut report = match provider.runners().await {
            Ok(report) => {
                summaries.push(format!("{}: {}", provider.name(), report.summary));
                to_value(&report)?
            }
            Err(e) => json!({ "error": e.to_string() }),
        };
        report["system"] = provider.name().into();
        systems.push(report);
    }
    if !systems.is_empty() {
        if reply.get("summary").is_none() {
            reply["summary"] = summaries.join("; ").into();
        }
        reply["ci_systems"] = systems.into();
    }
    Ok(reply)
}

// ---------------------------------------------------------------- pull_request

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrAction {
    /// List pull requests, most recently updated first.
    #[default]
    List,
    /// One pull request in full, with its head commit's CI verdict.
    Get,
    /// Open a pull request from `head` (default: the checkout's current
    /// branch) into `base` (default: the repository's default branch).
    Create,
    /// Merge one, after checking it hasn't moved, merges cleanly and (unless
    /// `require_ci` is false) its CI passed.
    Merge,
    /// Merge one once its CI passes: at once if it already has, otherwise
    /// the forge merges it when the checks the base branch requires pass.
    MergeWhenReady,
    /// Cancel a merge scheduled with `merge_when_ready`.
    CancelAutoMerge,
    /// Its reviews and their comments on lines of the diff; by default only
    /// what is still open.
    Reviews,
}

impl PrAction {
    pub fn name(self) -> &'static str {
        match self {
            PrAction::List => "list",
            PrAction::Get => "get",
            PrAction::Create => "create",
            PrAction::Merge => "merge",
            PrAction::MergeWhenReady => "merge_when_ready",
            PrAction::CancelAutoMerge => "cancel_auto_merge",
            PrAction::Reviews => "reviews",
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PullRequestParams {
    /// `list` (default), `get`, `reviews`, `create`, `merge`,
    /// `merge_when_ready` or `cancel_auto_merge`. Only the actions this
    /// forge and your access allow are listed.
    #[serde(default)]
    pub action: PrAction,
    /// For every action but `list` and `create`: the pull request's number.
    pub number: Option<u64>,
    /// For `list`: `open` (default), `closed` or `all`.
    #[serde(default)]
    pub state: StateFilter,
    /// For `list`: how many to return (default 20, at most 50).
    pub limit: Option<u32>,
    /// For `create`: the branch with the changes, already pushed. Default:
    /// the checkout's current branch.
    pub head: Option<String>,
    /// For `create`: the branch to merge into. Default: the repository's
    /// default branch.
    pub base: Option<String>,
    /// For `create`: the title.
    pub title: Option<String>,
    /// For `create`: the description.
    pub body: Option<String>,
    /// For `merge` and `merge_when_ready`: `merge` (default), `rebase`,
    /// `rebase-merge`, `squash` or `fast-forward-only`. The repository's
    /// settings decide which are allowed.
    pub style: Option<MergeStyle>,
    /// For `merge` and `merge_when_ready`: the head commit you reviewed,
    /// from `get`. Refused if the branch has moved since. Strongly
    /// recommended.
    pub expected_head_sha: Option<String>,
    /// For `merge` and `merge_when_ready`: delete the head branch
    /// afterwards. Default false.
    #[serde(default)]
    pub delete_branch: bool,
    /// For `merge`: refuse unless CI on the head commit passed. Default
    /// true; set false only when the user has said to merge anyway.
    pub require_ci: Option<bool>,
    /// For `reviews`: also list dismissed reviews, resolved comments, and
    /// reviews with nothing to read. Default false: only what is open.
    #[serde(default)]
    pub include_resolved: bool,
}

pub async fn pull_request(session: &Session, params: &PullRequestParams) -> ToolResult {
    match params.action {
        PrAction::List => {
            let prs = session
                .forge
                .pull_requests(
                    &session.repo,
                    params.state,
                    params.limit.unwrap_or(DEFAULT_PR_LIMIT),
                )
                .await?;
            Ok(json!({ "count": prs.len(), "pull_requests": prs }))
        }
        PrAction::Get => {
            let number = params
                .number
                .ok_or_else(|| ToolError("`get` needs `number`".into()))?;
            let pr = get_pr(session, number).await?;
            let mut value = to_value(&pr)?;
            value["ci"] = ci_summary(session, &pr.head_sha).await;
            value["reviews"] = review_summary(session, number).await;
            Ok(value)
        }
        PrAction::Reviews => {
            let number = params
                .number
                .ok_or_else(|| ToolError("`reviews` needs `number`".into()))?;
            let reviews = fetch_reviews(session, number).await?;
            let mut value = to_value(&pulls::review_report(reviews, params.include_resolved))?;
            value["number"] = json!(number);
            Ok(value)
        }
        PrAction::Create => create_pr(session, params).await,
        PrAction::Merge => merge_pr(session, params).await,
        PrAction::MergeWhenReady => merge_when_ready(session, params).await,
        PrAction::CancelAutoMerge => cancel_auto_merge(session, params).await,
    }
}

async fn get_pr(session: &Session, number: u64) -> Result<PullRequest, ToolError> {
    session
        .forge
        .pull_request(&session.repo, number)
        .await?
        .ok_or_else(|| ToolError(format!("no pull request #{number}")))
}

async fn fetch_reviews(session: &Session, number: u64) -> Result<Vec<pulls::Review>, ToolError> {
    match session
        .forge
        .pull_request_reviews(&session.repo, number)
        .await
    {
        Err(ApiError::NotFound { .. }) => Err(ToolError(format!("no pull request #{number}"))),
        other => Ok(other?),
    }
}

/// Where the reviews stand, without the reviews: who approved, who asked
/// for changes, and how many line comments are open.
async fn review_summary(session: &Session, number: u64) -> Value {
    match fetch_reviews(session, number).await {
        Ok(reviews) => {
            let report = pulls::review_report(reviews, false);
            json!({
                "approved_by": report.approved_by,
                "changes_requested_by": report.changes_requested_by,
                "review_requested": report.review_requested,
                "open_comments": report.open_comments,
            })
        }
        Err(e) => json!({ "error": e.0 }),
    }
}

/// The CI verdict for a commit, or null where the forge can't say.
async fn ci_summary(session: &Session, sha: &str) -> Value {
    if !reads_ci(session) || sha.is_empty() {
        return Value::Null;
    }
    match runs_for(session, sha).await {
        Ok(runs) => json!({
            "verdict": ci::verdict(&runs),
            "summary": ci::summarize(&runs),
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn create_pr(session: &Session, params: &PullRequestParams) -> ToolResult {
    let title = params
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| ToolError("`create` needs a `title`".into()))?;
    let head = match &params.head {
        Some(h) => h.clone(),
        None => local_git(session, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .filter(|b| b != "HEAD")
            .ok_or_else(|| {
                ToolError(
                    "the checkout isn't on a branch; give `head`, the branch to propose".into(),
                )
            })?,
    };
    let base = params
        .base
        .clone()
        .unwrap_or_else(|| session.info.default_branch.clone());
    if head == base {
        return Err(ToolError(format!(
            "head and base are both {head}: push your changes to their own branch first"
        )));
    }
    let new = NewPullRequest {
        head,
        base,
        title: title.trim().to_owned(),
        body: params.body.clone().unwrap_or_default(),
    };
    let pr = session
        .forge
        .create_pull_request(&session.repo, &new)
        .await?;
    Ok(json!({ "created": true, "pull_request": pr }))
}

/// Why a merge must not go ahead, if it mustn't.
pub fn merge_refusal(
    pr: &PullRequest,
    expected_head: Option<&str>,
    ci: Option<Verdict>,
    require_ci: bool,
) -> Option<String> {
    if pr.state != PrState::Open {
        return Some(format!("#{} is {:?}, not open", pr.number, pr.state).to_lowercase());
    }
    if let Some(expected) = expected_head.map(str::trim).filter(|e| !e.is_empty())
        && !(expected.len() >= MIN_SHA_PREFIX && pr.head_sha.starts_with(expected))
    {
        return Some(format!(
            "the head of #{} has moved: you reviewed {expected}, it is now {}. Look at the new commits before merging",
            pr.number, pr.head_sha
        ));
    }
    if pr.mergeable == Some(false) {
        return Some(format!(
            "#{} conflicts with {}: resolve the conflicts first",
            pr.number, pr.base
        ));
    }
    if require_ci {
        let why = match ci {
            Some(Verdict::Success) => return None,
            Some(Verdict::NoRuns) => "no CI run exists for its head commit (yet)".to_owned(),
            Some(Verdict::NothingRan) => "every CI job on its head commit was skipped".to_owned(),
            Some(v) => format!(
                "CI on its head commit is {}",
                serde_json::to_value(v)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            None => "this forge can't report CI for it".to_owned(),
        };
        return Some(format!(
            "#{} isn't ready: {why}. Pass require_ci=false only if the user has said to merge anyway",
            pr.number
        ));
    }
    None
}

/// Whether the repository runs CI at all: Actions are on and it has had a
/// run. When the forge can't say, assume it does, which keeps the CI check.
async fn has_ci(session: &Session, base: &str) -> bool {
    if session.capabilities.is_available(Feature::CiRuns)
        && session.forge.has_runs(&session.repo).await.unwrap_or(true)
    {
        return true;
    }
    if !session.capabilities.is_available(Feature::CiStatuses) {
        return false;
    }
    // CI outside the forge leaves its statuses on the base branch.
    match session.forge.branch_head(&session.repo, base).await {
        Ok(Some(head)) => session
            .forge
            .external_runs(&session.repo, &head)
            .await
            .map_or(true, |runs| !runs.is_empty()),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Pull request `number`, asked for again while the forge hasn't settled
/// whether it can merge: Gitea and Forgejo work it out in the background
/// and report `false` until they have, so a pull request opened a moment
/// ago looks conflicted, and GitHub reports `null` until it has.
async fn settled_pr(session: &Session, number: u64) -> Result<PullRequest, ToolError> {
    let mut pr = get_pr(session, number).await?;
    for _ in 0..MERGE_RECHECKS {
        if pr.state != PrState::Open || pr.mergeable == Some(true) {
            break;
        }
        tokio::time::sleep(MERGE_RECHECK_PAUSE).await;
        pr = get_pr(session, number).await?;
    }
    Ok(pr)
}

/// The CI verdict on a pull request's head, if the forge can say, and
/// whether the repository has no CI at all.
async fn head_verdict(
    session: &Session,
    pr: &PullRequest,
) -> Result<(Option<Verdict>, bool), ToolError> {
    let verdict = if reads_ci(session) {
        Some(ci::verdict(&runs_for(session, &pr.head_sha).await?))
    } else {
        None
    };
    let no_ci = verdict == Some(Verdict::NoRuns) && !has_ci(session, &pr.base).await;
    Ok((verdict, no_ci))
}

async fn merge_pr(session: &Session, params: &PullRequestParams) -> ToolResult {
    let number = params
        .number
        .ok_or_else(|| ToolError("`merge` needs `number`".into()))?;
    let pr = settled_pr(session, number).await?;
    let (verdict, no_ci) = head_verdict(session, &pr).await?;
    // A repository with no CI at all can't pass CI; requiring it would
    // only teach callers to pass require_ci=false out of habit. One that
    // has run CI before, but not yet on this head, still waits.
    let mut notes = Vec::new();
    let require_ci = params.require_ci.unwrap_or(true) && !no_ci;
    if no_ci && params.require_ci != Some(false) {
        notes.push("this repository has never run CI, so require_ci didn't apply".to_owned());
    }
    if let Some(why) = merge_refusal(
        &pr,
        params.expected_head_sha.as_deref(),
        verdict,
        require_ci,
    ) {
        return Err(ToolError(format!("not merged: {why}")));
    }
    merge_now(session, params, &pr, notes).await
}

/// Sends a merge, retrying only the answer that says nothing was merged.
async fn send_merge(
    session: &Session,
    params: &PullRequestParams,
    pr: &PullRequest,
    schedule: bool,
) -> Result<(), ApiError> {
    let style = params.style.unwrap_or_default();
    // Gitea answers 405 "Please try again later" while its mergeability
    // check is still running, before it merges or schedules anything, so
    // asking again is safe.
    let mut attempt = 0;
    loop {
        let outcome = if schedule {
            session
                .forge
                .schedule_merge(
                    &session.repo,
                    pr.number,
                    style,
                    &pr.head_sha,
                    params.delete_branch,
                )
                .await
        } else {
            session
                .forge
                .merge_pull_request(
                    &session.repo,
                    pr.number,
                    style,
                    &pr.head_sha,
                    params.delete_branch,
                )
                .await
        };
        match outcome {
            Err(ApiError::Rejected {
                status: 405,
                ref message,
                ..
            }) if attempt < MERGE_RECHECKS && message.to_lowercase().contains("try again") => {
                attempt += 1;
                tokio::time::sleep(MERGE_RECHECK_PAUSE).await;
            }
            other => return other,
        }
    }
}

/// Merges `pr`, already checked, and reports the outcome.
async fn merge_now(
    session: &Session,
    params: &PullRequestParams,
    pr: &PullRequest,
    mut notes: Vec<String>,
) -> ToolResult {
    send_merge(session, params, pr, false)
        .await
        .map_err(|e| ToolError(format!("not merged: {e}")))?;
    let after = get_pr(session, pr.number).await?;
    merged_reply(session, params, pr, after, &mut notes).await
}

/// The reply for a pull request that has merged, or was meant to: the
/// merge commit, and whether the branch went.
async fn merged_reply(
    session: &Session,
    params: &PullRequestParams,
    pr: &PullRequest,
    after: PullRequest,
    notes: &mut Vec<String>,
) -> ToolResult {
    // The forge deletes the branch as part of the merge, and says nothing
    // about it: look.
    let branch_deleted = if params.delete_branch && after.state == PrState::Merged {
        match session.forge.branch_head(&session.repo, &after.head).await {
            Ok(None) => true,
            Ok(Some(_)) => {
                notes.push(format!(
                    "{} wasn't deleted: the forge may not allow deleting it (a protected branch, or one in a fork)",
                    after.head
                ));
                false
            }
            Err(_) => false,
        }
    } else {
        false
    };
    if let Some(sha) = &after.merge_commit_sha {
        notes.push(format!(
            "to follow CI on {} after the merge, wait on sha={sha} rather than the branch, which may move on",
            after.base
        ));
    }
    Ok(json!({
        "merged": after.state == PrState::Merged,
        "style": params.style.unwrap_or_default(),
        "head_sha": pr.head_sha,
        "merge_commit_sha": after.merge_commit_sha,
        "branch_deleted": branch_deleted,
        "notes": notes,
        "pull_request": after,
    }))
}

/// Why a merge can't be left to wait for checks, if it can't: the forge
/// waits only for the checks the base branch's protection requires, so
/// without any it would merge at once, whatever CI says.
pub fn schedule_refusal(pr: &PullRequest, checks: Option<&StatusChecks>) -> Option<String> {
    match checks {
        None => Some(format!("its base branch {} doesn't exist", pr.base)),
        Some(c) if !c.required => Some(format!(
            "{base} doesn't require status checks, so the forge would merge #{n} at once without waiting \
for CI. Wait for CI (ci_wait sha={sha}) and then use merge, or ask a repository admin to turn on status checks \
in {base}'s branch protection",
            base = pr.base,
            n = pr.number,
            sha = pr.head_sha,
        )),
        Some(_) => None,
    }
}

async fn merge_when_ready(session: &Session, params: &PullRequestParams) -> ToolResult {
    let number = params
        .number
        .ok_or_else(|| ToolError("`merge_when_ready` needs `number`".into()))?;
    let pr = settled_pr(session, number).await?;
    // The same checks as merge, less CI: waiting for CI is the point.
    if let Some(why) = merge_refusal(&pr, params.expected_head_sha.as_deref(), None, false) {
        return Err(ToolError(format!("not scheduled: {why}")));
    }
    let (verdict, no_ci) = head_verdict(session, &pr).await?;
    // Nothing to wait for: merge now. Forgejo would otherwise leave the
    // merge waiting for a check result that has already come.
    if verdict == Some(Verdict::Success) || no_ci {
        let notes = vec![if no_ci {
            "this repository has never run CI, so it merged at once".to_owned()
        } else {
            "CI on its head had already passed, so it merged at once".to_owned()
        }];
        let mut reply = merge_now(session, params, &pr, notes).await?;
        reply["scheduled"] = json!(false);
        return Ok(reply);
    }
    let checks = session.forge.status_checks(&session.repo, &pr.base).await?;
    if let Some(why) = schedule_refusal(&pr, checks.as_ref()) {
        return Err(ToolError(format!("not scheduled: {why}")));
    }
    send_merge(session, params, &pr, true)
        .await
        .map_err(|e| ToolError(format!("not scheduled: {e}")))?;
    let after = get_pr(session, number).await?;
    if after.state == PrState::Merged {
        // Gitea checks at once, and the required checks had passed.
        let mut notes =
            vec!["the required checks had already passed, so it merged at once".to_owned()];
        let mut reply = merged_reply(session, params, &pr, after, &mut notes).await?;
        reply["scheduled"] = json!(false);
        return Ok(reply);
    }
    let mut notes = vec![
        format!(
            "the forge merges #{number} once the checks {base} requires pass. If more commits are pushed first, it \
merges those once their checks pass: cancel_auto_merge stops it",
            base = pr.base
        ),
        format!(
            "ci_wait sha={} follows the checks; get number={number} shows whether it has merged",
            pr.head_sha
        ),
    ];
    if matches!(verdict, Some(Verdict::Failure | Verdict::Failing)) {
        notes.push(
            "CI on the head is failing: it merges only once a rerun or a new commit passes"
                .to_owned(),
        );
    }
    Ok(json!({
        "merged": false,
        "scheduled": true,
        "style": params.style.unwrap_or_default(),
        "head_sha": pr.head_sha,
        "ci": verdict,
        "required_checks": checks.map(|c| c.contexts).unwrap_or_default(),
        "notes": notes,
        "pull_request": after,
    }))
}

async fn cancel_auto_merge(session: &Session, params: &PullRequestParams) -> ToolResult {
    let number = params
        .number
        .ok_or_else(|| ToolError("`cancel_auto_merge` needs `number`".into()))?;
    let pr = get_pr(session, number).await?;
    if pr.state == PrState::Merged {
        return Err(ToolError(format!(
            "#{number} has already merged{}: there is nothing to cancel",
            pr.merge_commit_sha
                .as_deref()
                .map(|sha| format!(" (merge commit {sha})"))
                .unwrap_or_default()
        )));
    }
    let cancelled = session
        .forge
        .cancel_scheduled_merge(&session.repo, number)
        .await?;
    let mut notes = Vec::new();
    if !cancelled {
        notes.push(format!("#{number} had no merge scheduled"));
    }
    Ok(json!({ "cancelled": cancelled, "number": number, "notes": notes }))
}

// ---------------------------------------------------------------- ci_control

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Stop a run that hasn't finished.
    Cancel,
    /// Run every job of a finished run again.
    Rerun,
    /// Run only a finished run's failed and cancelled jobs again.
    RerunFailed,
    /// Start a workflow that has a `workflow_dispatch` trigger.
    Dispatch,
}

impl ControlAction {
    pub fn name(self) -> &'static str {
        match self {
            ControlAction::Cancel => "cancel",
            ControlAction::Rerun => "rerun",
            ControlAction::RerunFailed => "rerun_failed",
            ControlAction::Dispatch => "dispatch",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CiControlParams {
    /// `cancel`, `rerun`, `rerun_failed` or `dispatch`. Only the actions this
    /// forge version and your access allow are listed.
    pub action: ControlAction,
    /// For `cancel` and the reruns, which run: a run number, or a commit or
    /// branch with exactly one run (default: the checkout's HEAD).
    #[serde(flatten)]
    pub target: Target,
    /// For `dispatch`: the workflow's file name, e.g. `deploy.yml`. On
    /// Jenkins, the job, as folder/job (default: the one this repository's
    /// statuses link to).
    pub workflow: Option<String>,
    /// For `dispatch`: the branch or tag to run it on. Default: the
    /// repository's default branch.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// For `dispatch`: the workflow's inputs. On Woodpecker, variables for
    /// the manual pipeline; on Jenkins, the job's build parameters.
    pub inputs: Option<serde_json::Map<String, Value>>,
    /// For `dispatch` where CI runs both on the forge and outside it: `forge`
    /// for the forge's own Actions, or the system outside it (`woodpecker`,
    /// `jenkins`). On Woodpecker, `workflow` is optional and runs only that
    /// workflow.
    pub ci: Option<String>,
}

pub async fn ci_control(session: &Session, params: &CiControlParams) -> ToolResult {
    let failed_only = match params.action {
        ControlAction::Dispatch => return dispatch(session, params).await,
        ControlAction::Cancel => return cancel(session, params).await,
        ControlAction::Rerun => false,
        ControlAction::RerunFailed => true,
    };
    rerun(session, params, failed_only).await
}

/// Which system `dispatch` starts CI on: the forge (`None`), or a CI
/// system outside it.
fn dispatch_target<'a>(
    session: &'a Session,
    ci: Option<&str>,
) -> Result<Option<&'a Provider>, ToolError> {
    let caps = &session.capabilities;
    let forge = caps.available_on(Feature::CiDispatch, None);
    let outside: Vec<&Provider> = session
        .providers
        .iter()
        .filter(|p| caps.available_on(Feature::CiDispatch, Some(p.name())))
        .collect();
    let wanted = ci.map(|c| c.trim().to_lowercase());
    match wanted.as_deref() {
        Some("forge" | "actions") | None if forge && (wanted.is_some() || outside.is_empty()) => {
            Ok(None)
        }
        Some("forge" | "actions") => Err(ToolError(format!(
            "the forge can't start CI here: {}",
            caps.reason_on(Feature::CiDispatch, None)
        ))),
        None if outside.len() == 1 && !forge => Ok(Some(outside[0])),
        None if outside.is_empty() => Ok(None),
        None => Err(ToolError(format!(
            "this repository's CI can be started in more than one place; say which with ci=\"{}\"",
            forge
                .then_some("forge")
                .into_iter()
                .chain(outside.iter().map(|p| p.name()))
                .collect::<Vec<_>>()
                .join("\" or ci=\"")
        ))),
        Some(name) => outside
            .iter()
            .find(|p| p.name() == name || p.name().starts_with(&format!("{name} ")))
            .copied()
            .map(Some)
            .ok_or_else(|| {
                ToolError(format!(
                    "no CI system named {name} can start CI here: {}",
                    std::iter::once("forge")
                        .filter(|_| forge)
                        .chain(outside.iter().map(|p| p.name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }),
    }
}

async fn dispatch(session: &Session, params: &CiControlParams) -> ToolResult {
    if let Some(provider) = dispatch_target(session, params.ci.as_deref())? {
        return dispatch_outside(session, params, provider).await;
    }
    let workflow = params
        .workflow
        .as_deref()
        .filter(|w| !w.trim().is_empty())
        .ok_or_else(|| ToolError("`dispatch` needs `workflow`, its file name".into()))?;
    let git_ref = params
        .git_ref
        .clone()
        .unwrap_or_else(|| session.info.default_branch.clone());
    let inputs = params.inputs.clone().unwrap_or_default();
    let started = session
        .forge
        .dispatch(&session.repo, workflow, &git_ref, &inputs)
        .await?;
    let mut note = Vec::new();
    if started.run_number.is_none() {
        note.push(format!(
            "this forge version doesn't say which run it started; ci_status branch={git_ref} will show it shortly"
        ));
    }
    Ok(json!({
        "action": "dispatch",
        "workflow": workflow,
        "ref": git_ref,
        "started": started,
        "notes": note,
    }))
}

/// A manual pipeline on a CI system outside the forge.
async fn dispatch_outside(
    session: &Session,
    params: &CiControlParams,
    provider: &Provider,
) -> ToolResult {
    let workflow = params.workflow.as_deref().filter(|w| !w.trim().is_empty());
    if workflow.is_some() && !provider.takes_workflow() {
        return Err(ToolError(format!(
            "{} runs only whole pipelines before Woodpecker 3.20; leave out `workflow`",
            provider.name()
        )));
    }
    let explicit = params
        .git_ref
        .as_deref()
        .is_some_and(|r| !r.trim().is_empty());
    let git_ref = params
        .git_ref
        .clone()
        .filter(|_| explicit)
        .unwrap_or_else(|| session.info.default_branch.clone());
    let inputs = params.inputs.clone().unwrap_or_default();
    let (started, notes) = provider
        .dispatch((&git_ref, explicit), &inputs, workflow)
        .await?;
    Ok(json!({
        "action": "dispatch",
        "ci": provider.name(),
        "workflow": workflow,
        "ref": git_ref,
        "started": started,
        "notes": notes,
    }))
}

async fn cancel(session: &Session, params: &CiControlParams) -> ToolResult {
    let run = pick_run(session, params, Feature::CiCancel).await?;
    if !run.is_cancellable() {
        return Err(ToolError(format!(
            "run {} has already finished",
            run.number
        )));
    }
    if let Some((provider, number)) = provider_with(session, &run, Feature::CiCancel) {
        let (done, notes) = provider.cancel(&run, number).await?;
        let after = provider.refresh(&run, number).await?;
        return Ok(control_reply(params.action, &run, &done, after, notes));
    }
    session.forge.cancel_run(&session.repo, &run).await?;
    let after = run_by_number(session, run.number).await?;
    let done = vec![format!("run {}", run.number)];
    Ok(control_reply(params.action, &run, &done, after, Vec::new()))
}

/// How often, and how far apart, a rerun is looked at for a cancellation
/// by the forge before it is reported as started.
const RERUN_CHECKS: usize = 4;
const RERUN_CHECK_PAUSE: std::time::Duration = std::time::Duration::from_millis(1500);

async fn rerun(session: &Session, params: &CiControlParams, failed_only: bool) -> ToolResult {
    let feature = if failed_only {
        Feature::CiRerunFailed
    } else {
        Feature::CiRerun
    };
    let run = pick_run(session, params, feature).await?;
    if !run.state.is_final() {
        return Err(ToolError(format!(
            "run {} is still going; cancel it or wait for it before rerunning",
            run.number
        )));
    }
    if let Some((provider, number)) = provider_with(session, &run, Feature::CiRerun) {
        let (done, started, notes) = provider.rerun(&run, number).await?;
        let after = match started {
            Some(n) => provider.run_by_number(n).await?,
            None => None,
        };
        return Ok(control_reply(params.action, &run, &done, after, notes));
    }
    let done = session
        .forge
        .rerun(&session.repo, &run, failed_only)
        .await?;
    // A forge can cancel a rerun before any runner takes it: Forgejo's
    // default concurrency cancels a waiting job while a newer run of the
    // same workflow on the same branch is going. Look for that briefly,
    // rather than report a rerun that will never run.
    let mut notes = Vec::new();
    let mut latest = None;
    for _ in 0..RERUN_CHECKS {
        tokio::time::sleep(RERUN_CHECK_PAUSE).await;
        latest = run_by_number(session, run.number).await?;
        let cancelled = latest.as_ref().is_some_and(|r| {
            r.jobs
                .iter()
                .filter(|j| !failed_only || done.contains(&j.name))
                .any(|j| j.state == ci::State::Cancelled)
        });
        if cancelled {
            notes.push(format!(
                "the forge cancelled the rerun before it ran. Forgejo does this while a newer run of the same \
                 workflow on the same branch is still going; wait for that one, or rerun it instead \
                 (ci_status branch={} lists them)",
                run.git_ref
            ));
            break;
        }
    }
    Ok(control_reply(params.action, &run, &done, latest, notes))
}

fn control_reply(
    action: ControlAction,
    run: &Run,
    done: &[String],
    after: Option<Run>,
    notes: Vec<String>,
) -> Value {
    json!({
        "action": action.name(),
        "run": run.number,
        "done": done,
        "run_now": after.as_ref().map(|r| json!({
            "state": r.state,
            "summary": ci::summarize(std::slice::from_ref(r)),
        })),
        "notes": notes,
    })
}

/// The one run `params` names: by number, or the only one that fits for the
/// commit or branch.
async fn pick_run(
    session: &Session,
    params: &CiControlParams,
    feature: Feature,
) -> Result<Run, ToolError> {
    let mut selected = select(session, &params.target).await?;
    let (controllable, other): (Vec<Run>, Vec<Run>) = selected
        .runs
        .drain(..)
        .partition(|r| available_for(session, r, feature));
    selected.runs = controllable;
    if selected.runs.is_empty()
        && let Some(run) = other.first()
    {
        return Err(ToolError(uncontrollable(
            session,
            run,
            (params.action, feature),
            &selected.label,
        )));
    }
    let fits = |r: &&Run| match params.action {
        ControlAction::Cancel => r.is_cancellable(),
        _ => r.state.is_final(),
    };
    let mut candidates: Vec<&Run> = selected.runs.iter().filter(fits).collect();
    if candidates.is_empty() {
        candidates = selected.runs.iter().collect();
    }
    match candidates.as_slice() {
        [] => Err(ToolError(format!(
            "there are no CI runs for {}",
            selected.label
        ))),
        [one] => Ok((*one).clone()),
        several => Err(ToolError(format!(
            "{} has several runs ({}); say which with run=<number>",
            selected.label,
            several
                .iter()
                .map(|r| format!("{} {} {:?}", r.number, r.workflow, r.state).to_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Why `action` can't be done to `run` here.
fn uncontrollable(
    session: &Session,
    run: &Run,
    (action, feature): (ControlAction, Feature),
    label: &str,
) -> String {
    let provider = external::provider_for(&session.providers, run).map(|(p, _)| p.name());
    let Some(system) = &run.external else {
        return format!(
            "{label}: the forge can't {} its own runs: {}",
            action.name(),
            session.capabilities.reason_on(feature, None)
        );
    };
    let link = run
        .url
        .as_deref()
        .map(|u| format!(" ({u})"))
        .unwrap_or_default();
    match provider {
        Some(name) => format!(
            "{label} runs on {name}, which can't {} it here: {}. Do it there{link}",
            action.name(),
            session.capabilities.reason_on(feature, Some(name))
        ),
        None => format!(
            "{label} runs on {system}, outside the forge, which reports it only through commit statuses: {} it there{link}",
            action.name(),
        ),
    }
}

// ---------------------------------------------------------------- ci_wait

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CiWaitParams {
    #[serde(flatten)]
    pub target: Target,
    /// Give up after this many seconds (default 300, at most 1800) and
    /// report where things stand.
    pub timeout_seconds: Option<u64>,
    /// Return as soon as a job fails, without waiting for the rest
    /// (default true).
    pub fail_fast: Option<bool>,
    /// Which jobs to list: `attention` (default: failed, cancelled and
    /// unfinished ones), `all`, or `none`.
    #[serde(default)]
    pub jobs: JobsShown,
}

/// The longest a wait may run, in seconds.
pub const MAX_WAIT: u64 = 1800;
/// ci_wait's wait when not given one, in seconds.
const DEFAULT_WAIT: u64 = 300;
/// Seconds between polls: often at first, then less, by how long ci_wait
/// has waited so far.
const POLL_PAUSES: &[(u64, u64)] = &[(60, 5), (300, 10)];
const LATE_POLL_PAUSE: u64 = 20;

fn poll_pause(waited: u64) -> u64 {
    POLL_PAUSES
        .iter()
        .find(|&&(before, _)| waited < before)
        .map_or(LATE_POLL_PAUSE, |&(_, pause)| pause)
}
/// How long to wait for a run to appear after a push before saying none did.
const NO_RUN_GRACE: u64 = 90;

/// Reports how the wait is going; the server sends it on as MCP progress.
pub type Progress<'a> = &'a (
        dyn Fn(u64, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Sync
    );

pub async fn ci_wait(
    session: &Session,
    params: &CiWaitParams,
    progress: Option<Progress<'_>>,
    cancelled: &tokio_util::sync::CancellationToken,
) -> ToolResult {
    let timeout = params
        .timeout_seconds
        .unwrap_or(DEFAULT_WAIT)
        .clamp(1, MAX_WAIT);
    let fail_fast = params.fail_fast.unwrap_or(true);
    let started = std::time::Instant::now();
    let first = select(session, &params.target).await?;
    let (label, sha) = (first.label.clone(), first.sha.clone());
    let mut runs = first.runs;
    let mut polls = 1;
    let mut histories = Histories::new();
    let (verdict, timed_out) = loop {
        let verdict = ci::verdict(&runs);
        let waited = started.elapsed().as_secs();
        let finished = matches!(
            verdict,
            Verdict::Success | Verdict::Failure | Verdict::NothingRan
        ) || (fail_fast && verdict == Verdict::Failing)
            || (verdict == Verdict::NoRuns && waited >= NO_RUN_GRACE.min(timeout));
        if finished {
            break (verdict, false);
        }
        if waited >= timeout {
            break (verdict, true);
        }
        if let Some(report) = progress {
            let etas = estimates(session, &runs, &mut histories).await;
            report(waited, with_etas(ci::progress(&runs), &etas)).await;
        }
        let pause = std::time::Duration::from_secs(poll_pause(waited).min(timeout - waited).max(1));
        tokio::select! {
            _ = tokio::time::sleep(pause) => {}
            _ = cancelled.cancelled() => return Err(ToolError("the wait was cancelled".into())),
        }
        runs = match (params.target.run, &sha) {
            (Some(number), _) => run_by_number(session, number).await?.into_iter().collect(),
            (None, Some(sha)) => runs_for(session, sha).await?,
            (None, None) => Vec::new(),
        };
        polls += 1;
    };
    let mut notes = first.notes;
    if timed_out {
        notes.push(format!(
            "still {} after {timeout} seconds; call ci_wait again to keep waiting",
            serde_json::to_value(verdict)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ));
    }
    if verdict == Verdict::NoRuns {
        notes.push(format!(
            "no run appeared within {} seconds: the commit may not be pushed, or no workflow runs on it",
            started.elapsed().as_secs()
        ));
    }
    let etas = estimates(session, &runs, &mut histories).await;
    let (summary, progress) = (ci::summarize(&runs), with_etas(ci::progress(&runs), &etas));
    let omitted = params.jobs.apply(&mut runs);
    Ok(json!({
        "verdict": verdict,
        "summary": summary,
        "progress": progress,
        "estimates": etas,
        "target": label,
        "commit": sha,
        "waited_seconds": started.elapsed().as_secs(),
        "timed_out": timed_out,
        "polls": polls,
        "notes": notes,
        "jobs_omitted": omitted,
        "runs": runs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: PrState, mergeable: Option<bool>) -> PullRequest {
        PullRequest {
            number: 7,
            title: "t".into(),
            state,
            draft: false,
            author: "dev".into(),
            head: "feature/x".into(),
            head_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            base: "main".into(),
            mergeable,
            labels: vec![],
            url: String::new(),
            created_at: None,
            updated_at: None,
            merged_at: None,
            merge_commit_sha: None,
            body: None,
        }
    }

    #[test]
    fn merges_only_what_was_reviewed_and_passed() {
        let open = pr(PrState::Open, Some(true));
        assert_eq!(
            merge_refusal(&open, Some("0123456"), Some(Verdict::Success), true),
            None
        );
        assert_eq!(
            merge_refusal(&open, None, Some(Verdict::Success), true),
            None
        );
        let moved = merge_refusal(&open, Some("fedcba9876"), Some(Verdict::Success), true).unwrap();
        assert!(moved.contains("has moved"), "{moved}");
        // A short prefix is too easy to match by accident.
        assert!(merge_refusal(&open, Some("0123"), Some(Verdict::Success), true).is_some());
    }

    #[test]
    fn refuses_conflicts_and_closed_pull_requests() {
        let conflicted = merge_refusal(
            &pr(PrState::Open, Some(false)),
            None,
            Some(Verdict::Success),
            false,
        );
        assert!(conflicted.unwrap().contains("conflicts with main"));
        let merged = merge_refusal(&pr(PrState::Merged, None), None, None, false).unwrap();
        assert!(merged.contains("not open"), "{merged}");
    }

    #[test]
    fn ci_must_pass_unless_told_otherwise() {
        let open = pr(PrState::Open, Some(true));
        for (verdict, words) in [
            (Some(Verdict::Failure), "is failure"),
            (Some(Verdict::Pending), "is pending"),
            (Some(Verdict::NothingRan), "was skipped"),
            (Some(Verdict::NoRuns), "no CI run"),
            (None, "can't report CI"),
        ] {
            let why = merge_refusal(&open, None, verdict, true).unwrap();
            assert!(why.contains(words), "{verdict:?}: {why}");
            assert!(why.contains("require_ci=false"));
            assert_eq!(merge_refusal(&open, None, verdict, false), None);
        }
    }

    #[test]
    fn schedules_only_where_the_forge_will_wait_for_checks() {
        let open = pr(PrState::Open, Some(true));
        let none = schedule_refusal(&open, Some(&StatusChecks::default())).unwrap();
        assert!(none.contains("doesn't require status checks"), "{none}");
        assert!(none.contains("ci_wait sha=0123456789abcdef"), "{none}");
        assert!(
            schedule_refusal(&open, None)
                .unwrap()
                .contains("doesn't exist")
        );
        let required = StatusChecks {
            required: true,
            contexts: vec!["ci / test (push)".into()],
        };
        assert_eq!(schedule_refusal(&open, Some(&required)), None);
        // Every check counts when protection names none.
        let all = StatusChecks {
            required: true,
            contexts: vec![],
        };
        assert_eq!(schedule_refusal(&open, Some(&all)), None);
    }
}
