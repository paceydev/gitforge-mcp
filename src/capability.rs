//! What the bound forge can do for this caller, and therefore which tools
//! (and which actions within a tool) are offered.
//!
//! The version thresholds come from each forge's published API spec
//! (Gitea `release/v1.22`..`v28`, Forgejo `v9`..`v17`, GitHub's
//! `github/rest-api-description` for api.github.com and GHES 3.14..3.22),
//! checked 2026-11.
//! A tool is offered only when every feature it needs is available, so an
//! agent never sees a tool that can only fail.

use serde::Serialize;

use crate::site::{Permissions, RepoInfo, ServerInfo};
use crate::version::{
    CiKind, FORGEJO_9, FORGEJO_11, FORGEJO_12, FORGEJO_15, FORGEJO_16, FORGEJO_17, ForgeKind,
    GHES_3_14, GITEA_1_24, GITEA_1_25, GITEA_1_26, GITEA_28, Version,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// List and read workflow runs.
    CiRuns,
    /// List a run's jobs.
    CiJobs,
    /// Read a job's log.
    CiLogs,
    /// Read the end of a log without downloading all of it.
    CiLogTail,
    CiCancel,
    CiRerun,
    /// Rerun only the jobs that failed.
    CiRerunFailed,
    CiDispatch,
    /// List runners and whether they are idle, busy or offline.
    CiRunners,
    /// Jobs waiting for a runner.
    CiQueue,
    /// Start and end times per job, for duration estimates.
    CiJobTimings,
    /// CI outside the forge (Woodpecker, Jenkins…), read from commit
    /// statuses.
    CiStatuses,
    PullRequestRead,
    PullRequestWrite,
    /// Schedule a pull request to merge once its required checks pass, and
    /// cancel that.
    PullRequestAutoMerge,
    /// Read repository settings and plan changes to them.
    RepoSettingsRead,
    /// Change repository settings, and send a webhook a test delivery.
    RepoSettings,
    /// List a webhook's recent deliveries.
    WebhookDeliveries,
}

impl Feature {
    /// Features that change something on the forge.
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Feature::PullRequestWrite
                | Feature::PullRequestAutoMerge
                | Feature::CiCancel
                | Feature::CiRerun
                | Feature::CiRerunFailed
                | Feature::CiDispatch
                | Feature::RepoSettings
        )
    }

    pub const ALL: [Feature; 18] = [
        Feature::CiRuns,
        Feature::CiJobs,
        Feature::CiLogs,
        Feature::CiLogTail,
        Feature::CiCancel,
        Feature::CiRerun,
        Feature::CiRerunFailed,
        Feature::CiDispatch,
        Feature::CiRunners,
        Feature::CiQueue,
        Feature::CiJobTimings,
        Feature::CiStatuses,
        Feature::PullRequestRead,
        Feature::PullRequestWrite,
        Feature::PullRequestAutoMerge,
        Feature::RepoSettingsRead,
        Feature::RepoSettings,
        Feature::WebhookDeliveries,
    ];
}

/// Since which server version a forge supports a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Support {
    Always,
    Since(Version),
    Never,
    /// The forge has the API, but gitforge-mcp's driver for it doesn't use
    /// it yet, so nothing that needs it is offered.
    #[allow(dead_code)] // every forge feature is implemented today; kept for the next one
    NotYetImplemented,
}

/// The repository access a feature needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Pull,
    Push,
    Admin,
}

fn support(feature: Feature, kind: ForgeKind) -> Support {
    use Feature::*;
    use ForgeKind::*;
    match (feature, kind) {
        (_, Github) => match feature {
            // Logs are served by a redirect to storage that GitHub doesn't
            // promise will honour a range; a tail is cut after it arrives.
            CiLogTail => Support::Never,
            // Every endpoint the driver uses, repository settings, classic
            // branch protection, webhooks and their deliveries included, is
            // in GHES 3.14's API description; github.com is always current.
            _ => Support::Since(GHES_3_14),
        },
        // The repository, branch_protections and hooks endpoints, with
        // every field the driver uses, are in every spec checked.
        (PullRequestRead | PullRequestWrite | RepoSettingsRead | RepoSettings, _) => {
            Support::Always
        }
        // The commit statuses API predates every version supported.
        (CiStatuses, _) => Support::Always,
        // `merge_when_checks_succeed` on the merge endpoint, and DELETE on it
        // to cancel, are in every spec checked (Gitea 1.22, Forgejo 9 on).
        (PullRequestAutoMerge, _) => Support::Always,
        // Gitea keeps deliveries but its API doesn't list them (main has
        // it, unreleased); Forgejo has no endpoint.
        (WebhookDeliveries, _) => Support::Never,

        (CiRuns | CiJobs | CiJobTimings | CiQueue, Gitea) => Support::Since(GITEA_1_25),
        // Job logs exist from 1.24, but finding a job id needs 1.25's job list.
        (CiLogs, Gitea) => Support::Since(GITEA_1_25),
        (CiLogTail, Gitea) => Support::Never,
        (CiCancel, Gitea) => Support::Since(GITEA_28),
        (CiRerun | CiRerunFailed, Gitea) => Support::Since(GITEA_1_26),
        (CiDispatch | CiRunners, Gitea) => Support::Since(GITEA_1_24),

        (CiRuns, Forgejo) => Support::Since(FORGEJO_12),
        (CiJobs | CiLogs | CiLogTail | CiCancel, Forgejo) => Support::Since(FORGEJO_16),
        (CiRerun | CiRerunFailed, Forgejo) => Support::Since(FORGEJO_17),
        (CiDispatch | CiJobTimings, Forgejo) => Support::Since(FORGEJO_9),
        (CiRunners, Forgejo) => Support::Since(FORGEJO_15),
        (CiQueue, Forgejo) => Support::Since(FORGEJO_11),
    }
}

fn access(feature: Feature) -> Access {
    use Feature::*;
    match feature {
        CiRuns | CiJobs | CiLogs | CiLogTail | CiQueue | CiJobTimings | CiStatuses
        | PullRequestRead => Access::Pull,
        CiCancel | CiRerun | CiRerunFailed | CiDispatch | PullRequestWrite
        | PullRequestAutoMerge => Access::Push,
        // Gitea and Forgejo check repo admin for runner listing on top of
        // the token's scope.
        CiRunners | RepoSettingsRead | RepoSettings | WebhookDeliveries => Access::Admin,
    }
}

fn needs_actions(feature: Feature) -> bool {
    use Feature::*;
    matches!(
        feature,
        CiRuns
            | CiJobs
            | CiLogs
            | CiLogTail
            | CiCancel
            | CiRerun
            | CiRerunFailed
            | CiDispatch
            | CiRunners
            | CiQueue
            | CiJobTimings
    )
}

/// Features that change the repository, and so are off while it's archived.
fn writes(feature: Feature) -> bool {
    access(feature) == Access::Push || feature == Feature::RepoSettings
}

/// Whether one feature is available here, and if not, why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Availability {
    pub feature: Feature,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The CI system outside the forge this row is about ("woodpecker
    /// ci.example.com"); absent for the forge itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Capabilities(pub Vec<Availability>);

impl Capabilities {
    pub fn evaluate(server: &ServerInfo, repo: &RepoInfo) -> Self {
        Self(
            Feature::ALL
                .iter()
                .map(|&f| evaluate_one(f, server, repo))
                .collect(),
        )
    }

    /// Whether the forge, or any CI system outside it, gives `feature`.
    pub fn is_available(&self, feature: Feature) -> bool {
        self.0.iter().any(|a| a.feature == feature && a.available)
    }

    /// Whether `feature` is available from the forge itself (`provider`
    /// `None`) or from that one CI system outside it.
    pub fn available_on(&self, feature: Feature, provider: Option<&str>) -> bool {
        self.on(feature, provider).is_some_and(|a| a.available)
    }

    /// Why `feature` isn't available there, if it isn't.
    pub fn reason_on(&self, feature: Feature, provider: Option<&str>) -> String {
        self.on(feature, provider)
            .and_then(|a| a.reason.clone())
            .unwrap_or_else(|| "it has no API for this".into())
    }

    fn on(&self, feature: Feature, provider: Option<&str>) -> Option<&Availability> {
        self.0
            .iter()
            .find(|a| a.feature == feature && a.provider.as_deref() == provider)
    }

    /// The same, with a CI system's rows added.
    pub fn with(mut self, rows: Vec<Availability>) -> Self {
        self.0.extend(rows);
        self
    }

    /// The same, with every write turned off: the server's read-only mode.
    pub fn read_only(mut self) -> Self {
        for a in self
            .0
            .iter_mut()
            .filter(|a| a.feature.is_write() && a.available)
        {
            a.available = false;
            a.reason = Some("the server is in read-only mode (--read-only)".into());
            a.note = None;
        }
        self
    }
}

fn evaluate_one(feature: Feature, server: &ServerInfo, repo: &RepoInfo) -> Availability {
    let unavailable = |reason: String| Availability {
        feature,
        available: false,
        reason: Some(reason),
        note: None,
        provider: None,
    };
    let mut note = None;
    match support(feature, server.kind) {
        Support::Never => {
            return unavailable(format!("{} has no API for this", server.kind));
        }
        Support::NotYetImplemented => {
            return unavailable(format!(
                "{} has an API for this, but gitforge-mcp doesn't support it there yet",
                server.kind
            ));
        }
        Support::Since(min) => match server.version {
            // github.com has no version: it is always the newest.
            None if is_github_com(server) => {}
            Some(version) if version < min => {
                return unavailable(format!(
                    "needs {} {} or later; this server runs {}",
                    server.kind,
                    display_min(server.kind, min),
                    version
                ));
            }
            Some(_) => {}
            None => {
                note = Some(format!(
                    "{} did not report a version this server can read; assumed supported",
                    server.kind
                ));
            }
        },
        Support::Always => {}
    }
    if feature == Feature::PullRequestAutoMerge && repo.auto_merge_allowed == Some(false) {
        return unavailable(
            "auto-merge is off in this repository's settings (\"Allow auto-merge\")".into(),
        );
    }
    if needs_actions(feature) && repo.actions_enabled == Some(false) {
        return unavailable("Actions is disabled for this repository".into());
    }
    if repo.archived && writes(feature) {
        return unavailable("the repository is archived".into());
    }
    let p = repo.permissions;
    let missing = match access(feature) {
        Access::Pull if !p.pull => Some("read access to the repository"),
        Access::Push if !p.push => Some("write (push) access to the repository"),
        Access::Admin if !p.admin => Some("admin access to the repository"),
        _ => None,
    };
    if let Some(what) = missing {
        return unavailable(format!("needs {what}"));
    }
    if feature == Feature::CiJobTimings
        && server.kind == ForgeKind::Forgejo
        && server.version.is_some_and(|v| v < FORGEJO_17)
    {
        note = Some(
            "approximate before Forgejo 17: jobs carry no timestamps, so timings come from runner tasks"
                .into(),
        );
    }
    if feature == Feature::CiRerunFailed && server.kind == ForgeKind::Forgejo {
        note =
            Some("Forgejo has no rerun-failed endpoint; each failed job is rerun in turn".into());
    }
    Availability {
        feature,
        available: true,
        reason: None,
        note,
        provider: None,
    }
}

/// Woodpecker's API as gitforge-mcp uses it is the same in every 3.x
/// release (`server/router/api.go`, checked 3.0 to 3.20); 2.x is out of
/// upstream support and untested here.
const WOODPECKER_3: Version = Version::new(3, 0, 0);
/// Dispatch can run one workflow alone (`workflow` in the manual pipeline
/// options) from 3.20.
pub const WOODPECKER_3_20: Version = Version::new(3, 20, 0);

/// The caller's access on a CI system outside the forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CiAccess {
    pub permissions: Permissions,
    /// Agents and the queue are for the system's admins only.
    pub site_admin: bool,
    /// Signed in as a user, not anonymous.
    pub signed_in: bool,
}

/// The features a CI system outside the forge can add, and what each
/// needs there.
fn ci_support(feature: Feature, kind: CiKind) -> Option<(Support, CiNeed)> {
    use Feature::*;
    match (feature, kind) {
        (CiJobs | CiJobTimings | CiLogs, CiKind::Woodpecker) => {
            Some((Support::Since(WOODPECKER_3), CiNeed::Pull))
        }
        (CiCancel | CiRerun | CiDispatch, CiKind::Woodpecker) => {
            Some((Support::Since(WOODPECKER_3), CiNeed::Push))
        }
        // Woodpecker restarts whole pipelines; it has no rerun of failed
        // workflows, and serves a step's log only whole.
        (CiRerunFailed | CiLogTail, CiKind::Woodpecker) => Some((Support::Never, CiNeed::Pull)),
        (CiRunners | CiQueue, CiKind::Woodpecker) => {
            Some((Support::Since(WOODPECKER_3), CiNeed::SiteAdmin))
        }
        // Jenkins: a build's JSON, its console and the node and queue
        // lists, all in core since 1.x and readable with Overall/Read and
        // Job/Read.
        (CiJobs | CiJobTimings | CiLogs | CiRunners | CiQueue, CiKind::Jenkins) => {
            Some((Support::Always, CiNeed::Pull))
        }
        // Jenkins: a build's `stop`, and a job's `build` and
        // `buildWithParameters`, all in core since 1.x. Jenkins doesn't
        // say what a user may do, so only a signed-in user is asked for.
        (CiCancel | CiRerun | CiDispatch, CiKind::Jenkins) => {
            Some((Support::Always, CiNeed::SignedIn))
        }
        // Rebuilding only what failed, and a log's tail, have no API in
        // core Jenkins.
        (CiRerunFailed | CiLogTail, CiKind::Jenkins) => Some((Support::Never, CiNeed::Pull)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiNeed {
    Pull,
    Push,
    SiteAdmin,
    /// A user, not anonymous: for a system that doesn't report what its
    /// users may do.
    SignedIn,
}

/// What to know about a CI feature that is available.
fn ci_note(feature: Feature, kind: CiKind) -> Option<String> {
    let permission = match (feature, kind) {
        (Feature::CiCancel, CiKind::Jenkins) => "Job/Cancel",
        (Feature::CiDispatch, CiKind::Jenkins) => "Job/Build",
        (Feature::CiRerun, CiKind::Jenkins) => {
            return Some(
                "Jenkins has no rebuild: each job is built again with the same parameters (Job/Build), and a branch job builds its branch as it is now"
                    .into(),
            );
        }
        _ => return None,
    };
    Some(format!(
        "Jenkins doesn't say what a user may do; a refusal means it lacks {permission}"
    ))
}

/// The rows a CI system outside the forge adds, under `name`.
pub fn evaluate_ci(
    name: &str,
    kind: CiKind,
    version: Option<Version>,
    access: CiAccess,
) -> Vec<Availability> {
    Feature::ALL
        .iter()
        .filter_map(|&f| ci_support(f, kind).map(|s| (f, s)))
        .map(|(feature, (support, need))| {
            let (reason, note) = ci_reason(kind, version, access, support, need);
            let note = note.or_else(|| reason.is_none().then(|| ci_note(feature, kind)).flatten());
            Availability {
                feature,
                available: reason.is_none(),
                reason,
                note,
                provider: Some(name.to_owned()),
            }
        })
        .collect()
}

/// Why a CI feature is unavailable, or a note on one that is.
fn ci_reason(
    kind: CiKind,
    version: Option<Version>,
    access: CiAccess,
    support: Support,
    need: CiNeed,
) -> (Option<String>, Option<String>) {
    let mut note = None;
    match (support, version) {
        (Support::Never, _) => return (Some(format!("{kind} has no API for this")), None),
        (Support::NotYetImplemented, _) => {
            return (
                Some(format!(
                    "gitforge-mcp doesn't do this on {kind} yet; do it in {kind}"
                )),
                None,
            );
        }
        (Support::Since(min), Some(v)) if v < min => {
            return (
                Some(format!("needs {kind} {min} or later; this server runs {v}")),
                None,
            );
        }
        (Support::Since(_), None) => {
            note = Some(format!(
                "{kind} did not report a version this server can read; assumed supported"
            ));
        }
        _ => {}
    }
    let p = access.permissions;
    let missing = match need {
        CiNeed::Pull if !p.pull => Some(format!("read access to the repository on {kind}")),
        CiNeed::Push if !p.push => Some(format!(
            "write (push) access to the repository on {kind}: sign in with a {kind} token"
        )),
        CiNeed::SiteAdmin if !access.site_admin => Some(format!("a {kind} admin account")),
        CiNeed::SignedIn if !access.signed_in => Some(format!(
            "a {kind} user: sign in with a {kind} API token and its user"
        )),
        _ => None,
    };
    match missing {
        Some(what) => (Some(format!("needs {what}")), None),
        None => (None, note),
    }
}

/// github.com reports no version; GitHub Enterprise Server always does, or
/// says "unknown".
fn is_github_com(server: &ServerInfo) -> bool {
    server.kind == ForgeKind::Github && server.version_raw.is_none()
}

/// Gitea's thresholds before v28 read as 1.x; print them that way.
fn display_min(kind: ForgeKind, min: Version) -> String {
    match (kind, min.patch) {
        (ForgeKind::Gitea, 0) if min.major == 1 => format!("{}.{}", min.major, min.minor),
        (ForgeKind::Github, 0) => format!("Enterprise Server {}.{}", min.major, min.minor),
        (_, 0) if min.minor == 0 => min.major.to_string(),
        _ => min.to_string(),
    }
}

/// When a tool, or one action of a tool, is offered.
#[derive(Debug, Clone, Copy)]
pub enum Gate {
    /// Whatever the forge, reachable or not.
    Always,
    /// Only when every listed feature is available.
    All(&'static [Feature]),
    /// When at least one listed feature is available: a tool whose actions
    /// are gated one by one.
    Any(&'static [Feature]),
}

impl Gate {
    /// `None` means the forge hasn't been reached and nothing is cached, so
    /// only always-on tools are offered.
    pub fn allows(self, caps: Option<&Capabilities>) -> bool {
        match (self, caps) {
            (Gate::Always, _) => true,
            (_, None) => false,
            (Gate::All(features), Some(caps)) => features.iter().all(|&f| caps.is_available(f)),
            (Gate::Any(features), Some(caps)) => features.iter().any(|&f| caps.is_available(f)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::Permissions;

    fn server(kind: ForgeKind, version: Option<Version>) -> ServerInfo {
        ServerInfo {
            kind,
            version,
            version_raw: version.map(|v| v.to_string()),
            detected_by: "test".into(),
        }
    }

    fn repo(admin: bool, push: bool) -> RepoInfo {
        RepoInfo {
            full_name: "acme/widgets".into(),
            default_branch: "main".into(),
            private: false,
            archived: false,
            permissions: Permissions {
                admin,
                push,
                pull: true,
            },
            actions_enabled: Some(true),
            auto_merge_allowed: None,
        }
    }

    fn find(caps: &Capabilities, f: Feature) -> &Availability {
        caps.0.iter().find(|a| a.feature == f).unwrap()
    }

    #[test]
    fn forgejo_15_lacks_logs_cancel_and_rerun() {
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Forgejo, Some(Version::new(15, 0, 11))),
            &repo(true, true),
        );
        assert!(caps.is_available(Feature::CiRuns));
        assert!(caps.is_available(Feature::CiRunners));
        for f in [Feature::CiLogs, Feature::CiCancel, Feature::CiRerun] {
            assert!(!caps.is_available(f), "{f:?}");
        }
        assert_eq!(
            find(&caps, Feature::CiCancel).reason.as_deref(),
            Some("needs Forgejo 16 or later; this server runs 15.0.11")
        );
        assert!(
            find(&caps, Feature::CiJobTimings)
                .note
                .as_deref()
                .unwrap()
                .starts_with("approximate")
        );
    }

    #[test]
    fn gitea_cancel_needs_v28_and_reads_1x_thresholds() {
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Gitea, Some(Version::new(1, 26, 2))),
            &repo(true, true),
        );
        assert!(caps.is_available(Feature::CiRerun));
        assert!(!caps.is_available(Feature::CiCancel));
        let old = Capabilities::evaluate(
            &server(ForgeKind::Gitea, Some(Version::new(1, 24, 0))),
            &repo(true, true),
        );
        assert_eq!(
            find(&old, Feature::CiRuns).reason.as_deref(),
            Some("needs Gitea 1.25 or later; this server runs 1.24.0")
        );
        let v28 = Capabilities::evaluate(
            &server(ForgeKind::Gitea, Some(Version::new(28, 3, 0))),
            &repo(true, true),
        );
        assert!(v28.is_available(Feature::CiCancel));
    }

    #[test]
    fn permissions_gate_writes_and_admin_features() {
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Forgejo, Some(Version::new(17, 0, 1))),
            &repo(false, false),
        );
        assert!(caps.is_available(Feature::CiRuns));
        assert!(caps.is_available(Feature::PullRequestRead));
        assert!(!caps.is_available(Feature::PullRequestWrite));
        assert!(!caps.is_available(Feature::CiRunners));
        assert_eq!(
            find(&caps, Feature::RepoSettings).reason.as_deref(),
            Some("needs admin access to the repository")
        );
    }

    #[test]
    fn auto_merge_needs_push_and_works_on_every_gitea_and_forgejo() {
        for (kind, version) in [
            (ForgeKind::Gitea, Version::new(1, 25, 5)),
            (ForgeKind::Gitea, Version::new(28, 3, 0)),
            (ForgeKind::Forgejo, Version::new(15, 0, 11)),
            (ForgeKind::Forgejo, Version::new(17, 0, 1)),
        ] {
            let writer = Capabilities::evaluate(&server(kind, Some(version)), &repo(false, true));
            assert!(
                writer.is_available(Feature::PullRequestAutoMerge),
                "{kind} {version}"
            );
            let reader = Capabilities::evaluate(&server(kind, Some(version)), &repo(false, false));
            assert_eq!(
                find(&reader, Feature::PullRequestAutoMerge)
                    .reason
                    .as_deref(),
                Some("needs write (push) access to the repository")
            );
        }
        let mut archived = repo(true, true);
        archived.archived = true;
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Forgejo, Some(Version::new(17, 0, 1))),
            &archived,
        );
        assert!(!caps.is_available(Feature::PullRequestAutoMerge));
    }

    #[test]
    fn github_com_offers_every_ci_and_pull_request_feature_without_notes() {
        let caps = Capabilities::evaluate(&server(ForgeKind::Github, None), &repo(true, true));
        for f in Feature::ALL {
            let a = find(&caps, f);
            match f {
                Feature::CiLogTail => {
                    assert_eq!(a.reason.as_deref(), Some("GitHub has no API for this"))
                }
                _ => assert!(a.available && a.note.is_none(), "{f:?}: {a:?}"),
            }
        }
    }

    #[test]
    fn github_enterprise_needs_3_14_and_says_when_its_version_is_unknown() {
        let old = Capabilities::evaluate(
            &server(ForgeKind::Github, Some(Version::new(3, 13, 2))),
            &repo(true, true),
        );
        assert_eq!(
            find(&old, Feature::CiRuns).reason.as_deref(),
            Some("needs GitHub Enterprise Server 3.14 or later; this server runs 3.13.2")
        );
        let current = Capabilities::evaluate(
            &server(ForgeKind::Github, Some(Version::new(3, 19, 1))),
            &repo(false, true),
        );
        assert!(current.is_available(Feature::CiRerunFailed));
        assert!(!current.is_available(Feature::CiRunners));
        let unknown = ServerInfo {
            version_raw: Some("unknown".into()),
            ..server(ForgeKind::Github, None)
        };
        let caps = Capabilities::evaluate(&unknown, &repo(true, true));
        assert!(find(&caps, Feature::CiRuns).note.is_some());
    }

    #[test]
    fn github_auto_merge_follows_the_repository_setting() {
        let mut r = repo(false, true);
        r.auto_merge_allowed = Some(false);
        let caps = Capabilities::evaluate(&server(ForgeKind::Github, None), &r);
        assert!(
            find(&caps, Feature::PullRequestAutoMerge)
                .reason
                .as_deref()
                .unwrap()
                .contains("Allow auto-merge")
        );
        assert!(caps.is_available(Feature::PullRequestWrite));
        r.auto_merge_allowed = None;
        let caps = Capabilities::evaluate(&server(ForgeKind::Github, None), &r);
        assert!(caps.is_available(Feature::PullRequestAutoMerge));
    }

    #[test]
    fn disabled_actions_and_archived_repos() {
        let mut r = repo(true, true);
        r.actions_enabled = Some(false);
        let caps =
            Capabilities::evaluate(&server(ForgeKind::Gitea, Some(Version::new(28, 0, 0))), &r);
        assert!(!caps.is_available(Feature::CiRuns));
        assert!(caps.is_available(Feature::PullRequestWrite));

        let mut r = repo(true, true);
        r.archived = true;
        let caps =
            Capabilities::evaluate(&server(ForgeKind::Gitea, Some(Version::new(28, 0, 0))), &r);
        assert!(caps.is_available(Feature::CiRuns));
        assert!(!caps.is_available(Feature::PullRequestWrite));
        assert!(caps.is_available(Feature::CiRunners));
        assert!(caps.is_available(Feature::RepoSettingsRead));
        assert!(!caps.is_available(Feature::RepoSettings));
    }

    #[test]
    fn unknown_version_is_assumed_current_with_a_note() {
        let caps = Capabilities::evaluate(&server(ForgeKind::Forgejo, None), &repo(true, true));
        let cancel = find(&caps, Feature::CiCancel);
        assert!(cancel.available);
        assert!(
            cancel
                .note
                .as_deref()
                .unwrap()
                .contains("assumed supported")
        );
    }

    #[test]
    fn webhook_deliveries_only_on_github() {
        let gh = Capabilities::evaluate(&server(ForgeKind::Github, None), &repo(true, true));
        assert!(gh.is_available(Feature::WebhookDeliveries));
        assert!(!gh.is_available(Feature::CiLogTail));
        let gitea = Capabilities::evaluate(
            &server(ForgeKind::Gitea, Some(Version::new(28, 3, 0))),
            &repo(true, true),
        );
        assert_eq!(
            find(&gitea, Feature::WebhookDeliveries).reason.as_deref(),
            Some("Gitea has no API for this")
        );
    }

    #[test]
    fn gates() {
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Forgejo, Some(Version::new(15, 0, 11))),
            &repo(true, true),
        );
        assert!(Gate::Always.allows(None));
        assert!(!Gate::All(&[Feature::CiRuns]).allows(None));
        assert!(Gate::All(&[Feature::CiRuns]).allows(Some(&caps)));
        assert!(!Gate::All(&[Feature::CiRuns, Feature::CiLogs]).allows(Some(&caps)));
        assert!(Gate::Any(&[Feature::CiCancel, Feature::CiDispatch]).allows(Some(&caps)));
        assert!(!Gate::Any(&[Feature::CiCancel, Feature::CiRerun]).allows(Some(&caps)));
        assert!(!Gate::Any(&[Feature::CiDispatch]).allows(None));
    }

    #[test]
    fn woodpecker_adds_rows_of_its_own() {
        let reader = CiAccess {
            permissions: Permissions {
                pull: true,
                ..Permissions::default()
            },
            site_admin: false,
            signed_in: true,
        };
        let name = "woodpecker ci.example.com";
        let forge = Capabilities::evaluate(
            &server(ForgeKind::Forgejo, Some(Version::new(15, 0, 11))),
            &repo(false, false),
        );
        assert!(!forge.is_available(Feature::CiLogs));
        let caps = forge.clone().with(evaluate_ci(
            name,
            CiKind::Woodpecker,
            Some(Version::new(3, 20, 1)),
            reader,
        ));
        // The union gates tools; the forge's own rows are unchanged.
        assert!(caps.is_available(Feature::CiLogs));
        assert!(!caps.available_on(Feature::CiLogs, None));
        assert!(caps.available_on(Feature::CiLogs, Some(name)));
        assert!(!caps.available_on(Feature::CiCancel, Some(name)));
        assert!(
            caps.reason_on(Feature::CiCancel, Some(name))
                .contains("sign in with a Woodpecker token")
        );
        assert_eq!(
            caps.reason_on(Feature::CiRerunFailed, Some(name)),
            "Woodpecker has no API for this"
        );
        assert_eq!(
            caps.reason_on(Feature::CiRunners, Some(name)),
            "needs a Woodpecker admin account"
        );
        assert!(!caps.0.iter().any(|a| a.provider.is_some()
            && matches!(a.feature, Feature::CiRuns | Feature::PullRequestRead)));

        let admin = CiAccess {
            permissions: Permissions {
                pull: true,
                push: true,
                admin: true,
            },
            site_admin: true,
            signed_in: true,
        };
        let old = evaluate_ci(name, CiKind::Woodpecker, Some(Version::new(2, 8, 0)), admin);
        assert!(old.iter().all(|a| !a.available));
        assert_eq!(
            old[0].reason.as_deref(),
            Some("needs Woodpecker 3.0.0 or later; this server runs 2.8.0")
        );
        let writer = forge
            .with(evaluate_ci(name, CiKind::Woodpecker, None, admin))
            .read_only();
        assert!(writer.available_on(Feature::CiRunners, Some(name)));
        assert!(!writer.available_on(Feature::CiCancel, Some(name)));
    }

    #[test]
    fn jenkins_writes_need_a_user_and_say_what_a_refusal_means() {
        let name = "jenkins ci.example.com";
        let reader = CiAccess {
            permissions: Permissions {
                pull: true,
                ..Permissions::default()
            },
            ..CiAccess::default()
        };
        let anonymous = evaluate_ci(name, CiKind::Jenkins, None, reader);
        let cancel = anonymous
            .iter()
            .find(|a| a.feature == Feature::CiCancel)
            .unwrap();
        assert!(!cancel.available);
        assert_eq!(
            cancel.reason.as_deref(),
            Some("needs a Jenkins user: sign in with a Jenkins API token and its user")
        );
        let user = evaluate_ci(
            name,
            CiKind::Jenkins,
            Version::parse("2.580.2"),
            CiAccess {
                signed_in: true,
                ..reader
            },
        );
        for feature in [Feature::CiCancel, Feature::CiRerun, Feature::CiDispatch] {
            let row = user.iter().find(|a| a.feature == feature).unwrap();
            assert!(row.available, "{feature:?}");
            assert!(
                row.note.as_deref().is_some_and(|n| n.contains("Job/")),
                "{feature:?}"
            );
        }
        assert!(
            user.iter()
                .any(|a| a.feature == Feature::CiRerunFailed && !a.available)
        );
    }

    #[test]
    fn read_only_mode_turns_off_every_write() {
        let caps = Capabilities::evaluate(
            &server(ForgeKind::Gitea, Some(Version::new(28, 3, 0))),
            &repo(true, true),
        )
        .read_only();
        assert!(caps.is_available(Feature::CiRuns));
        assert!(caps.is_available(Feature::RepoSettingsRead));
        for f in Feature::ALL.iter().filter(|f| f.is_write()) {
            assert!(!caps.is_available(*f), "{f:?}");
        }
        assert!(
            find(&caps, Feature::CiCancel)
                .reason
                .as_deref()
                .unwrap()
                .contains("read-only mode")
        );
    }
}
