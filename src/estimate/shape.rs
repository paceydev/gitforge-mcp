//! Job sets, and the history of a workflow read through them.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use serde::Serialize;

use super::{FAIL_SAMPLES, MIN_SAMPLES, PASS_SAMPLES, Spread};
use crate::cache::now_unix;
use crate::ci::{self, Job, Run, RunsOf, Source, State};
use crate::context::Session;
use crate::forge::{ApiError, Forge};
use crate::history::{PastRun, Series, Store};

/// Pages of the run list read when a workflow's history is new.
const HISTORY_PAGES: u32 = 3;
/// How long a workflow's history is used before it's refreshed.
const HISTORY_TTL_SECONDS: u64 = 600;
/// Passing runs, newest first, whose job sets are worth reading.
const SHAPE_DEPTH: usize = 60;
/// Runs whose jobs are read in one call, so the first estimate for a
/// workflow isn't slow; the rest are read by later calls.
const JOB_READS_PER_CALL: usize = 20;
/// Job reads in flight at once.
const CONCURRENT_READS: usize = 5;

/// The jobs that ran: the names of jobs not skipped, sorted. `None` when
/// the forge listed no actual job.
pub fn job_set(jobs: &[Job]) -> Option<Vec<String>> {
    let listed: Vec<&Job> = jobs.iter().filter(|j| j.source != Source::Run).collect();
    if listed.is_empty() {
        return None;
    }
    let mut names: Vec<String> = listed
        .iter()
        .filter(|j| j.state != State::Skipped)
        .map(|j| j.name.clone())
        .collect();
    names.sort_unstable();
    names.dedup();
    Some(names)
}

/// What's known so far of an unfinished run's job set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownJobs {
    /// Jobs that have run, are running, or are queued to.
    pub ran: Vec<String>,
    /// Jobs already skipped.
    pub skipped: Vec<String>,
    /// Jobs still waiting on others, which may yet be skipped.
    pub undecided: usize,
}

/// Whether a job runs, as far as its state says yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Runs,
    Skipped,
    /// Waiting on other jobs, so it may yet be skipped.
    Undecided,
}

fn decided(state: State) -> Decision {
    match state {
        State::Skipped => Decision::Skipped,
        State::Blocked | State::Unknown => Decision::Undecided,
        _ => Decision::Runs,
    }
}

impl KnownJobs {
    pub fn of(run: &Run) -> Self {
        let listed = || run.jobs.iter().filter(|j| j.source != Source::Run);
        let named = |decision: Decision| {
            listed()
                .filter(|j| decided(j.state) == decision)
                .map(|j| j.name.clone())
                .collect()
        };
        KnownJobs {
            ran: named(Decision::Runs),
            skipped: named(Decision::Skipped),
            undecided: listed()
                .filter(|j| decided(j.state) == Decision::Undecided)
                .count(),
        }
    }

    /// Whether a past run that ran exactly `shape` agrees with this.
    pub fn allows(&self, shape: &[String]) -> bool {
        self.ran.iter().all(|j| shape.contains(j))
            && !self.skipped.iter().any(|j| shape.contains(j))
    }

    /// "the 14 jobs known so far" or "the same 14 jobs".
    fn words(&self) -> String {
        if self.undecided == 0 {
            format!("the same {} jobs", self.ran.len())
        } else {
            format!(
                "a job set agreeing with this run's so far ({} jobs decided, {} not yet)",
                self.ran.len() + self.skipped.len(),
                self.undecided
            )
        }
    }
}

/// The passing runs of `series` whose job set agrees with `known`, when
/// that rules some out and leaves enough to go on.
fn matching_passes<'a>(
    series: &'a Series,
    passes: &[&'a PastRun],
    known: &KnownJobs,
) -> Option<Vec<&'a PastRun>> {
    let (agree, disagree): (Vec<&PastRun>, Vec<&PastRun>) = passes
        .iter()
        .filter(|r| r.shape.is_some())
        .partition(|r| series.shape_of(r).is_some_and(|s| known.allows(s)));
    (!disagree.is_empty() && agree.len() >= MIN_SAMPLES).then_some(agree)
}

/// What a workflow's recent runs say.
#[derive(Debug, Clone, Serialize)]
pub struct History {
    /// Which runs: "20 passed and 6 failed push runs of ci.yml on main".
    pub basis: String,
    /// How long passing runs took, start to finish.
    pub pass: Option<Spread>,
    /// How long failed runs took to show their first failed job.
    pub fail: Option<Spread>,
    /// Whether passing runs were matched on their job set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub matched_job_set: bool,
}

impl History {
    /// From a stored series. With `known`, passing runs are those whose job
    /// set agrees with it, where enough do.
    pub fn of(series: &Series, known: Option<&KnownJobs>, of: RunsOf<'_>) -> Option<History> {
        let passes: Vec<&PastRun> = series
            .runs
            .iter()
            .filter(|r| r.passed && r.seconds.is_some())
            .collect();
        let matched = known.and_then(|k| matching_passes(series, &passes, k).map(|m| (m, k)));
        let pass: Vec<i64> = matched
            .as_ref()
            .map_or(&passes, |(m, _)| m)
            .iter()
            .filter_map(|r| r.seconds)
            .take(PASS_SAMPLES)
            .collect();
        let fail: Vec<i64> = series
            .runs
            .iter()
            .filter(|r| !r.passed)
            .filter_map(|r| r.first_failure.or(r.seconds))
            .take(FAIL_SAMPLES)
            .collect();
        let basis = match &matched {
            Some((_, k)) => format!(
                "{} passed runs with {} and {} failed {of}",
                pass.len(),
                k.words(),
                fail.len()
            ),
            None => format!("{} passed and {} failed {of}", pass.len(), fail.len()),
        };
        let (pass, fail) = (Spread::of(pass), Spread::of(fail));
        (pass.is_some() || fail.is_some()).then_some(History {
            basis,
            pass,
            fail,
            matched_job_set: matched.is_some(),
        })
    }
}

/// The stored history of `workflow` for `event` on `branch`, refreshed if
/// it's older than `HISTORY_TTL_SECONDS`, with the jobs of up to
/// `JOB_READS_PER_CALL` more runs read. Empty if it can't be read.
pub async fn series(session: &Session, of: RunsOf<'_>) -> Series {
    let store = Store::default_location();
    let repo = format!("{}/{}", session.repo.host, session.repo.full_name());
    let key = format!("{}\n{}\n{}", of.workflow, of.event, of.branch);
    let mut series = store.get(&repo, &key);
    let mut changed = false;
    if now_unix().saturating_sub(series.refreshed_unix) >= HISTORY_TTL_SECONDS {
        let after = (!series.is_new()).then_some(series.resume_after);
        match session
            .forge
            .recent_runs(&session.repo, of, HISTORY_PAGES, after)
            .await
        {
            Ok(runs) => {
                series.merge(&runs);
                changed = true;
            }
            Err(e) => tracing::debug!("{of} unreadable: {e}"),
        }
    }
    let wanted = runs_to_read(&series, JOB_READS_PER_CALL);
    changed |= read_jobs(session, &mut series, &wanted, of).await;
    if changed {
        store.put(&repo, &key, series.clone());
    }
    series
}

/// Runs whose jobs are worth reading and haven't been: the newest
/// `SHAPE_DEPTH` passing runs and `FAIL_SAMPLES` failed ones.
pub fn runs_to_read(series: &Series, limit: usize) -> Vec<u64> {
    let passes = series.runs.iter().filter(|r| r.passed).take(SHAPE_DEPTH);
    let failures = series.runs.iter().filter(|r| !r.passed).take(FAIL_SAMPLES);
    let mut wanted: Vec<u64> = passes
        .chain(failures)
        .filter(|r| !r.jobs_read)
        .map(|r| r.id)
        .collect();
    wanted.sort_unstable_by_key(|id| std::cmp::Reverse(*id));
    wanted.truncate(limit);
    wanted
}

/// Reads the jobs of runs `ids`, a few at a time, into `series`. Stops at
/// the first error other than a missing run, so an unreachable forge costs
/// one round. Returns whether anything was recorded.
pub async fn read_jobs(
    session: &Session,
    series: &mut Series,
    ids: &[u64],
    of: RunsOf<'_>,
) -> bool {
    let runs: Vec<Run> = ids
        .iter()
        .filter_map(|id| series.runs.iter().find(|r| r.id == *id))
        .filter_map(|r| r.as_run(of))
        .collect();
    let mut changed = false;
    for chunk in runs.chunks(CONCURRENT_READS) {
        let reads = chunk
            .iter()
            .map(|run| session.forge.jobs_of(&session.repo, run));
        let mut failed = false;
        for (run, jobs) in chunk.iter().zip(join_all(reads.collect()).await) {
            match jobs {
                Ok(jobs) => {
                    series.record_jobs(run.id, job_set(&jobs), first_failure(run, &jobs));
                    changed = true;
                }
                Err(ApiError::NotFound { .. }) => {
                    series.record_jobs(run.id, None, None);
                    changed = true;
                }
                Err(e) => {
                    tracing::debug!("jobs of run {} unreadable: {e}", run.id);
                    failed = true;
                }
            }
        }
        if failed {
            break;
        }
    }
    changed
}

/// Seconds from a failed run's start to its first failed job finishing.
pub fn first_failure(run: &Run, jobs: &[Job]) -> Option<i64> {
    if run.state != State::Failure {
        return None;
    }
    jobs.iter()
        .filter(|j| j.state == State::Failure)
        .filter_map(|j| ci::seconds_between(run.started_at.as_deref(), j.finished_at.as_deref()))
        .min()
}

/// Runs every future to completion, together, and returns their outputs
/// in order.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<Option<Pin<Box<F>>>> =
        futures.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut done: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        for (slot, out) in pending.iter_mut().zip(done.iter_mut()) {
            if let Some(future) = slot
                && let Poll::Ready(value) = future.as_mut().poll(cx)
            {
                *out = Some(value);
                *slot = None;
            }
        }
        if pending.iter().all(Option::is_none) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    done.into_iter().flatten().collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const PUSH: RunsOf<'static> = RunsOf {
        workflow: "ci.yml",
        event: "push",
        branch: "main",
    };

    pub(crate) fn job(name: &str, state: State) -> Job {
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

    pub(crate) fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    pub(crate) fn run(id: u64, state: State, seconds: i64) -> Run {
        Run {
            id,
            number: id,
            attempt: None,
            workflow: "ci.yml".into(),
            title: String::new(),
            event: "push".into(),
            git_ref: "main".into(),
            sha: String::new(),
            state,
            url: None,
            started_at: None,
            finished_at: None,
            duration_seconds: Some(seconds),
            jobs: Vec::new(),
            external: None,
        }
    }

    /// Nine passing runs: three each of a quick docs-only set, a full
    /// set, and a full set with deploy.
    pub(crate) fn three_shapes() -> Series {
        let mut series = Series::default();
        let runs: Vec<Run> = (1..=9)
            .rev()
            .map(|id| run(id, State::Success, [100, 600, 900][(id as usize - 1) % 3]))
            .collect();
        series.merge(&runs);
        for id in 1..=9u64 {
            let shape = match (id - 1) % 3 {
                0 => names(&["docs"]),
                1 => names(&["build", "test"]),
                _ => names(&["build", "deploy", "test"]),
            };
            series.record_jobs(id, Some(shape), None);
        }
        series
    }

    #[test]
    fn a_job_set_leaves_out_skipped_jobs_and_placeholders() {
        let mut placeholder = job("(jobs not yet listed)", State::Waiting);
        placeholder.source = Source::Run;
        let jobs = [
            job("test", State::Success),
            job("deploy", State::Skipped),
            job("build", State::Success),
            placeholder.clone(),
        ];
        assert_eq!(job_set(&jobs), Some(names(&["build", "test"])));
        assert_eq!(job_set(&[placeholder]), None);
    }

    #[test]
    fn what_is_known_of_a_running_run_narrows_its_matches() {
        let mut live = run(20, State::Running, 0);
        live.jobs = vec![
            job("build", State::Running),
            job("test", State::Blocked),
            job("deploy", State::Blocked),
        ];
        let series = three_shapes();
        // Only build has run: both full sets agree, docs-only doesn't.
        let known = KnownJobs::of(&live);
        let h = History::of(&series, Some(&known), PUSH).unwrap();
        assert!(h.matched_job_set);
        assert_eq!(h.pass.unwrap().samples, 6);
        assert!(h.basis.contains("2 not yet"), "{}", h.basis);

        // Deploy is skipped: only the full set without it agrees.
        live.jobs[2].state = State::Skipped;
        live.jobs[1].state = State::Waiting;
        let known = KnownJobs::of(&live);
        let h = History::of(&series, Some(&known), PUSH).unwrap();
        assert_eq!(h.pass.unwrap().median_seconds, 600);
        assert!(h.basis.starts_with("3 passed runs with the same 2 jobs"));
    }

    #[test]
    fn too_few_matches_fall_back_to_the_whole_workflow() {
        let mut live = run(20, State::Running, 0);
        live.jobs = vec![job("lint", State::Running)];
        let h = History::of(&three_shapes(), Some(&KnownJobs::of(&live)), PUSH).unwrap();
        assert!(!h.matched_job_set);
        assert_eq!(h.pass.unwrap().samples, 9);
    }

    #[test]
    fn only_unread_recent_runs_are_read() {
        let mut series = three_shapes();
        assert!(runs_to_read(&series, 10).is_empty());
        series.merge(&[run(11, State::Failure, 50), run(10, State::Success, 600)]);
        assert_eq!(runs_to_read(&series, 10), [11, 10]);
        assert_eq!(runs_to_read(&series, 1), [11]);
    }

    #[test]
    fn first_failure_is_the_earliest_failed_job() {
        let mut failed = run(1, State::Failure, 900);
        failed.started_at = Some("2026-11-09T10:00:00Z".into());
        let mut a = job("a", State::Failure);
        a.finished_at = Some("2026-11-09T10:05:00Z".into());
        let mut b = job("b", State::Failure);
        b.finished_at = Some("2026-11-09T10:03:00Z".into());
        assert_eq!(first_failure(&failed, &[a.clone(), b]), Some(180));
        assert_eq!(first_failure(&run(2, State::Success, 9), &[a]), None);
    }

    #[test]
    fn joins_futures_in_order() {
        let out = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(join_all(vec![
                Box::pin(async { 1 }) as Pin<Box<dyn Future<Output = i32>>>,
                Box::pin(async {
                    tokio::task::yield_now().await;
                    2
                }),
            ]));
        assert_eq!(out, [1, 2]);
    }
}
