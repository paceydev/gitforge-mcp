//! How long CI usually takes, from a workflow's recent runs on the same
//! branch and event: when a running run will likely finish if it passes,
//! and by when a failure usually shows.
//!
//! "When a failure shows" is the time from the run's start to its first
//! failed job finishing, which is when a fail-fast wait returns. A run
//! that's past the point where nine in ten past failures had shown is
//! likely to pass.
//!
//! Passing runs are matched on their job set: the jobs that ran, skipped
//! ones left out. Path filters and `if:` conditions make one workflow run
//! very different sets of jobs, and mixing them was most of the error. A
//! run still going has a job set only partly known: a job waiting on others
//! may yet be skipped. So it's matched with the past runs whose job set
//! agrees with what is known so far (every job that has run or will run is
//! in it, and no job already skipped), which is the whole workflow at first
//! and narrows as jobs are decided. Where fewer than three past runs agree,
//! the whole workflow is used. Failures aren't matched: a failure skips the
//! jobs after it, so a failed run's job set says little.
//!
//! History is kept on disk (`history.rs`) and refreshed with only the runs
//! that are new since the last read.

mod backtest;
mod outlook;
mod queue;
mod shape;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;

use crate::ci::{self, Run};

pub use crate::history::Series;
pub use backtest::{Backtest, backtest, backtest_by_job_set, backtest_series};
pub use outlook::{JobSetSpread, Outlook};
pub use queue::{Contention, QueueWait, contention, queue_waits, runner_report};
pub use shape::{History, KnownJobs, first_failure, job_set, read_jobs, runs_to_read, series};

/// Passing runs an estimate draws on, newest first.
pub const PASS_SAMPLES: usize = 20;
/// Failed runs an estimate draws on.
pub const FAIL_SAMPLES: usize = 10;
/// Fewer samples than this make no estimate.
const MIN_SAMPLES: usize = 3;

/// The middle and the slow end of some durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Spread {
    pub samples: usize,
    pub median_seconds: i64,
    pub p90_seconds: i64,
}

impl Spread {
    /// `None` with fewer than `MIN_SAMPLES` durations.
    pub fn of(mut seconds: Vec<i64>) -> Option<Self> {
        if seconds.len() < MIN_SAMPLES {
            return None;
        }
        seconds.sort_unstable();
        Some(Spread {
            samples: seconds.len(),
            median_seconds: percentile(&seconds, MEDIAN),
            p90_seconds: percentile(&seconds, P90),
        })
    }

    /// Whether the slow end is far enough from the middle that a single
    /// time would mislead.
    fn is_wide(&self) -> bool {
        self.p90_seconds > 2 * self.median_seconds.max(1)
    }

    /// "12m (18m if slow)".
    fn words(&self) -> String {
        format!(
            "{} ({} if slow)",
            minutes(self.median_seconds),
            minutes(self.p90_seconds)
        )
    }
}

const MEDIAN: f64 = 0.5;
const P90: f64 = 0.9;
const PERCENT: i64 = 100;
const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[i64], p: f64) -> i64 {
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// When an unfinished run will likely end, from its history, and what its
/// waiting jobs are waiting for.
#[derive(Debug, Clone, Serialize)]
pub struct Eta {
    pub run: u64,
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// Seconds since the run started (0 if it hasn't).
    pub elapsed_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass: Option<Forecast>,
    /// Absent once a job has failed: the failure has already shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail: Option<Forecast>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub matched_job_set: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub queue: Vec<QueueWait>,
    /// For a run longer than nine in ten of its passes: what its runners
    /// are busy with besides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contention: Option<Contention>,
    /// The forecast in words, as the progress line gives it.
    pub summary: String,
}

/// A spread of durations, and the clock times it puts the end at.
#[derive(Debug, Clone, Serialize)]
pub struct Forecast {
    #[serde(flatten)]
    pub spread: Spread,
    /// Start plus the median, in local time.
    pub likely_at: String,
    /// Start plus the 90th percentile, in local time.
    pub latest_at: String,
}

impl Eta {
    /// The forecast for `run` at `now`, or `None` if it has finished or
    /// there's neither history nor a queue to speak of.
    pub fn of(
        run: &Run,
        history: Option<&History>,
        queue: Vec<QueueWait>,
        contention: Option<Contention>,
        now: DateTime<Utc>,
    ) -> Option<Eta> {
        if (run.state.is_final() && run.jobs.iter().all(|j| j.state.is_final()))
            || (history.is_none() && queue.is_empty())
        {
            return None;
        }
        let started = started(run, now);
        let elapsed = (now - started).num_seconds();
        let forecast = |spread: Spread| Forecast {
            spread,
            likely_at: clock(started, spread.median_seconds).to_rfc3339(),
            latest_at: clock(started, spread.p90_seconds).to_rfc3339(),
        };
        let failing = run.jobs.iter().any(|j| j.state.is_failed());
        let pass = history.and_then(|h| h.pass);
        let fail = history.and_then(|h| h.fail).filter(|_| !failing);
        let mut summary = describe(elapsed, started, pass, fail);
        for wait in &queue {
            summary.push_str("; ");
            summary.push_str(&wait.summary);
        }
        if let Some(c) = &contention {
            summary.push_str("; ");
            summary.push_str(&c.summary);
        }
        Some(Eta {
            run: run.number,
            workflow: run.workflow.clone(),
            basis: history.map(|h| h.basis.clone()),
            elapsed_seconds: elapsed,
            pass: pass.map(forecast),
            fail: fail.map(forecast),
            matched_job_set: history.is_some_and(|h| h.matched_job_set),
            queue,
            contention,
            summary,
        })
    }
}

/// When `run` started, or `now` if it hasn't.
fn started(run: &Run, now: DateTime<Utc>) -> DateTime<Utc> {
    run.started_at
        .as_deref()
        .and_then(ci::parse_time)
        .map(|t| t.with_timezone(&Utc))
        .filter(|t| *t <= now)
        .unwrap_or(now)
}

/// Whether `run` is still running and has gone on longer than nine in ten
/// of its passes did: worth asking what else its runners are doing.
pub fn overdue(run: &Run, history: Option<&History>, now: DateTime<Utc>) -> bool {
    let Some(pass) = history.and_then(|h| h.pass) else {
        return false;
    };
    run.jobs.iter().any(|j| j.state == ci::State::Running)
        && (now - started(run, now)).num_seconds() > pass.p90_seconds
}

fn clock(started: DateTime<Utc>, seconds: i64) -> DateTime<Local> {
    (started + chrono::Duration::seconds(seconds)).with_timezone(&Local)
}

fn hhmm(started: DateTime<Utc>, seconds: i64) -> String {
    clock(started, seconds).format("%H:%M").to_string()
}

/// "12m in; a pass usually takes 24m (31m if slow): around 10:42, or
/// 10:51 if slow; a failure usually shows by 10:35 (10:43 if slow)".
fn describe(
    elapsed: i64,
    started: DateTime<Utc>,
    pass: Option<Spread>,
    fail: Option<Spread>,
) -> String {
    let mut parts = vec![elapsed_words(elapsed)];
    if let Some(p) = pass {
        if elapsed <= p.p90_seconds {
            parts.push(format!(
                "a pass usually takes {}: around {}, or {} if slow",
                p.words(),
                hhmm(started, p.median_seconds),
                hhmm(started, p.p90_seconds)
            ));
        } else {
            parts.push(format!(
                "already longer than 9 in 10 passing runs ({})",
                minutes(p.p90_seconds)
            ));
        }
    }
    if let Some(f) = fail {
        if elapsed <= f.p90_seconds {
            parts.push(format!(
                "a failure usually shows by {} ({} if slow)",
                hhmm(started, f.median_seconds),
                hhmm(started, f.p90_seconds)
            ));
        } else {
            parts.push(format!(
                "no failure yet, and 9 in 10 failures had shown by {}: likely to pass",
                minutes(f.p90_seconds)
            ));
        }
    }
    if pass.is_some_and(|p| p.is_wide()) {
        parts.push("runs vary a lot, so treat this as rough".into());
    }
    parts.join("; ")
}

/// How long a run has gone, in whole minutes: "under 1m in", "12m in". The
/// forecast changes only once a minute, so a reader that prints it only
/// when it changes isn't flooded.
fn elapsed_words(seconds: i64) -> String {
    match seconds {
        s if s < MINUTE => "under 1m in".into(),
        s if s < HOUR => format!("{}m in", s / MINUTE),
        s => format!("{}h{:02}m in", s / HOUR, s % HOUR / MINUTE),
    }
}

/// "45s", "12m", "1h05m".
fn minutes(seconds: i64) -> String {
    if seconds < MINUTE {
        return format!("{seconds}s");
    }
    // Rounded to the minute before splitting, so 1h59m40s is 2h00m.
    let rounded = (seconds + MINUTE / 2) / MINUTE;
    let per_hour = HOUR / MINUTE;
    match rounded {
        m if m < per_hour => format!("{m}m"),
        m => format!("{}h{:02}m", m / per_hour, m % per_hour),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_need_three_samples() {
        assert_eq!(Spread::of(vec![10, 20]), None);
        let s = Spread::of(vec![50, 10, 30, 20, 40, 60, 70, 80, 90, 100]).unwrap();
        assert_eq!((s.samples, s.median_seconds, s.p90_seconds), (10, 50, 90));
    }

    #[test]
    fn describes_a_run_still_in_the_window_where_failures_show() {
        let started = DateTime::parse_from_rfc3339("2026-11-09T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let pass = Spread {
            samples: 20,
            median_seconds: 1440,
            p90_seconds: 1860,
        };
        let fail = Spread {
            samples: 6,
            median_seconds: 300,
            p90_seconds: 900,
        };
        let text = describe(720, started, Some(pass), Some(fail));
        assert!(
            text.starts_with("12m in; a pass usually takes 24m (31m if slow)"),
            "{text}"
        );
        assert!(text.contains("a failure usually shows by"), "{text}");
        let text = describe(1000, started, Some(pass), Some(fail));
        assert!(
            text.contains("9 in 10 failures had shown by 15m: likely to pass"),
            "{text}"
        );
        let text = describe(2000, started, Some(pass), None);
        assert!(
            text.contains("already longer than 9 in 10 passing runs (31m)"),
            "{text}"
        );
    }

    #[test]
    fn warns_when_runs_vary_a_lot() {
        let started = Utc::now();
        let wide = Spread {
            samples: 10,
            median_seconds: 300,
            p90_seconds: 1500,
        };
        assert!(describe(0, started, Some(wide), None).ends_with("treat this as rough"));
    }

    #[test]
    fn minutes_read_naturally() {
        assert_eq!(minutes(45), "45s");
        assert_eq!(minutes(749), "12m");
        assert_eq!(minutes(3900), "1h05m");
        assert_eq!(minutes(3580), "1h00m");
        assert_eq!(minutes(7179), "2h00m");
    }

    #[test]
    fn elapsed_time_moves_a_minute_at_a_time() {
        assert_eq!(elapsed_words(6), "under 1m in");
        assert_eq!(elapsed_words(59), "under 1m in");
        assert_eq!(elapsed_words(119), "1m in");
        assert_eq!(elapsed_words(3725), "1h02m in");
    }
}
