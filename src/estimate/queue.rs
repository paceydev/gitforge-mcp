//! What a job waiting for a runner waits for, and what a run that has gone
//! on longer than usual shares its runners with.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;

use crate::ci::{Run, State};
use crate::context::Session;
use crate::forge::Forge;
use crate::runners::{self, Activity, Pool, RepoLoad, RunnerReport, label_set, repo_list};

/// How long a runner report is reused for queue estimates.
const RUNNERS_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// A job waiting for a runner when none with its labels is free.
#[derive(Debug, Clone, Serialize)]
pub struct QueueWait {
    pub job: String,
    pub labels: Vec<String>,
    pub runners_busy: usize,
    /// Other jobs waiting for runners with the same labels, from every
    /// repository the credential can see.
    pub others_waiting: usize,
    /// The repositories whose jobs are running on those runners.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held_by: Vec<RepoLoad>,
    pub summary: String,
}

/// Waits worth mentioning for `run`'s waiting jobs: those with no idle
/// runner to take them. A job a free runner will take, or one for runners
/// the forge hosts, gets nothing.
pub fn queue_waits(run: &Run, report: &RunnerReport, repo: &str) -> Vec<QueueWait> {
    let Some(waiting) = &report.waiting else {
        return Vec::new();
    };
    run.jobs
        .iter()
        .filter(|j| j.state == State::Waiting)
        .filter_map(|job| waiting.iter().find(|w| w.name == job.name))
        .filter(|w| !w.hosted && w.matching_idle == 0)
        .map(|w| {
            let pool = report.pool(&w.labels);
            // The pool counts every repository's jobs; without one, only
            // this repository's are known.
            let others = pool.map_or_else(
                || {
                    let set = label_set(&w.labels);
                    waiting.iter().filter(|o| label_set(&o.labels) == set).count() - 1
                },
                |p| p.waiting.saturating_sub(1),
            );
            let held_by: Vec<RepoLoad> = pool
                .map(|p| p.repos.iter().filter(|r| r.running > 0).cloned().collect())
                .unwrap_or_default();
            let labels = w.labels.join(", ");
            let summary = if w.matching_busy == 0 {
                format!(
                    "{} waits for a runner with labels {labels}: none is online (one may start on demand)",
                    w.name
                )
            } else {
                let mut text = format!(
                    "{} waits for a runner: all {} with labels {labels} are busy",
                    w.name, w.matching_busy
                );
                if !held_by.is_empty() {
                    text.push_str(&format!(
                        ", running {}",
                        repo_list(&held_by, |r| r.running)
                    ));
                }
                match others {
                    0 => {}
                    1 => text.push_str(", and 1 other job waits for them"),
                    n => text.push_str(&format!(", and {n} other jobs wait for them")),
                }
                // Whose they are: every repository's waiting jobs, less
                // this one.
                if let Some(p) = pool.filter(|_| others > 0) {
                    let mut ahead = p.repos.clone();
                    if let Some(own) = ahead
                        .iter_mut()
                        .find(|r| r.waiting > 0 && r.repo.eq_ignore_ascii_case(repo))
                    {
                        own.waiting -= 1;
                    }
                    text.push_str(&format!(" ({})", repo_list(&ahead, |r| r.waiting)));
                }
                text
            };
            QueueWait {
                job: w.name.clone(),
                labels: w.labels.clone(),
                runners_busy: w.matching_busy,
                others_waiting: others,
                held_by,
                summary,
            }
        })
        .collect()
}

/// What a run that has gone on longer than nine in ten of its passes shares
/// its runners with. The forges don't say how loaded a runner's machine
/// is; this is the evidence they do give.
#[derive(Debug, Clone, Serialize)]
pub struct Contention {
    /// The labels its running jobs asked for.
    pub labels: Vec<String>,
    /// Other repositories with jobs running on those runners.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<RepoLoad>,
    /// Those runners are taking several jobs at once.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub doubled_up: bool,
    pub summary: String,
}

/// Contention on the runners `run`'s running jobs are on, in `repo`. The
/// strongest sign is runners taking several jobs at once; weaker, other
/// repositories' jobs on the same runners, which slow it only if they share
/// a machine. Where neither shows and every job on the instance was seen,
/// it says the runners run nothing else. `None` where nothing can be said.
pub fn contention(run: &Run, report: &RunnerReport, repo: &str) -> Option<Contention> {
    let mut pools: Vec<&Pool> = Vec::new();
    for job in run.jobs.iter().filter(|j| j.state == State::Running) {
        let Some(active) = report.active.iter().find(|a| {
            a.activity == Activity::Running
                && a.name == job.name
                && a.repo.eq_ignore_ascii_case(repo)
        }) else {
            continue;
        };
        if let Some(pool) = report.pool(&active.labels)
            && !pools.iter().any(|p| p.labels == pool.labels)
        {
            pools.push(pool);
        }
    }
    let worst = pools
        .iter()
        .max_by_key(|p| (p.doubled_up, p.others_running(repo).len()))?;
    let others: Vec<RepoLoad> = worst.others_running(repo).into_iter().cloned().collect();
    let labels = worst.labels.join(", ");
    let summary = if worst.doubled_up {
        format!(
            "likely slowed by its runners taking several jobs at once: {}",
            worst.summary
        )
    } else if !others.is_empty() {
        format!(
            "its runners ({labels}) are also running jobs from {}; if they share a machine, that may be slowing it",
            repo_list(&others, |r| r.running)
        )
    } else if runners::every_job_seen(report) {
        format!("its runners ({labels}) run nothing else, so the time is going on the run itself")
    } else {
        return None;
    };
    Some(Contention {
        labels: worst.labels.clone(),
        doubled_up: worst.doubled_up,
        others,
        summary,
    })
}

/// The forge's runners and waiting jobs, read at most every `RUNNERS_TTL`.
/// `None` if they can't be read.
pub async fn runner_report(session: &Session) -> Option<RunnerReport> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, RunnerReport)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = format!("{}/{}", session.repo.host, session.repo.full_name());
    if let Some((at, report)) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        && at.elapsed() < RUNNERS_TTL
    {
        return Some(report.clone());
    }
    let report = session
        .forge
        .runners(&session.repo, session.user.as_ref())
        .await
        .inspect_err(|e| tracing::debug!("runners unreadable: {e}"))
        .ok()?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (Instant::now(), report.clone()));
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::shape::tests::{job, run};
    use crate::runners::{
        self, ActiveJob, ActiveJobs, Runner, RunnerState, Scope, ScopeJobs, ScopeOutcome,
    };

    fn active(id: u64, name: &str, repo: &str, activity: Activity) -> ActiveJob {
        ActiveJob {
            id,
            name: name.into(),
            repo: repo.into(),
            labels: vec!["docker".into()],
            activity,
            runner: None,
            queued_at: None,
        }
    }

    fn busy(id: u64) -> Runner {
        Runner {
            id,
            name: format!("r{id}"),
            state: RunnerState::Busy,
            labels: vec!["docker".into()],
            scope: Scope::Instance,
            version: None,
        }
    }

    /// A report from `jobs`, listed at the instance scope when `all`, with
    /// `runners` and every runner scope read.
    fn report(runners: Vec<Runner>, jobs: Vec<ActiveJob>, all: bool) -> RunnerReport {
        let wide = if all { Scope::Instance } else { Scope::Owner };
        let listed = ActiveJobs::of(vec![
            ScopeJobs::listed(Scope::Repository, "/r".into(), Vec::new()),
            ScopeJobs::listed(wide, "/i".into(), jobs),
        ]);
        let waiting = listed
            .jobs
            .iter()
            .filter(|j| j.activity == Activity::Waiting && j.repo == "acme/widgets")
            .map(|j| (j.name.clone(), j.labels.clone()))
            .collect();
        let scopes = vec![ScopeOutcome {
            scope: Scope::Instance,
            endpoint: "/runners".into(),
            outcome: "listed".into(),
            runners: runners.len(),
        }];
        runners::report(runners, scopes, Some(waiting), listed, |_| false)
    }

    #[test]
    fn a_wait_names_who_holds_the_runners() {
        let mut ours = run(7, State::Running, 0);
        ours.jobs = vec![job("lint", State::Waiting)];
        let report = report(
            vec![busy(1), busy(2)],
            vec![
                active(1, "build", "acme/infra", Activity::Running),
                active(2, "test", "acme/infra", Activity::Running),
                active(3, "lint", "acme/widgets", Activity::Waiting),
                active(4, "deploy", "acme/site", Activity::Waiting),
            ],
            true,
        );
        let waits = queue_waits(&ours, &report, "acme/widgets");
        assert_eq!(waits.len(), 1);
        assert_eq!(
            waits[0].summary,
            "lint waits for a runner: all 2 with labels docker are busy, running acme/infra 2, and 1 other job waits for them (acme/site)"
        );
        assert_eq!(waits[0].others_waiting, 1);
        assert_eq!(waits[0].held_by[0].repo, "acme/infra");
    }

    #[test]
    fn a_slow_run_on_doubled_up_runners_is_likely_contention() {
        let mut ours = run(7, State::Running, 0);
        ours.jobs = vec![job("build", State::Running)];
        let report = report(
            vec![busy(1)],
            vec![
                active(1, "build", "acme/widgets", Activity::Running),
                active(2, "build", "acme/infra", Activity::Running),
            ],
            false,
        );
        let c = contention(&ours, &report, "acme/widgets").unwrap();
        assert!(c.doubled_up);
        assert_eq!(c.others[0].repo, "acme/infra");
        assert!(
            c.summary.starts_with(
                "likely slowed by its runners taking several jobs at once: docker: 1 of 1 runners busy, 2 jobs running"
            ),
            "{}",
            c.summary
        );
    }

    #[test]
    fn other_repositories_on_the_same_runners_are_only_a_maybe() {
        let mut ours = run(7, State::Running, 0);
        ours.jobs = vec![job("build", State::Running)];
        let report = report(
            vec![busy(1), busy(2)],
            vec![
                active(1, "build", "Acme/Widgets", Activity::Running),
                active(2, "build", "acme/infra", Activity::Running),
            ],
            false,
        );
        let c = contention(&ours, &report, "acme/widgets").unwrap();
        assert!(!c.doubled_up);
        assert_eq!(
            c.summary,
            "its runners (docker) are also running jobs from acme/infra; if they share a machine, that may be slowing it"
        );
    }

    #[test]
    fn runners_running_nothing_else_are_said_so_only_when_every_job_was_seen() {
        let mut ours = run(7, State::Running, 0);
        ours.jobs = vec![job("build", State::Running)];
        let jobs = vec![active(1, "build", "acme/widgets", Activity::Running)];
        let seen = report(vec![busy(1)], jobs.clone(), true);
        assert_eq!(
            contention(&ours, &seen, "acme/widgets").unwrap().summary,
            "its runners (docker) run nothing else, so the time is going on the run itself"
        );
        let partly = report(vec![busy(1)], jobs, false);
        assert!(contention(&ours, &partly, "acme/widgets").is_none());
    }
}
