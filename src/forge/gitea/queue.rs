//! Jobs running and waiting in every scope the credential can list them in:
//! the repository, its owner (user or organisation) and the instance. These
//! show which repositories are holding shared runners, which the runners
//! themselves don't say.
//!
//! Gitea and Forgejo list them differently:
//!
//! - Forgejo's `…/actions/runners/jobs` returns only the jobs waiting and
//!   running, unpaged, as `repo_id`s. Forgejo 17 adds each job's `html_url`,
//!   which names the repository; before it, each other repository is looked
//!   up by id once. Neither says when a job was queued or which runner took
//!   it.
//! - Gitea's `…/actions/jobs` lists every job ever run, paged, filtered by
//!   `status`. Each names its runner, and when it was queued.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

use super::{GiteaFamily, RepoBody};
use crate::forge::shapes::{ActionsJob, ActionsJobList, named, nonzero};
use crate::forge::{ApiError, User};
use crate::remote::RepoRef;
use crate::runners::{ActiveJob, ActiveJobs, Activity, Scope, ScopeJobs};
use crate::version::{FORGEJO_11, GITEA_1_25};

/// Gitea's largest page by default.
const PAGE_SIZE: usize = 50;
/// Pages read for each state in each scope. A longer queue is reported as
/// far as this reaches.
const JOB_PAGES: u32 = 4;
/// What a missing endpoint means: an older forge.
const NOT_FOUND: &str = "unavailable on this forge version";

/// A job from Forgejo's runner job listing.
#[derive(Deserialize)]
struct ForgejoQueued {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    runs_on: Option<Vec<String>>,
    #[serde(default)]
    repo_id: u64,
    /// Forgejo 17 on.
    html_url: Option<String>,
}

impl GiteaFamily {
    /// The endpoint for each scope, `<prefix><tail>`: the repository's,
    /// the owner's (under `/user` for your own repositories and `/orgs`
    /// otherwise, which needs to know who you are) and the instance's
    /// (only for site administrators). A scope that can't be asked is
    /// `Err` with why.
    pub(super) fn scope_paths(
        repo: &RepoRef,
        user: Option<&User>,
        tail: &str,
        what: &str,
    ) -> [(Scope, Result<String, String>); 3] {
        let owner = match user {
            Some(u) if u.login.eq_ignore_ascii_case(&repo.owner) => Ok(format!("/user{tail}")),
            Some(_) => Ok(format!("/orgs/{}{tail}", repo.owner)),
            None => Err(format!(
                "not_permitted: the signed-in user is unknown (the token can't read it), so the owner's {what} can't be located"
            )),
        };
        let instance = match user {
            Some(u) if !u.site_admin => Err(format!(
                "not_permitted: instance {what} are listed only to site administrators"
            )),
            _ => Ok(format!("/admin{tail}")),
        };
        [
            (
                Scope::Repository,
                Ok(format!("/repos/{}/{}{tail}", repo.owner, repo.name)),
            ),
            (Scope::Owner, owner),
            (Scope::Instance, instance),
        ]
    }

    /// Which job listing this forge has, if any.
    fn listing(&self) -> Option<Listing> {
        match (
            self.is_forgejo(),
            self.at_least(FORGEJO_11),
            self.at_least(GITEA_1_25),
        ) {
            (true, true, _) => Some(Listing::Forgejo),
            (false, _, true) => Some(Listing::Gitea),
            _ => None,
        }
    }

    /// The jobs running and waiting in each scope, or `None` where this
    /// forge version lists none.
    pub(super) async fn active_jobs(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> Result<Option<ActiveJobs>, ApiError> {
        match self.listing() {
            Some(Listing::Forgejo) => self.forgejo_active(repo, user).await.map(Some),
            Some(Listing::Gitea) => self.gitea_active(repo, user).await.map(Some),
            None => Ok(None),
        }
    }

    async fn forgejo_active(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> Result<ActiveJobs, ApiError> {
        let paths = Self::scope_paths(repo, user, "/actions/runners/jobs", "jobs");
        let listed = self.forgejo_scopes(paths).await?;
        let known = known_names(repo, &listed);
        let names = self.look_up_names(&listed, known).await?;
        Ok(ActiveJobs::of(forgejo_jobs(listed, &names)))
    }

    async fn forgejo_scopes(
        &self,
        paths: [(Scope, Result<String, String>); 3],
    ) -> Result<Vec<ScopeJobs<ForgejoQueued>>, ApiError> {
        let mut listed = Vec::new();
        for (scope, path) in paths {
            listed.push(self.forgejo_scope(scope, path).await?);
        }
        Ok(listed)
    }

    async fn forgejo_scope(
        &self,
        scope: Scope,
        path: Result<String, String>,
    ) -> Result<ScopeJobs<ForgejoQueued>, ApiError> {
        match path {
            Err(outcome) => Ok(ScopeJobs::not_listed(scope, String::new(), outcome)),
            Ok(path) => asked(
                scope,
                self.http
                    .get_json::<Option<Vec<ForgejoQueued>>>(&self.api(&path))
                    .await
                    .map(Option::unwrap_or_default),
                path,
            ),
        }
    }

    /// `names` with every other repository the jobs name looked up.
    async fn look_up_names(
        &self,
        listed: &[ScopeJobs<ForgejoQueued>],
        mut names: HashMap<u64, String>,
    ) -> Result<HashMap<u64, String>, ApiError> {
        for id in listed.iter().flat_map(|s| &s.jobs).map(|j| j.repo_id) {
            if let Entry::Vacant(slot) = names.entry(id) {
                slot.insert(self.repo_name(id).await?);
            }
        }
        Ok(names)
    }

    /// A repository's name by its id, or `repository #<id>` where the
    /// credential can't read it. Asked once per repository.
    async fn repo_name(&self, id: u64) -> Result<String, ApiError> {
        match cached_name(&self.base, id) {
            Some(name) => Ok(name),
            None => remember_name(
                &self.base,
                id,
                self.http
                    .get_json_opt::<RepoBody>(&self.api(&format!("/repositories/{id}")))
                    .await,
            ),
        }
    }

    async fn gitea_active(
        &self,
        repo: &RepoRef,
        user: Option<&User>,
    ) -> Result<ActiveJobs, ApiError> {
        let paths = Self::scope_paths(repo, user, "/actions/jobs", "jobs");
        let listed = self.gitea_scopes(paths).await?;
        Ok(ActiveJobs::of(gitea_jobs(listed, &repo.full_name())))
    }

    async fn gitea_scopes(
        &self,
        paths: [(Scope, Result<String, String>); 3],
    ) -> Result<Vec<ScopeJobs<(ActionsJob, Activity)>>, ApiError> {
        let mut listed = Vec::new();
        for (scope, path) in paths {
            listed.push(self.gitea_scope(scope, path).await?);
        }
        Ok(listed)
    }

    async fn gitea_scope(
        &self,
        scope: Scope,
        path: Result<String, String>,
    ) -> Result<ScopeJobs<(ActionsJob, Activity)>, ApiError> {
        match path {
            Err(outcome) => Ok(ScopeJobs::not_listed(scope, String::new(), outcome)),
            Ok(path) => asked(scope, self.gitea_scope_jobs(&path).await, path),
        }
    }

    /// One scope's queued and running jobs.
    async fn gitea_scope_jobs(&self, path: &str) -> Result<Vec<(ActionsJob, Activity)>, ApiError> {
        let mut found = Vec::new();
        for (status, activity) in [
            ("queued", Activity::Waiting),
            ("in_progress", Activity::Running),
        ] {
            for page in 1..=JOB_PAGES {
                let url = self.api(&format!(
                    "{path}?status={status}&limit={PAGE_SIZE}&page={page}"
                ));
                let list: ActionsJobList = self.http.get_json(&url).await?;
                let jobs = list.jobs.unwrap_or_default();
                let full = jobs.len() >= PAGE_SIZE;
                found.extend(jobs.into_iter().map(|j| (j, activity)));
                if !full {
                    break;
                }
            }
        }
        Ok(found)
    }
}

/// The two ways the forges list jobs.
enum Listing {
    Forgejo,
    Gitea,
}

/// What asking a scope at `endpoint` said: listed, or why not. Other
/// failures are the caller's.
fn asked<T>(
    scope: Scope,
    asked: Result<Vec<T>, ApiError>,
    endpoint: String,
) -> Result<ScopeJobs<T>, ApiError> {
    match asked {
        Ok(found) => Ok(ScopeJobs::listed(scope, endpoint, found)),
        Err(ApiError::Refused { hint, .. }) => Ok(ScopeJobs::not_listed(
            scope,
            endpoint,
            format!("not_permitted: {hint}"),
        )),
        Err(ApiError::NotFound { .. }) => {
            Ok(ScopeJobs::not_listed(scope, endpoint, NOT_FOUND.to_owned()))
        }
        Err(e) => Err(e),
    }
}

/// The repositories named without asking: this one, by the id its own jobs
/// carry, and those whose jobs have a page (Forgejo 17).
fn known_names(repo: &RepoRef, listed: &[ScopeJobs<ForgejoQueued>]) -> HashMap<u64, String> {
    let own = listed
        .iter()
        .filter(|s| s.scope == Scope::Repository)
        .flat_map(|s| s.jobs.iter().map(|j| (j.repo_id, repo.full_name())));
    let paged = listed.iter().flat_map(|s| &s.jobs).filter_map(|j| {
        j.html_url
            .as_deref()
            .and_then(repo_in_url)
            .map(|name| (j.repo_id, name))
    });
    // The repository's own name, as the checkout spells it, wins.
    paged.chain(own).collect()
}

/// Forgejo's jobs, each named by its repository.
fn forgejo_jobs(
    listed: Vec<ScopeJobs<ForgejoQueued>>,
    names: &HashMap<u64, String>,
) -> Vec<ScopeJobs> {
    listed
        .into_iter()
        .map(|s| ScopeJobs {
            scope: s.scope,
            endpoint: s.endpoint,
            outcome: s.outcome,
            jobs: s
                .jobs
                .into_iter()
                .filter_map(|job| {
                    let activity = match job.status.as_str() {
                        "waiting" => Activity::Waiting,
                        "running" => Activity::Running,
                        _ => return None,
                    };
                    Some(ActiveJob {
                        id: job.id,
                        repo: names
                            .get(&job.repo_id)
                            .cloned()
                            .unwrap_or_else(|| format!("repository #{}", job.repo_id)),
                        name: job.name,
                        labels: job.runs_on.unwrap_or_default(),
                        activity,
                        runner: None,
                        queued_at: None,
                    })
                })
                .collect(),
        })
        .collect()
}

/// Gitea's jobs, each named by its repository: `own` for the repository's
/// own scope, else the one its page names.
fn gitea_jobs(listed: Vec<ScopeJobs<(ActionsJob, Activity)>>, own: &str) -> Vec<ScopeJobs> {
    listed
        .into_iter()
        .map(|s| {
            let own = (s.scope == Scope::Repository).then_some(own);
            ScopeJobs {
                scope: s.scope,
                endpoint: s.endpoint,
                outcome: s.outcome,
                jobs: s
                    .jobs
                    .into_iter()
                    .map(|(job, activity)| ActiveJob {
                        repo: own
                            .map(str::to_owned)
                            .or_else(|| job.html_url.as_deref().and_then(repo_in_url))
                            .unwrap_or_else(|| "a repository".to_owned()),
                        id: job.id,
                        name: job.name,
                        labels: job.labels.unwrap_or_default(),
                        activity,
                        runner: (activity == Activity::Running)
                            .then(|| named(job.runner_name))
                            .flatten(),
                        queued_at: nonzero(job.created_at),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Repository names already looked up, by instance and id.
fn names_cache() -> &'static Mutex<HashMap<(String, u64), String>> {
    static NAMES: OnceLock<Mutex<HashMap<(String, u64), String>>> = OnceLock::new();
    NAMES.get_or_init(Default::default)
}

fn cached_name(base: &url::Url, id: u64) -> Option<String> {
    names_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(base.to_string(), id))
        .cloned()
}

/// The name a lookup found, kept for next time; `repository #<id>` where
/// the credential can't read it.
fn remember_name(
    base: &url::Url,
    id: u64,
    found: Result<Option<RepoBody>, ApiError>,
) -> Result<String, ApiError> {
    let name = match found {
        Ok(Some(body)) => body.full_name,
        Ok(None) | Err(ApiError::Refused { .. }) => format!("repository #{id}"),
        Err(e) => return Err(e),
    };
    names_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((base.to_string(), id), name.clone());
    Ok(name)
}

/// `owner/name` from a job's page, `…/<owner>/<name>/actions/runs/…`,
/// whatever path the instance is served under.
fn repo_in_url(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let segments: Vec<&str> = url.path_segments()?.collect();
    let at = segments.windows(2).position(|w| w == ["actions", "runs"])?;
    (at >= 2).then(|| format!("{}/{}", segments[at - 2], segments[at - 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gitea 1.25's listing, as it reads with a job running on the shared
    /// runner and another waiting for it.
    #[test]
    fn a_running_job_keeps_the_runner_it_runs_on() {
        let job = |v: serde_json::Value| -> ActionsJob { serde_json::from_value(v).unwrap() };
        let running = job(serde_json::json!({
            "id": 9, "name": "hold", "status": "in_progress", "labels": ["shared"],
            "runner_id": 2, "runner_name": "shared-runner",
            "html_url": "http://127.0.0.1:3101/gfadmin/queue-a/actions/runs/1/jobs/0",
            "created_at": "2026-11-17T00:05:33Z", "started_at": "2026-11-17T00:05:36Z"
        }));
        let queued = job(serde_json::json!({
            "id": 11, "name": "hold", "status": "queued", "labels": ["shared"],
            "runner_name": "",
            "html_url": "http://127.0.0.1:3101/gfadmin/queue-b/actions/runs/1/jobs/0",
            "created_at": "2026-11-17T00:05:34Z"
        }));
        let listed = vec![ScopeJobs::listed(
            Scope::Instance,
            "/admin/actions/jobs".into(),
            vec![(running, Activity::Running), (queued, Activity::Waiting)],
        )];
        let jobs = &gitea_jobs(listed, "gfadmin/queue-a")[0].jobs;
        assert_eq!(jobs[0].runner.as_deref(), Some("shared-runner"));
        assert_eq!(jobs[0].repo, "gfadmin/queue-a");
        assert_eq!(jobs[1].runner, None);
        assert_eq!(jobs[1].repo, "gfadmin/queue-b");
        assert_eq!(jobs[1].queued_at.as_deref(), Some("2026-11-17T00:05:34Z"));
    }

    #[test]
    fn a_jobs_page_names_its_repository() {
        assert_eq!(
            repo_in_url("https://git.example.com/acme/widgets/actions/runs/2/jobs/0/attempt/1"),
            Some("acme/widgets".into())
        );
        assert_eq!(
            repo_in_url("https://example.com/git/acme/widgets/actions/runs/2/jobs/0"),
            Some("acme/widgets".into())
        );
        assert_eq!(repo_in_url("https://example.com/actions/runs/2"), None);
        assert_eq!(repo_in_url("not a url"), None);
    }

    #[test]
    fn a_scope_that_refuses_is_reported_not_raised() {
        let refused: Result<Vec<u8>, ApiError> = Err(ApiError::Refused {
            host: "git.example.com".into(),
            status: 403,
            hint: "token lacks read:admin".into(),
        });
        let scope = asked(Scope::Instance, refused, "/admin/actions/jobs".into()).unwrap();
        assert_eq!(scope.outcome, "not_permitted: token lacks read:admin");
        assert_eq!(scope.endpoint, "/admin/actions/jobs");
    }

    #[test]
    fn scopes_follow_who_you_are() {
        let repo = RepoRef {
            host: "git.example.com".into(),
            base_url: url::Url::parse("https://git.example.com").unwrap(),
            owner: "acme".into(),
            name: "widgets".into(),
        };
        let member = User {
            login: "pat".into(),
            site_admin: false,
        };
        let paths = GiteaFamily::scope_paths(&repo, Some(&member), "/actions/jobs", "jobs");
        assert_eq!(
            paths[0].1.as_deref(),
            Ok("/repos/acme/widgets/actions/jobs")
        );
        assert_eq!(paths[1].1.as_deref(), Ok("/orgs/acme/actions/jobs"));
        assert!(
            paths[2]
                .1
                .as_ref()
                .unwrap_err()
                .starts_with("not_permitted")
        );
        let owner = User {
            login: "acme".into(),
            site_admin: true,
        };
        let paths = GiteaFamily::scope_paths(&repo, Some(&owner), "/actions/runners/jobs", "jobs");
        assert_eq!(paths[1].1.as_deref(), Ok("/user/actions/runners/jobs"));
        assert_eq!(paths[2].1.as_deref(), Ok("/admin/actions/runners/jobs"));
    }
}
