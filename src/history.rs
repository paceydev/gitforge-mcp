//! Past CI runs kept on disk, so estimates survive a restart and a refresh
//! reads only the runs that are new since the last one.
//!
//! One JSON file per repository, beside the probe cache (`cache.rs`): a
//! series of finished runs for each workflow, event and branch. A run's job
//! set is stored once per distinct set and referred to by index, so a
//! series of 150 runs with three shapes stays a few kilobytes. Finished runs
//! don't change, so what was read about one (its job set, when its first
//! failure showed) is read once.
//!
//! Like the probe cache, it is best effort: a file that can't be read is an
//! empty history, and one that can't be written costs the next refresh.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cache::{CACHE_ENV, now_unix};
use crate::ci::{Run, RunsOf, State};

/// Finished runs kept per series, newest first.
pub const MAX_RUNS: usize = 150;
/// Series kept per repository; the least recently refreshed go first.
const MAX_SERIES: usize = 30;

/// One finished run, as much as estimates need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PastRun {
    pub id: u64,
    pub passed: bool,
    /// Start to finish, where the forge gave both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i64>,
    /// Whether its jobs have been read. Until they are, `shape` and
    /// `first_failure` say nothing.
    #[serde(default)]
    pub jobs_read: bool,
    /// Index into `Series::shapes`: the jobs that ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<usize>,
    /// A failed run's seconds from start to its first failed job finishing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<i64>,
    /// What reading its jobs takes (Forgejo before 16 finds them by commit
    /// and run number), kept only until they are read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// What a run's jobs are read with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub number: u64,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

impl PastRun {
    /// A finished run that passed or failed; `None` for anything else
    /// (cancelled, skipped, unfinished), which says nothing about duration.
    pub fn of(run: &Run) -> Option<PastRun> {
        let passed = match run.state {
            State::Success => true,
            State::Failure => false,
            _ => return None,
        };
        Some(PastRun {
            id: run.id,
            passed,
            seconds: run.duration_seconds,
            jobs_read: false,
            shape: None,
            first_failure: None,
            pending: Some(Pending {
                number: run.number,
                sha: run.sha.clone(),
                started_at: run.started_at.clone(),
            }),
        })
    }

    /// The run again, as far as reading its jobs needs, or `None` once
    /// they've been read.
    pub fn as_run(&self, of: RunsOf<'_>) -> Option<Run> {
        let pending = self.pending.as_ref()?;
        Some(Run {
            id: self.id,
            number: pending.number,
            attempt: None,
            workflow: of.workflow.to_owned(),
            title: String::new(),
            event: of.event.to_owned(),
            git_ref: of.branch.to_owned(),
            sha: pending.sha.clone(),
            state: if self.passed {
                State::Success
            } else {
                State::Failure
            },
            url: None,
            started_at: pending.started_at.clone(),
            finished_at: None,
            duration_seconds: self.seconds,
            jobs: Vec::new(),
            external: None,
        })
    }
}

/// The finished runs of one workflow, for one event, on one branch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Series {
    pub refreshed_unix: u64,
    /// The next refresh reads back to here: runs above it were new or
    /// unfinished at the last one.
    pub resume_after: u64,
    /// Each distinct job set seen, as sorted job names.
    #[serde(default)]
    pub shapes: Vec<Vec<String>>,
    /// Newest first.
    #[serde(default)]
    pub runs: Vec<PastRun>,
}

impl Series {
    /// Whether nothing has been read yet, so a refresh reads every page.
    pub fn is_new(&self) -> bool {
        self.refreshed_unix == 0
    }

    /// Adds what a refresh read (newest first, finished or not), keeping
    /// the newest `MAX_RUNS`. A run already kept stays as it is, with what
    /// was read about its jobs, unless it changed (a rerun).
    pub fn merge(&mut self, fetched: &[Run]) {
        self.merge_keeping(fetched, MAX_RUNS);
    }

    /// Like `merge`, keeping every run: for a backtest, never stored.
    pub fn merge_all(&mut self, fetched: &[Run]) {
        self.merge_keeping(fetched, usize::MAX);
    }

    fn merge_keeping(&mut self, fetched: &[Run], limit: usize) {
        for run in fetched {
            let Some(past) = PastRun::of(run) else {
                continue;
            };
            match self.runs.iter_mut().find(|r| r.id == past.id) {
                Some(kept) if kept.passed == past.passed && kept.seconds == past.seconds => {}
                Some(kept) => *kept = past,
                None => self.runs.push(past),
            }
        }
        self.runs.sort_unstable_by_key(|r| std::cmp::Reverse(r.id));
        self.runs.truncate(limit);
        self.resume_after = resume_point(fetched, self.resume_after);
        self.refreshed_unix = now_unix();
        self.compact_shapes();
    }

    /// Records the jobs that ran in run `id`, and when its first failure
    /// showed.
    pub fn record_jobs(&mut self, id: u64, shape: Option<Vec<String>>, first_failure: Option<i64>) {
        let index = shape.map(|s| self.shape_index(s));
        if let Some(run) = self.runs.iter_mut().find(|r| r.id == id) {
            run.jobs_read = true;
            run.shape = index;
            run.first_failure = first_failure;
            run.pending = None;
        }
    }

    pub fn shape_of(&self, run: &PastRun) -> Option<&[String]> {
        run.shape
            .and_then(|i| self.shapes.get(i))
            .map(Vec::as_slice)
    }

    fn shape_index(&mut self, shape: Vec<String>) -> usize {
        if let Some(i) = self.shapes.iter().position(|s| *s == shape) {
            return i;
        }
        self.shapes.push(shape);
        self.shapes.len() - 1
    }

    /// Drops job sets no kept run refers to, renumbering the rest.
    fn compact_shapes(&mut self) {
        let mut used: Vec<usize> = self.runs.iter().filter_map(|r| r.shape).collect();
        used.sort_unstable();
        used.dedup();
        let shapes = std::mem::take(&mut self.shapes);
        self.shapes = used
            .iter()
            .filter_map(|&i| shapes.get(i).cloned())
            .collect();
        for run in &mut self.runs {
            run.shape = run.shape.and_then(|i| used.binary_search(&i).ok());
        }
    }
}

/// Where the next refresh must read back to: just below the oldest run
/// still unfinished, or else the newest run seen.
fn resume_point(fetched: &[Run], before: u64) -> u64 {
    let unfinished = fetched
        .iter()
        .filter(|r| !r.state.is_final())
        .map(|r| r.id)
        .min();
    match unfinished {
        Some(id) => id.saturating_sub(1),
        None => fetched.iter().map(|r| r.id).max().unwrap_or(0).max(before),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoFile {
    #[serde(default)]
    series: BTreeMap<String, Series>,
}

/// The history files of every repository.
pub struct Store {
    dir: Option<PathBuf>,
}

impl Store {
    /// `history/` beside the probe cache, which `GITFORGE_MCP_CACHE` moves.
    pub fn default_location() -> Self {
        let dir = std::env::var_os(CACHE_ENV)
            .map(PathBuf::from)
            .map(|p| p.parent().map(|d| d.join("history")).unwrap_or_default())
            .or_else(|| dirs::cache_dir().map(|d| d.join("gitforge-mcp").join("history")));
        Self { dir }
    }

    #[cfg(test)]
    pub fn at(dir: &std::path::Path) -> Self {
        Self {
            dir: Some(dir.to_owned()),
        }
    }

    /// The directory the history files are kept in.
    pub fn dir(&self) -> Option<&std::path::Path> {
        self.dir.as_deref()
    }

    /// The series named `key` in repository `repo` (host/owner/name).
    pub fn get(&self, repo: &str, key: &str) -> Series {
        self.read(repo).series.remove(key).unwrap_or_default()
    }

    /// Best effort, like the probe cache.
    pub fn put(&self, repo: &str, key: &str, series: Series) {
        let Some(path) = self.path(repo) else { return };
        let mut file = self.read(repo);
        file.series.insert(key.to_owned(), series);
        while file.series.len() > MAX_SERIES {
            let Some(oldest) = file
                .series
                .iter()
                .min_by_key(|(_, s)| s.refreshed_unix)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            file.series.remove(&oldest);
        }
        let Ok(text) = serde_json::to_string(&file) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Another process may write the same file: each writes its own
        // temporary file and the last rename wins.
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn read(&self, repo: &str) -> RepoFile {
        self.path(repo)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn path(&self, repo: &str) -> Option<PathBuf> {
        let name: String = repo
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.as_ref().map(|d| d.join(format!("{name}.json")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::Run;

    const OF: RunsOf<'static> = RunsOf {
        workflow: "ci.yml",
        event: "push",
        branch: "main",
    };

    fn run(id: u64, state: State, seconds: Option<i64>) -> Run {
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
            duration_seconds: seconds,
            jobs: Vec::new(),
            external: None,
        }
    }

    #[test]
    fn merging_keeps_what_was_read_and_resumes_below_unfinished_runs() {
        let mut series = Series::default();
        assert!(series.is_new());
        series.merge(&[
            run(12, State::Running, None),
            run(11, State::Success, Some(60)),
            run(10, State::Cancelled, Some(5)),
            run(9, State::Failure, Some(30)),
        ]);
        assert_eq!(
            series.runs.iter().map(|r| r.id).collect::<Vec<_>>(),
            [11, 9]
        );
        assert_eq!(series.resume_after, 11);
        series.record_jobs(11, Some(vec!["build".into(), "test".into()]), None);

        // The next refresh sees 12 finished, and 11 unchanged.
        series.merge(&[
            run(13, State::Success, Some(70)),
            run(12, State::Success, Some(65)),
            run(11, State::Success, Some(60)),
        ]);
        assert_eq!(series.resume_after, 13);
        let kept = series.runs.iter().find(|r| r.id == 11).unwrap();
        assert!(kept.jobs_read && kept.pending.is_none());
        assert!(kept.as_run(OF).is_none());
        assert_eq!(series.shape_of(kept).unwrap(), ["build", "test"]);
        let fresh = series.runs.iter().find(|r| r.id == 13).unwrap();
        assert_eq!(fresh.as_run(OF).unwrap().number, 13);
    }

    #[test]
    fn a_rerun_replaces_the_kept_run_and_unused_job_sets_go() {
        let mut series = Series::default();
        series.merge(&[run(5, State::Failure, Some(30))]);
        series.record_jobs(5, Some(vec!["a".into()]), Some(20));
        series.merge(&[run(5, State::Success, Some(90))]);
        let kept = &series.runs[0];
        assert!(kept.passed && !kept.jobs_read && kept.shape.is_none());
        assert!(series.shapes.is_empty());
    }

    #[test]
    fn keeps_at_most_max_runs() {
        let mut series = Series::default();
        let many: Vec<Run> = (1..=(MAX_RUNS as u64 + 20))
            .rev()
            .map(|id| run(id, State::Success, Some(10)))
            .collect();
        series.merge(&many);
        assert_eq!(series.runs.len(), MAX_RUNS);
        assert_eq!(series.runs[0].id, MAX_RUNS as u64 + 20);
    }

    #[test]
    fn round_trips_through_a_file_and_survives_a_corrupt_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let repo = "git.example.com/acme/widgets";
        assert!(store.get(repo, "k").is_new());
        std::fs::write(store.path(repo).unwrap(), "not json").unwrap();
        let mut series = Series::default();
        series.merge(&[run(3, State::Success, Some(42))]);
        series.record_jobs(3, Some(vec!["build".into()]), None);
        store.put(repo, "k", series.clone());
        assert_eq!(store.get(repo, "k"), series);
    }
}
