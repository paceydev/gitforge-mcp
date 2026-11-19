//! Runners and the jobs waiting for them.
//!
//! A forge lists runners per scope: those registered to the repository, to
//! its owner (user or organisation), and to the whole instance. Most jobs
//! run on instance-wide runners, which the repository's own list doesn't
//! include, so every scope the credential can read is asked, and each
//! scope's outcome is reported: a short list is only meaningful alongside
//! which scopes could not be read.

mod jobs;
mod pools;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

pub use jobs::{ActiveJob, ActiveJobs, Activity, JobScopeOutcome, ScopeJobs};
pub use pools::{Pool, RepoLoad, label_set, repo_list};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Idle,
    Busy,
    Offline,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Repository,
    Owner,
    Instance,
}

#[derive(Debug, Clone, Serialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub state: RunnerState,
    pub labels: Vec<String>,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// What one scope said.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeOutcome {
    pub scope: Scope,
    /// The endpoint asked.
    pub endpoint: String,
    /// `listed`, `not_permitted` (and why) or `unavailable`.
    pub outcome: String,
    pub runners: usize,
}

/// A job waiting for a runner, and which visible runners could take it.
#[derive(Debug, Clone, Serialize)]
pub struct WaitingJob {
    pub name: String,
    pub labels: Vec<String>,
    /// It asked for a runner the forge hosts (GitHub-hosted), which the API
    /// doesn't list, so no visible runner matching it says nothing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hosted: bool,
    /// Visible runners carrying every label: idle, busy, offline.
    pub matching_idle: usize,
    pub matching_busy: usize,
    pub matching_offline: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerReport {
    pub summary: String,
    pub runners: Vec<Runner>,
    pub scopes: Vec<ScopeOutcome>,
    /// `None` when the forge can't list waiting jobs.
    pub waiting: Option<Vec<WaitingJob>>,
    /// The runners for each label set jobs asked for, and which
    /// repositories' jobs are running on them and waiting for them, from
    /// every scope the credential can list jobs in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<Pool>,
    /// What each scope's job listing said.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub job_scopes: Vec<JobScopeOutcome>,
    /// The jobs the pools were made from.
    #[serde(skip)]
    pub active: Vec<ActiveJob>,
}

impl RunnerReport {
    /// The pool for `labels`.
    pub fn pool(&self, labels: &[String]) -> Option<&Pool> {
        let set = label_set(labels);
        self.pools.iter().find(|p| p.labels == set)
    }
}

/// Whether every running and waiting job on the instance was listed, so a
/// pool with nothing else in it really has nothing else.
pub fn every_job_seen(report: &RunnerReport) -> bool {
    jobs::listed(&report.job_scopes, Scope::Instance)
}

/// Whether every scope's runners were read: none was refused.
pub fn runners_complete(scopes: &[ScopeOutcome]) -> bool {
    !scopes
        .iter()
        .any(|s| s.outcome.starts_with("not_permitted"))
}

/// Counts visible runners that carry all of `labels`.
pub fn matching(runners: &[Runner], labels: &[String]) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for r in runners
        .iter()
        .filter(|r| labels.iter().all(|l| r.labels.iter().any(|rl| rl == l)))
    {
        match r.state {
            RunnerState::Idle => counts.0 += 1,
            RunnerState::Busy => counts.1 += 1,
            RunnerState::Offline | RunnerState::Unknown => counts.2 += 1,
        }
    }
    counts
}

/// Pools named in a report's summary: those with jobs waiting for busy
/// runners, or runners taking several jobs at once.
const POOLS_SUMMARIZED: usize = 3;

pub fn summarize(
    runners: &[Runner],
    scopes: &[ScopeOutcome],
    waiting: Option<&[WaitingJob]>,
    pools: &[Pool],
) -> String {
    let count = |s: RunnerState| runners.iter().filter(|r| r.state == s).count();
    let mut parts = vec![format!(
        "{} runners visible: {} idle, {} busy, {} offline",
        runners.len(),
        count(RunnerState::Idle),
        count(RunnerState::Busy),
        count(RunnerState::Offline)
    )];
    let hidden: Vec<String> = scopes
        .iter()
        .filter(|s| s.outcome != "listed")
        .map(|s| format!("{:?}", s.scope).to_lowercase())
        .collect();
    if !hidden.is_empty() {
        parts.push(format!(
            "not visible with this credential: {} runners",
            hidden.join(" and ")
        ));
    }
    if let Some(waiting) = waiting {
        let stuck = waiting
            .iter()
            .filter(|w| !w.hosted && w.matching_idle + w.matching_busy == 0)
            .count();
        let hosted = waiting.iter().filter(|w| w.hosted).count();
        parts.push(format!("{} jobs waiting", waiting.len()));
        if hosted > 0 {
            parts.push(format!(
                "{hosted} of them for hosted runners, which aren't listed"
            ));
        }
        if stuck > 0 {
            parts.push(format!("{stuck} of them match no visible online runner"));
        }
    }
    parts.extend(
        pools
            .iter()
            // A pool with no runner busy adds nothing to "match no visible
            // online runner" above.
            .filter(|p| (p.waiting > 0 && p.runners_busy > 0) || p.doubled_up)
            .take(POOLS_SUMMARIZED)
            .map(|p| p.summary.clone()),
    );
    parts.join("; ")
}

/// The report for runners found in each scope, this repository's jobs
/// waiting, as `(name, labels)`, and the jobs running and waiting in each
/// scope the forge lists them for. `hosted` says which jobs asked for a
/// runner the forge hosts and doesn't list.
pub fn report(
    mut runners: Vec<Runner>,
    scopes: Vec<ScopeOutcome>,
    waiting: Option<Vec<(String, Vec<String>)>>,
    active: ActiveJobs,
    hosted: impl Fn(&[String]) -> bool,
) -> RunnerReport {
    // A job that names the runner it's running on outranks the runner's own
    // flag: Gitea's says idle for a while after the runner took a job.
    for r in runners.iter_mut().filter(|r| r.state == RunnerState::Idle) {
        if active.jobs.iter().any(|j| {
            j.activity == Activity::Running && j.runner.as_deref() == Some(r.name.as_str())
        }) {
            r.state = RunnerState::Busy;
        }
    }
    let waiting = waiting.map(|jobs| {
        jobs.into_iter()
            .map(|(name, labels)| {
                let (idle, busy, offline) = matching(&runners, &labels);
                WaitingJob {
                    hosted: hosted(&labels),
                    name,
                    labels,
                    matching_idle: idle,
                    matching_busy: busy,
                    matching_offline: offline,
                }
            })
            .collect::<Vec<_>>()
    });
    let pools = pools::pools(
        &runners,
        &active.jobs,
        runners_complete(&scopes),
        &hosted,
        Utc::now(),
    );
    RunnerReport {
        summary: summarize(&runners, &scopes, waiting.as_deref(), &pools),
        runners,
        scopes,
        waiting,
        pools,
        job_scopes: active.scopes,
        active: active.jobs,
    }
}

/// Runners from either shape: Gitea's `{runners: [...]}` with label
/// objects and a `busy` flag, or Forgejo's bare array with label strings
/// and `active` for busy. GitHub's is Gitea's.
pub fn parse_runners(body: &Value, scope: Scope) -> Vec<Runner> {
    let items = match body {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("runners")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        _ => &[],
    };
    items
        .iter()
        .map(|r| {
            let status = r["status"].as_str().unwrap_or_default();
            let busy = r["busy"].as_bool().unwrap_or(false);
            let state = match status {
                "offline" => RunnerState::Offline,
                "active" | "busy" => RunnerState::Busy,
                "online" | "idle" if busy => RunnerState::Busy,
                "online" | "idle" => RunnerState::Idle,
                _ => RunnerState::Unknown,
            };
            let labels = r["labels"]
                .as_array()
                .map(|ls| {
                    ls.iter()
                        .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Runner {
                id: r["id"].as_u64().unwrap_or_default(),
                name: r["name"].as_str().unwrap_or_default().to_owned(),
                state,
                labels,
                scope,
                version: r["version"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(state: RunnerState, labels: &[&str]) -> Runner {
        Runner {
            id: 1,
            name: "r".into(),
            state,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            scope: Scope::Instance,
            version: None,
        }
    }

    #[test]
    fn matches_on_every_label() {
        let runners = vec![
            runner(RunnerState::Idle, &["linux", "docker"]),
            runner(RunnerState::Busy, &["macos-arm64"]),
            runner(RunnerState::Offline, &["macos-arm64"]),
        ];
        assert_eq!(matching(&runners, &["macos-arm64".into()]), (0, 1, 1));
        assert_eq!(
            matching(&runners, &["linux".into(), "docker".into()]),
            (1, 0, 0)
        );
        assert_eq!(matching(&runners, &["windows".into()]), (0, 0, 0));
    }

    #[test]
    fn summary_names_hidden_scopes_and_stuck_jobs() {
        let scopes = vec![
            ScopeOutcome {
                scope: Scope::Repository,
                endpoint: "x".into(),
                outcome: "listed".into(),
                runners: 0,
            },
            ScopeOutcome {
                scope: Scope::Instance,
                endpoint: "y".into(),
                outcome: "not_permitted: token lacks read:admin".into(),
                runners: 0,
            },
        ];
        let waiting = vec![WaitingJob {
            hosted: false,
            name: "build".into(),
            labels: vec!["windows".into()],
            matching_idle: 0,
            matching_busy: 0,
            matching_offline: 0,
        }];
        assert_eq!(
            summarize(&[], &scopes, Some(&waiting), &[]),
            "0 runners visible: 0 idle, 0 busy, 0 offline; not visible with this credential: instance runners; 1 jobs waiting; 1 of them match no visible online runner"
        );
    }

    #[test]
    fn parses_both_runner_shapes() {
        let gitea = serde_json::json!({"runners": [
            {"id": 1, "name": "a", "status": "online", "busy": true, "labels": [{"id": 1, "name": "ubuntu-latest", "type": "custom"}]},
            {"id": 2, "name": "b", "status": "offline", "busy": false, "labels": []}
        ], "total_count": 2});
        let r = parse_runners(&gitea, Scope::Repository);
        assert_eq!(r[0].state, RunnerState::Busy);
        assert_eq!(r[0].labels, ["ubuntu-latest"]);
        assert_eq!(r[1].state, RunnerState::Offline);

        let forgejo = serde_json::json!([
            {"id": 3, "name": "c", "status": "active", "labels": ["docker"], "version": "v12.0.0"},
            {"id": 4, "name": "d", "status": "idle", "labels": ["macos-arm64"], "version": ""}
        ]);
        let r = parse_runners(&forgejo, Scope::Instance);
        assert_eq!(r[0].state, RunnerState::Busy);
        assert_eq!(r[0].version.as_deref(), Some("v12.0.0"));
        assert_eq!(r[1].state, RunnerState::Idle);
        assert_eq!(r[1].version, None);
    }

    #[test]
    fn hosted_jobs_are_not_counted_as_stuck() {
        let report = report(
            vec![],
            vec![],
            Some(vec![
                ("build".into(), vec!["ubuntu-latest".into()]),
                ("deploy".into(), vec!["self-hosted".into(), "prod".into()]),
            ]),
            ActiveJobs::default(),
            |labels| !labels.iter().any(|l| l == "self-hosted"),
        );
        assert!(report.waiting.as_ref().unwrap()[0].hosted);
        assert_eq!(
            report.summary,
            "0 runners visible: 0 idle, 0 busy, 0 offline; 2 jobs waiting; 1 of them for hosted runners, which aren't listed; 1 of them match no visible online runner"
        );
    }

    #[test]
    fn a_runner_a_job_is_running_on_is_busy_whatever_its_flag_says() {
        let idle = Runner {
            id: 1,
            name: "shared-runner".into(),
            state: RunnerState::Idle,
            labels: vec!["shared".into()],
            scope: Scope::Instance,
            version: None,
        };
        let active = ActiveJobs::of(vec![ScopeJobs::listed(
            Scope::Instance,
            "/admin/actions/jobs".into(),
            vec![ActiveJob {
                id: 1,
                name: "hold".into(),
                repo: "acme/infra".into(),
                labels: vec!["shared".into()],
                activity: Activity::Running,
                runner: Some("shared-runner".into()),
                queued_at: None,
            }],
        )]);
        let r = report(vec![idle], Vec::new(), None, active, |_| false);
        assert_eq!(r.runners[0].state, RunnerState::Busy);
        assert_eq!(r.pools[0].runners_busy, 1);
    }

    #[test]
    fn the_summary_names_who_holds_the_runners_jobs_wait_for() {
        let job = |id, repo: &str, activity| ActiveJob {
            id,
            name: format!("j{id}"),
            repo: repo.into(),
            labels: vec!["docker".into()],
            activity,
            runner: None,
            queued_at: None,
        };
        let active = ActiveJobs::of(vec![
            ScopeJobs::listed(
                Scope::Repository,
                "/repos/acme/widgets/actions/runners/jobs".into(),
                vec![job(3, "acme/widgets", Activity::Waiting)],
            ),
            ScopeJobs::listed(
                Scope::Owner,
                "/orgs/acme/actions/runners/jobs".into(),
                vec![
                    job(1, "acme/infra", Activity::Running),
                    job(2, "acme/infra", Activity::Running),
                    job(3, "acme/widgets", Activity::Waiting),
                ],
            ),
        ]);
        let scopes = vec![ScopeOutcome {
            scope: Scope::Owner,
            endpoint: "/orgs/acme/actions/runners".into(),
            outcome: "listed".into(),
            runners: 2,
        }];
        let busy = |id, name: &str| Runner {
            id,
            name: name.into(),
            state: RunnerState::Busy,
            labels: vec!["docker".into()],
            scope: Scope::Owner,
            version: None,
        };
        let waiting = active.waiting_in("acme/widgets");
        let r = report(
            vec![busy(1, "a"), busy(2, "b")],
            scopes,
            waiting,
            active,
            |_| false,
        );
        assert_eq!(
            r.summary,
            "2 runners visible: 0 idle, 2 busy, 0 offline; 1 jobs waiting; docker: 2 of 2 runners busy, 2 jobs running (acme/infra 2), 1 waiting (acme/widgets)"
        );
        assert_eq!(r.job_scopes[1].jobs, 3);
        assert!(!every_job_seen(&r));
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("active").is_none(), "the raw jobs stay internal");
        assert_eq!(json["pools"][0]["repos"][0]["repo"], "acme/infra");
    }
}
