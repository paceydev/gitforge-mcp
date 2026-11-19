//! How long a workflow takes before anything has run, for `ci_estimate`.

use std::collections::HashMap;

use serde::Serialize;

use super::{History, MIN_SAMPLES, PASS_SAMPLES, Series, Spread};
use crate::ci::RunsOf;

/// Job sets an outlook lists.
const OUTLOOK_JOB_SETS: usize = 4;
/// Job names listed where two job sets differ.
const DIFF_NAMES: usize = 4;

/// How long a workflow takes before anything has run: for `ci_estimate`.
#[derive(Debug, Clone, Serialize)]
pub struct Outlook {
    pub workflow: String,
    pub basis: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass: Option<Spread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail: Option<Spread>,
    /// The commonest job sets among passing runs, each with its spread.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub job_sets: Vec<JobSetSpread>,
    pub summary: String,
}

/// How long passing runs with one job set took.
#[derive(Debug, Clone, Serialize)]
pub struct JobSetSpread {
    /// Jobs that ran.
    pub jobs: usize,
    /// Passing runs with this job set, of those whose job set is known.
    pub runs: usize,
    #[serde(flatten)]
    pub spread: Spread,
    /// How it differs from the commonest set: "+deploy", "-docs".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub differs_by: Vec<String>,
}

impl Outlook {
    /// `None` when the series has too little to go on.
    pub fn of(series: &Series, of: RunsOf<'_>) -> Option<Outlook> {
        let history = History::of(series, None, of)?;
        let job_sets = job_set_spreads(series);
        let mut parts = Vec::new();
        if let Some(p) = history.pass {
            parts.push(format!("a pass usually takes {}", p.words()));
        }
        if let Some(f) = history.fail {
            parts.push(format!("a failure usually shows after {}", f.words()));
        }
        if job_sets.len() > 1 {
            let each: Vec<String> = job_sets
                .iter()
                .map(|s| {
                    let jobs = if s.jobs == 1 { "job" } else { "jobs" };
                    format!("{} {jobs} ({} runs): {}", s.jobs, s.runs, s.spread.words())
                })
                .collect();
            parts.push(format!("by job set: {}", each.join(", ")));
        }
        Some(Outlook {
            workflow: of.workflow.to_owned(),
            basis: history.basis,
            pass: history.pass,
            fail: history.fail,
            job_sets,
            summary: parts.join("; "),
        })
    }
}

/// The commonest job sets among passing runs with `MIN_SAMPLES` runs or
/// more, commonest first.
fn job_set_spreads(series: &Series) -> Vec<JobSetSpread> {
    let mut by_shape: HashMap<usize, Vec<i64>> = HashMap::new();
    for run in series.runs.iter().filter(|r| r.passed) {
        if let (Some(shape), Some(seconds)) = (run.shape, run.seconds) {
            by_shape.entry(shape).or_default().push(seconds);
        }
    }
    let mut ranked: Vec<(usize, Vec<i64>)> = by_shape
        .into_iter()
        .filter(|(_, s)| s.len() >= MIN_SAMPLES)
        .collect();
    ranked.sort_by_key(|(shape, s)| (std::cmp::Reverse(s.len()), *shape));
    ranked.truncate(OUTLOOK_JOB_SETS);
    let common = ranked.first().map(|(i, _)| series.shapes[*i].clone());
    ranked
        .into_iter()
        .filter_map(|(shape, seconds)| {
            let jobs = &series.shapes[shape];
            Some(JobSetSpread {
                jobs: jobs.len(),
                runs: seconds.len(),
                spread: Spread::of(seconds.into_iter().take(PASS_SAMPLES).collect())?,
                differs_by: common.as_deref().map(|c| diff(c, jobs)).unwrap_or_default(),
            })
        })
        .collect()
}

/// "+name" for each job in `other` but not `base`, "-name" for the reverse,
/// at most `DIFF_NAMES` of them and then a count.
fn diff(base: &[String], other: &[String]) -> Vec<String> {
    let added = other
        .iter()
        .filter(|j| !base.contains(j))
        .map(|j| format!("+{j}"));
    let removed = base
        .iter()
        .filter(|j| !other.contains(j))
        .map(|j| format!("-{j}"));
    let all: Vec<String> = added.chain(removed).collect();
    if all.len() <= DIFF_NAMES {
        return all;
    }
    let mut shown = all[..DIFF_NAMES].to_vec();
    shown.push(format!("and {} more", all.len() - DIFF_NAMES));
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::shape::tests::{PUSH, names, three_shapes};

    #[test]
    fn an_outlook_lists_each_common_job_set() {
        let outlook = Outlook::of(&three_shapes(), PUSH).unwrap();
        assert_eq!(outlook.job_sets.len(), 3);
        assert!(
            outlook.summary.contains("by job set:"),
            "{}",
            outlook.summary
        );
        let deploy = outlook
            .job_sets
            .iter()
            .find(|s| s.spread.median_seconds == 900)
            .unwrap();
        assert_eq!(deploy.jobs, 3);
    }

    #[test]
    fn diffs_name_a_few_jobs_then_count() {
        let base = names(&["a", "b"]);
        assert_eq!(diff(&base, &names(&["a", "c"])), ["+c", "-b"]);
        let many = names(&["c", "d", "e", "f", "g"]);
        let d = diff(&base, &many);
        assert_eq!(d.len(), DIFF_NAMES + 1);
        assert_eq!(d.last().unwrap(), "and 3 more");
    }
}
