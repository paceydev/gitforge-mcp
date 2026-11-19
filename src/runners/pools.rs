//! Pools: the runners that carry a set of labels, and the jobs from every
//! repository the credential can see that asked for that set.
//!
//! A forge says a runner is busy, but Forgejo doesn't say which job it is
//! running. When runners are shared between projects, the question is which
//! repositories are holding them and how many jobs wait, and a pool answers
//! that from the jobs alone. A job counts in the pool of exactly the labels
//! it asked for; pools overlap where runners carry several of those sets.
//!
//! Whether runners are taking several jobs at once (a runner's capacity, or
//! several runners on one machine) is the one sign of an overloaded machine
//! the forges give. Gitea names the runner each job runs on, so there it is
//! counted. Elsewhere it shows only as more jobs running than busy runners,
//! which says so only when every runner scope could be read: a runner the
//! credential can't see may be the one running them.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::jobs::{ActiveJob, Activity};
use super::{Runner, matching};
use crate::ci;

/// Repositories named in a pool's summary; the rest are counted.
const REPOS_NAMED: usize = 4;
const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;

/// One repository's jobs in a pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoLoad {
    pub repo: String,
    pub running: usize,
    pub waiting: usize,
}

/// The runners carrying a set of labels, and the jobs that asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct Pool {
    pub labels: Vec<String>,
    pub runners_idle: usize,
    pub runners_busy: usize,
    pub runners_offline: usize,
    pub running: usize,
    pub waiting: usize,
    /// Busiest first.
    pub repos: Vec<RepoLoad>,
    /// How long the longest-waiting job has waited, where the forge says
    /// when jobs were queued (not Forgejo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_wait_seconds: Option<i64>,
    /// Some runners are running several of these jobs at once.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub doubled_up: bool,
    pub summary: String,
}

impl Pool {
    /// Repositories other than `repo` with jobs running here.
    pub fn others_running(&self, repo: &str) -> Vec<&RepoLoad> {
        self.repos
            .iter()
            .filter(|r| !r.repo.eq_ignore_ascii_case(repo) && r.running > 0)
            .collect()
    }
}

/// `labels` sorted, without repeats: one pool per set, whatever the order
/// a workflow gave them in.
pub fn label_set(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A pool for each label set a job asked for, busiest first. Jobs that
/// asked for runners the forge hosts (`hosted`) are left out: those runners
/// aren't listed. `runners_complete` says every scope's runners were read.
pub fn pools(
    runners: &[Runner],
    jobs: &[ActiveJob],
    runners_complete: bool,
    hosted: impl Fn(&[String]) -> bool,
    now: DateTime<Utc>,
) -> Vec<Pool> {
    let mut by_labels: BTreeMap<Vec<String>, Vec<&ActiveJob>> = BTreeMap::new();
    for job in jobs.iter().filter(|j| !hosted(&j.labels)) {
        by_labels
            .entry(label_set(&job.labels))
            .or_default()
            .push(job);
    }
    let mut pools: Vec<Pool> = by_labels
        .into_iter()
        .map(|(labels, jobs)| pool(runners, labels, &jobs, runners_complete, now))
        .collect();
    pools.sort_by(|a, b| {
        (b.waiting, b.running)
            .cmp(&(a.waiting, a.running))
            .then_with(|| a.labels.cmp(&b.labels))
    });
    pools
}

fn pool(
    runners: &[Runner],
    labels: Vec<String>,
    jobs: &[&ActiveJob],
    runners_complete: bool,
    now: DateTime<Utc>,
) -> Pool {
    let (idle, busy, offline) = matching(runners, &labels);
    let running: Vec<&&ActiveJob> = jobs
        .iter()
        .filter(|j| j.activity == Activity::Running)
        .collect();
    let waiting = jobs.len() - running.len();
    let named: BTreeSet<&str> = running.iter().filter_map(|j| j.runner.as_deref()).collect();
    // Gitea says which runner runs each job, so a runner running two shows
    // directly. Otherwise, more jobs running than busy runners shows it,
    // where every runner that could run them was seen.
    let on = if !running.is_empty() && running.iter().all(|j| j.runner.is_some()) {
        Some(named.len())
    } else if runners_complete && busy > 0 {
        Some(busy)
    } else {
        None
    };
    let doubled_up = on.is_some_and(|on| running.len() > on);
    let oldest_wait_seconds = jobs
        .iter()
        .filter(|j| j.activity == Activity::Waiting)
        .filter_map(|j| j.queued_at.as_deref().and_then(ci::parse_time))
        .min()
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0));
    let mut pool = Pool {
        labels,
        runners_idle: idle,
        runners_busy: busy,
        runners_offline: offline,
        running: running.len(),
        waiting,
        repos: repo_loads(jobs),
        oldest_wait_seconds,
        doubled_up,
        summary: String::new(),
    };
    pool.summary = describe(&pool, on);
    pool
}

fn repo_loads(jobs: &[&ActiveJob]) -> Vec<RepoLoad> {
    let mut by_repo: BTreeMap<&str, RepoLoad> = BTreeMap::new();
    for job in jobs {
        let load = by_repo.entry(&job.repo).or_insert_with(|| RepoLoad {
            repo: job.repo.clone(),
            running: 0,
            waiting: 0,
        });
        match job.activity {
            Activity::Running => load.running += 1,
            Activity::Waiting => load.waiting += 1,
        }
    }
    let mut loads: Vec<RepoLoad> = by_repo.into_values().collect();
    loads.sort_by(|a, b| {
        (b.running + b.waiting, b.running)
            .cmp(&(a.running + a.waiting, a.running))
            .then_with(|| a.repo.cmp(&b.repo))
    });
    loads
}

/// "linux, docker: 3 of 3 runners busy, 5 jobs running (acme/infra 3,
/// acme/widgets 2), 4 waiting (acme/widgets 3, acme/site 1), the oldest for
/// 14m; 5 jobs on 3 runners, so some runners are taking several at once".
/// No semicolon but that last one, so pools can be listed with them.
fn describe(p: &Pool, on: Option<usize>) -> String {
    let total = p.runners_idle + p.runners_busy + p.runners_offline;
    let mut text = format!("{}: ", p.labels.join(", "));
    if total == 0 {
        text.push_str("no visible runner carries these labels");
    } else {
        text.push_str(&format!("{} of {total} runners busy", p.runners_busy));
        if p.runners_offline > 0 {
            text.push_str(&format!(", {} offline", p.runners_offline));
        }
    }
    if p.running > 0 {
        text.push_str(&format!(
            ", {} running ({})",
            jobs(p.running),
            repo_list(&p.repos, |r| r.running)
        ));
    }
    if p.waiting > 0 {
        text.push_str(&format!(
            ", {} waiting ({})",
            p.waiting,
            repo_list(&p.repos, |r| r.waiting)
        ));
        if let Some(s) = p.oldest_wait_seconds {
            text.push_str(&format!(", the oldest for {}", duration(s)));
        }
    }
    if p.doubled_up
        && let Some(on) = on
    {
        text.push_str(&format!(
            " - {} on {on} {}, so some runners are taking several at once",
            jobs(p.running),
            if on == 1 { "runner" } else { "runners" }
        ));
    }
    text
}

fn jobs(n: usize) -> String {
    if n == 1 {
        "1 job".into()
    } else {
        format!("{n} jobs")
    }
}

/// "acme/infra 3, acme/widgets" (a count only above one), the busiest
/// `REPOS_NAMED` and how many more.
pub fn repo_list(repos: &[RepoLoad], count: impl Fn(&RepoLoad) -> usize) -> String {
    let mut with: Vec<(&str, usize)> = repos
        .iter()
        .map(|r| (r.repo.as_str(), count(r)))
        .filter(|(_, n)| *n > 0)
        .collect();
    with.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut named: Vec<String> = with
        .iter()
        .take(REPOS_NAMED)
        .map(|(repo, n)| {
            if *n > 1 {
                format!("{repo} {n}")
            } else {
                (*repo).to_owned()
            }
        })
        .collect();
    if with.len() > REPOS_NAMED {
        named.push(format!("{} more repositories", with.len() - REPOS_NAMED));
    }
    named.join(", ")
}

/// "45s", "14m", "2h05m".
fn duration(seconds: i64) -> String {
    match seconds {
        s if s < MINUTE => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s => format!("{}h{:02}m", s / HOUR, s % HOUR / MINUTE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runners::{RunnerState, Scope};

    fn runner(name: &str, state: RunnerState, labels: &[&str]) -> Runner {
        Runner {
            id: name.len() as u64,
            name: name.into(),
            state,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            scope: Scope::Instance,
            version: None,
        }
    }

    fn job(id: u64, repo: &str, labels: &[&str], activity: Activity) -> ActiveJob {
        ActiveJob {
            id,
            name: format!("job{id}"),
            repo: repo.into(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            activity,
            runner: None,
            queued_at: None,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-11-17T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn a_pool_names_the_repositories_holding_its_runners() {
        let runners = [
            runner("a", RunnerState::Busy, &["docker", "linux"]),
            runner("bb", RunnerState::Busy, &["docker", "linux"]),
            runner("ccc", RunnerState::Offline, &["docker", "linux"]),
        ];
        let mut queued = job(5, "acme/widgets", &["linux", "docker"], Activity::Waiting);
        queued.queued_at = Some("2026-11-17T09:46:00Z".into());
        let jobs = [
            job(1, "acme/infra", &["docker"], Activity::Running),
            job(2, "acme/infra", &["docker"], Activity::Running),
            job(3, "acme/site", &["docker"], Activity::Running),
            job(4, "acme/widgets", &["docker"], Activity::Waiting),
            queued,
        ];
        let pools = pools(&runners, &jobs, true, |_| false, now());
        assert_eq!(pools.len(), 2);
        let docker = &pools[0];
        assert_eq!(docker.labels, ["docker"]);
        assert_eq!((docker.running, docker.waiting), (3, 1));
        assert!(docker.doubled_up, "3 jobs on 2 busy runners");
        assert_eq!(
            docker.summary,
            "docker: 2 of 3 runners busy, 1 offline, 3 jobs running (acme/infra 2, acme/site), 1 waiting (acme/widgets) - 3 jobs on 2 runners, so some runners are taking several at once"
        );
        assert_eq!(
            docker.others_running("acme/widgets"),
            [&docker.repos[0], &docker.repos[1]]
        );
        // The same labels in another order are the same pool.
        let both = &pools[1];
        assert_eq!(both.labels, ["docker", "linux"]);
        assert_eq!(both.oldest_wait_seconds, Some(14 * 60));
        assert!(
            both.summary
                .ends_with("1 waiting (acme/widgets), the oldest for 14m")
        );
    }

    #[test]
    fn more_jobs_than_busy_runners_counts_only_when_every_runner_was_seen() {
        let runners = [runner("a", RunnerState::Busy, &["docker"])];
        let jobs = [
            job(1, "acme/infra", &["docker"], Activity::Running),
            job(2, "acme/site", &["docker"], Activity::Running),
        ];
        assert!(pools(&runners, &jobs, true, |_| false, now())[0].doubled_up);
        // An instance runner the credential can't list may run the second.
        assert!(!pools(&runners, &jobs, false, |_| false, now())[0].doubled_up);
    }

    #[test]
    fn named_runners_are_counted_directly() {
        let mut one = job(1, "acme/infra", &["docker"], Activity::Running);
        let mut two = job(2, "acme/site", &["docker"], Activity::Running);
        one.runner = Some("r1".into());
        two.runner = Some("r2".into());
        let jobs = [one.clone(), two.clone()];
        // Two runners, one job each, though only one runner is listed busy.
        let runners = [runner("r1", RunnerState::Busy, &["docker"])];
        assert!(!pools(&runners, &jobs, true, |_| false, now())[0].doubled_up);
        two.runner = Some("r1".into());
        let pool = &pools(&runners, &[one, two], false, |_| false, now())[0];
        assert!(pool.doubled_up);
        assert!(
            pool.summary
                .ends_with("2 jobs on 1 runner, so some runners are taking several at once")
        );
    }

    #[test]
    fn hosted_jobs_and_unknown_labels() {
        let jobs = [
            job(1, "acme/widgets", &["ubuntu-latest"], Activity::Waiting),
            job(
                2,
                "acme/widgets",
                &["self-hosted", "gpu"],
                Activity::Waiting,
            ),
        ];
        let pools = pools(
            &[],
            &jobs,
            true,
            |l| !l.contains(&"self-hosted".to_owned()),
            now(),
        );
        assert_eq!(pools.len(), 1);
        assert_eq!(
            pools[0].summary,
            "gpu, self-hosted: no visible runner carries these labels, 1 waiting (acme/widgets)"
        );
    }

    #[test]
    fn a_long_list_of_repositories_is_cut_short() {
        let repos: Vec<RepoLoad> = (1..=6)
            .map(|i| RepoLoad {
                repo: format!("acme/r{i}"),
                running: i,
                waiting: 0,
            })
            .collect();
        assert_eq!(
            repo_list(&repos, |r| r.running),
            "acme/r6 6, acme/r5 5, acme/r4 4, acme/r3 3, 2 more repositories"
        );
    }
}
