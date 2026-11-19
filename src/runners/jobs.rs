//! The jobs waiting and running in each scope a forge lists them for: the
//! raw material pools are made from.

use serde::Serialize;

use super::Scope;

/// What a job in the queue is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Waiting,
    Running,
}

/// A job waiting for a runner or running on one, in any repository a scope
/// listed.
#[derive(Debug, Clone)]
pub struct ActiveJob {
    /// The forge's id for the job, to count a job listed by two scopes once.
    pub id: u64,
    pub name: String,
    /// `owner/name`, or `repository #<id>` where the credential can't read
    /// it.
    pub repo: String,
    /// The labels it asked for.
    pub labels: Vec<String>,
    pub activity: Activity,
    /// The runner it runs on, where the forge says (Gitea and GitHub).
    pub runner: Option<String>,
    /// When it was queued, where the forge says.
    pub queued_at: Option<String>,
}

/// What one scope's job listing said.
#[derive(Debug, Clone, Serialize)]
pub struct JobScopeOutcome {
    pub scope: Scope,
    /// The endpoint asked.
    pub endpoint: String,
    /// `listed`, `not_permitted` (and why) or `unavailable`.
    pub outcome: String,
    pub jobs: usize,
}

/// One scope's jobs, as asked: listed, or why not.
#[derive(Debug, Clone)]
pub struct ScopeJobs<T = ActiveJob> {
    pub scope: Scope,
    pub endpoint: String,
    pub outcome: String,
    pub jobs: Vec<T>,
}

impl<T> ScopeJobs<T> {
    /// A scope that was listed.
    pub fn listed(scope: Scope, endpoint: String, jobs: Vec<T>) -> Self {
        ScopeJobs {
            scope,
            endpoint,
            outcome: "listed".into(),
            jobs,
        }
    }

    /// A scope that wasn't asked or didn't answer, and why.
    pub fn not_listed(scope: Scope, endpoint: String, outcome: String) -> Self {
        ScopeJobs {
            scope,
            endpoint,
            outcome,
            jobs: Vec::new(),
        }
    }
}

/// The jobs waiting and running in each scope the forge lists them for.
#[derive(Debug, Clone, Default)]
pub struct ActiveJobs {
    pub jobs: Vec<ActiveJob>,
    pub scopes: Vec<JobScopeOutcome>,
}

impl ActiveJobs {
    /// Every scope's jobs, a job listed by two scopes counted once.
    pub fn of(listed: Vec<ScopeJobs>) -> Self {
        let mut all = ActiveJobs {
            jobs: Vec::new(),
            scopes: Vec::new(),
        };
        for scope in listed {
            all.scopes.push(JobScopeOutcome {
                scope: scope.scope,
                endpoint: scope.endpoint,
                outcome: scope.outcome,
                jobs: scope.jobs.len(),
            });
            for job in scope.jobs {
                if !all.jobs.iter().any(|j| j.id == job.id) {
                    all.jobs.push(job);
                }
            }
        }
        all
    }

    /// `repo`'s waiting jobs as `(name, labels)`, if its own scope was
    /// listed.
    pub fn waiting_in(&self, repo: &str) -> Option<Vec<(String, Vec<String>)>> {
        listed(&self.scopes, Scope::Repository).then(|| {
            self.jobs
                .iter()
                .filter(|j| j.activity == Activity::Waiting && j.repo.eq_ignore_ascii_case(repo))
                .map(|j| (j.name.clone(), j.labels.clone()))
                .collect()
        })
    }
}

/// Whether `scope` was listed.
pub fn listed(scopes: &[JobScopeOutcome], scope: Scope) -> bool {
    scopes
        .iter()
        .any(|s| s.scope == scope && s.outcome == "listed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_listed_by_two_scopes_counts_once() {
        let one = ActiveJob {
            id: 1,
            name: "job1".into(),
            repo: "acme/widgets".into(),
            labels: vec!["docker".into()],
            activity: Activity::Waiting,
            runner: None,
            queued_at: None,
        };
        let active = ActiveJobs::of(vec![
            ScopeJobs::listed(Scope::Repository, "/r".into(), vec![one.clone()]),
            ScopeJobs::listed(Scope::Instance, "/i".into(), vec![one]),
            ScopeJobs::not_listed(Scope::Owner, String::new(), "not_permitted: no".into()),
        ]);
        assert_eq!(active.jobs.len(), 1);
        assert_eq!(active.scopes[1].jobs, 1);
        assert!(listed(&active.scopes, Scope::Instance));
        assert!(!listed(&active.scopes, Scope::Owner));
        assert_eq!(
            active.waiting_in("Acme/Widgets"),
            Some(vec![("job1".to_owned(), vec!["docker".to_owned()])])
        );
        assert_eq!(active.waiting_in("acme/site"), Some(vec![]));
    }
}
