//! How well the estimates would have done on past runs.

use serde::Serialize;
use serde_json::{Value, json};

use super::{FAIL_SAMPLES, MEDIAN, PASS_SAMPLES, PERCENT, Series, Spread, percentile};

/// The backtest of every run in `series`: passing runs from the whole
/// workflow and by job set, failed runs' time to first failure from the
/// whole workflow.
pub fn backtest_series(series: &Series) -> Value {
    let oldest_first = series.runs.iter().rev();
    let passes: Vec<(i64, Option<usize>)> = oldest_first
        .clone()
        .filter(|r| r.passed)
        .filter_map(|r| Some((r.seconds?, r.shape)))
        .collect();
    let failures: Vec<i64> = oldest_first
        .filter(|r| !r.passed)
        .filter_map(|r| r.first_failure.or(r.seconds))
        .collect();
    let durations: Vec<i64> = passes.iter().map(|p| p.0).collect();
    let mut shapes: Vec<usize> = passes.iter().filter_map(|p| p.1).collect();
    shapes.sort_unstable();
    shapes.dedup();
    json!({
        "pass": {
            "runs": passes.len(),
            "with_job_set": passes.iter().filter(|p| p.1.is_some()).count(),
            "job_sets": shapes.len(),
            "durations": Spread::of(durations.clone()),
            "backtest": backtest(&durations, PASS_SAMPLES),
            "backtest_by_job_set": backtest_by_job_set(&passes, PASS_SAMPLES),
        },
        "fail": {
            "runs": failures.len(),
            "time_to_first_failure": Spread::of(failures.clone()),
            "backtest": backtest(&failures, FAIL_SAMPLES),
        },
    })
}

/// How well the median and 90th percentile of the runs before each run
/// predicted it.
#[derive(Debug, Clone, Serialize)]
pub struct Backtest {
    /// Runs predicted (each needs `MIN_SAMPLES` earlier ones).
    pub predicted: usize,
    /// The typical miss, in seconds: the median of |actual - median|.
    pub median_error_seconds: i64,
    /// The typical miss as a share of the actual duration, in percent.
    pub median_error_percent: i64,
    /// Share of runs that finished by the predicted 90th percentile, in
    /// percent. Near 90 means the "if slow" time is honest.
    pub within_p90_percent: i64,
    /// Share of predictions made from runs with the same job set, in
    /// percent; the rest used the whole workflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_job_set_percent: Option<i64>,
}

/// Predicts each of `durations` (oldest first) from up to `window` before
/// it.
pub fn backtest(durations: &[i64], window: usize) -> Option<Backtest> {
    let runs: Vec<(i64, Option<usize>)> = durations.iter().map(|&d| (d, None)).collect();
    let mut result = backtest_by_job_set(&runs, window)?;
    result.by_job_set_percent = None;
    Some(result)
}

/// Like `backtest`, but each run is predicted from up to `window` earlier
/// runs with the same job set (the second of each pair), or from the
/// whole workflow where fewer than `MIN_SAMPLES` match: the estimate once
/// a run's job set is known.
pub fn backtest_by_job_set(runs: &[(i64, Option<usize>)], window: usize) -> Option<Backtest> {
    let mut errors = Vec::new();
    let mut percents = Vec::new();
    let (mut within, mut matched) = (0, 0);
    for (i, &(actual, shape)) in runs.iter().enumerate() {
        let same: Vec<i64> = runs[..i]
            .iter()
            .rev()
            .filter(|(_, s)| shape.is_some() && *s == shape)
            .map(|(d, _)| *d)
            .take(window)
            .collect();
        let spread = match Spread::of(same) {
            Some(spread) => {
                matched += 1;
                spread
            }
            None => {
                let Some(spread) = Spread::of(
                    runs[i.saturating_sub(window)..i]
                        .iter()
                        .map(|(d, _)| *d)
                        .collect(),
                ) else {
                    continue;
                };
                spread
            }
        };
        let error = (actual - spread.median_seconds).abs();
        errors.push(error);
        percents.push(error * PERCENT / actual.max(1));
        if actual <= spread.p90_seconds {
            within += 1;
        }
    }
    if errors.is_empty() {
        return None;
    }
    let predicted = errors.len();
    errors.sort_unstable();
    percents.sort_unstable();
    Some(Backtest {
        predicted,
        median_error_seconds: percentile(&errors, MEDIAN),
        median_error_percent: percentile(&percents, MEDIAN),
        within_p90_percent: within as i64 * PERCENT / predicted as i64,
        by_job_set_percent: Some(matched as i64 * PERCENT / predicted as i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtests_each_run_against_the_ones_before() {
        // Steady around 100s, with one slow run.
        let durations = [100, 102, 98, 101, 99, 250, 100, 103];
        let b = backtest(&durations, 20).unwrap();
        assert_eq!(b.predicted, 5);
        assert!(b.median_error_seconds <= 3, "{b:?}");
        assert_eq!(b.within_p90_percent, 80);
        assert!(b.by_job_set_percent.is_none());
    }

    #[test]
    fn backtesting_by_job_set_separates_two_shapes() {
        // Alternating quick and slow shapes: the whole workflow's median
        // misses both, the job set's hits each.
        let runs: Vec<(i64, Option<usize>)> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    (100, Some(0))
                } else {
                    (600, Some(1))
                }
            })
            .collect();
        let durations: Vec<i64> = runs.iter().map(|r| r.0).collect();
        let whole = backtest(&durations, 20).unwrap();
        let by_set = backtest_by_job_set(&runs, 20).unwrap();
        assert!(whole.median_error_seconds >= 250, "{whole:?}");
        assert_eq!(by_set.median_error_seconds, 0, "{by_set:?}");
        assert!(by_set.by_job_set_percent.unwrap() > 50);
    }
}
