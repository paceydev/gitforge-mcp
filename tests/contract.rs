//! Contract tests against real Gitea and Forgejo builds, started by
//! scripts/contract/provision.sh (CI) or scripts/contract/run-local.sh.
//!
//! Three things are checked for every forge:
//!
//! 1. What forge_context reports through the real binary: kind, version,
//!    user, permissions for an admin and a read-only collaborator, and
//!    Actions switched off for one repository.
//! 2. That the capability table is true: each version-gated feature is
//!    reported available exactly when the server's own API spec
//!    (`/swagger.v1.json`) has the endpoint for it.
//! 3. What the CI and pull request tools report about a real workflow run
//!    (scripts/contract/workflow.yml: one job passes, one fails, one is
//!    skipped, one waits on another) on a runner registered to the repo,
//!    and about an open pull request.
//!
//! Skipped unless GITFORGE_CONTRACT_TARGETS names the targets file;
//! GITFORGE_CONTRACT_REQUIRED=1 turns the skip into a failure, as CI does.

mod common;

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    kind: String,
    version: String,
    base_url: String,
    admin_token: String,
    reader_token: String,
    ci_sha: String,
    pr_number: u64,
    pr_sha: String,
    runner_name: String,
}

fn targets() -> Option<Vec<Target>> {
    let Ok(path) = std::env::var("GITFORGE_CONTRACT_TARGETS") else {
        assert!(
            std::env::var("GITFORGE_CONTRACT_REQUIRED").as_deref() != Ok("1"),
            "GITFORGE_CONTRACT_REQUIRED=1 but GITFORGE_CONTRACT_TARGETS is not set"
        );
        eprintln!("skipping contract tests: GITFORGE_CONTRACT_TARGETS is not set");
        return None;
    };
    let text = std::fs::read_to_string(&path).expect("targets file");
    let targets: Vec<Target> = serde_json::from_str(&text).expect("targets JSON");
    assert!(!targets.is_empty(), "{path} lists no forges");
    Some(targets)
}

fn host_env(base_url: &str) -> String {
    let host = base_url.trim_start_matches("http://").trim_end_matches('/');
    gitforge_mcp::auth::default_env_var(host)
}

async fn client_for(
    target: &Target,
    repo: &str,
    token: &str,
) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(
        dir.path(),
        &format!("{}/gfadmin/{repo}.git", target.base_url),
    );
    let var = host_env(&target.base_url);
    let client = common::start(dir.path(), &[(var.as_str(), token)]).await;
    (client, dir)
}

async fn report_for(target: &Target, repo: &str, token: &str) -> Value {
    let (client, _dir) = client_for(target, repo, token).await;
    let report = common::forge_context(&client, false).await;
    client.cancel().await.unwrap();
    report
}

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// ci_status for `sha`, once every job has finished (the runner may still
/// be working through the workflow when the tests start).
async fn finished_status(client: &common::Client, sha: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    loop {
        let status = common::call(
            client,
            "ci_status",
            args(json!({ "sha": sha, "jobs": "all" })),
        )
        .await;
        let verdict = status["verdict"].as_str().unwrap_or_default().to_owned();
        if !matches!(verdict.as_str(), "pending" | "failing" | "no_runs")
            || std::time::Instant::now() > deadline
        {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn job_states(status: &Value) -> BTreeSet<String> {
    status["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["jobs"].as_array().into_iter().flatten())
        .map(|j| {
            format!(
                "{} {}",
                j["name"].as_str().unwrap_or("?"),
                j["state"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

fn available(report: &Value, feature: &str) -> bool {
    report["capabilities"]
        .as_array()
        .unwrap_or_else(|| panic!("no capabilities in {report}"))
        .iter()
        .find(|c| c["feature"] == feature)
        .unwrap_or_else(|| panic!("no {feature}"))["available"]
        .as_bool()
        .unwrap()
}

fn reason(report: &Value, feature: &str) -> String {
    report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == feature)
        .unwrap()["reason"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The server's API paths with parameter names erased, so Gitea's
/// `{run}` and Forgejo's `{run_id}` compare equal.
async fn swagger(base_url: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base_url}/swagger.v1.json"))
        .header("User-Agent", "gitforge-mcp-contract")
        .send()
        .await
        .expect("swagger reachable")
        .json()
        .await
        .expect("swagger JSON")
}

fn swagger_paths(spec: &Value) -> BTreeSet<String> {
    spec["paths"]
        .as_object()
        .expect("swagger paths")
        .keys()
        .map(|p| common::erase_params(p))
        .collect()
}

/// The endpoint each version-gated feature depends on. A feature is
/// supported when any of its paths is in the spec.
fn feature_paths(kind: &str, feature: &str) -> &'static [&'static str] {
    match (kind, feature) {
        (_, "ci_runs") => &["/repos/{}/{}/actions/runs"],
        (_, "ci_jobs") => &["/repos/{}/{}/actions/runs/{}/jobs"],
        (_, "ci_logs") => &["/repos/{}/{}/actions/jobs/{}/logs"],
        (_, "ci_cancel") => &["/repos/{}/{}/actions/runs/{}/cancel"],
        (_, "ci_rerun") => &["/repos/{}/{}/actions/runs/{}/rerun"],
        ("gitea", "ci_rerun_failed") => &["/repos/{}/{}/actions/runs/{}/rerun-failed-jobs"],
        ("forgejo", "ci_rerun_failed") => &[
            "/repos/{}/{}/actions/jobs/{}/rerun",
            "/repos/{}/{}/actions/runs/{}/jobs/{}/rerun",
        ],
        (_, "ci_dispatch") => &["/repos/{}/{}/actions/workflows/{}/dispatches"],
        (_, "ci_runners") => &["/repos/{}/{}/actions/runners"],
        ("gitea", "ci_queue") => &["/repos/{}/{}/actions/jobs"],
        ("forgejo", "ci_queue") => &["/repos/{}/{}/actions/runners/jobs"],
        (_, "repo_settings" | "repo_settings_read") => &["/repos/{}/{}/branch_protections/{}"],
        _ => &[],
    }
}

const SPEC_CHECKED: &[&str] = &[
    "ci_runs",
    "ci_jobs",
    "ci_logs",
    "ci_cancel",
    "ci_rerun",
    "ci_rerun_failed",
    "ci_dispatch",
    "ci_runners",
    "ci_queue",
    "repo_settings_read",
    "repo_settings",
];

#[tokio::test]
async fn forge_context_matches_every_forge() {
    let Some(targets) = targets() else { return };
    let mut failures = Vec::new();
    for target in &targets {
        eprintln!("== {}", target.name);
        let admin = report_for(target, "widgets", &target.admin_token).await;
        let check = |ok: bool, what: String, failures: &mut Vec<String>| {
            if !ok {
                failures.push(format!("{}: {what}", target.name));
            }
        };
        check(
            admin["status"] == "live",
            format!("admin status: {admin}"),
            &mut failures,
        );
        if admin["status"] != "live" {
            continue;
        }
        check(
            admin["forge"]["kind"] == target.kind.as_str(),
            format!("kind {}", admin["forge"]["kind"]),
            &mut failures,
        );
        check(
            admin["forge"]["version"] == target.version.as_str(),
            format!("version {}", admin["forge"]["version"]),
            &mut failures,
        );
        check(
            admin["user"]["login"] == "gfadmin",
            format!("user {}", admin["user"]),
            &mut failures,
        );
        check(
            admin["user"]["site_admin"] == true,
            "admin is not site admin".into(),
            &mut failures,
        );
        check(
            admin["repository"]["permissions"]["admin"] == true,
            "admin lacks repo admin".into(),
            &mut failures,
        );
        check(
            admin["repository"]["actions_enabled"] == true,
            "Actions not reported on".into(),
            &mut failures,
        );
        check(
            available(&admin, "repo_settings"),
            "repo_settings unavailable to admin".into(),
            &mut failures,
        );
        check(
            available(&admin, "pull_request_write"),
            "pull_request_write unavailable to admin".into(),
            &mut failures,
        );

        // The capability table against the server's own API spec.
        let spec = swagger(&target.base_url).await;
        let paths = swagger_paths(&spec);
        for feature in SPEC_CHECKED {
            let in_spec = feature_paths(&target.kind, feature)
                .iter()
                .any(|p| paths.contains(*p));
            let claimed = available(&admin, feature);
            check(
                claimed == in_spec,
                format!(
                    "{feature}: reported {} but the API spec {} it (reason: {})",
                    if claimed { "available" } else { "unavailable" },
                    if in_spec { "has" } else { "lacks" },
                    reason(&admin, feature)
                ),
                &mut failures,
            );
        }

        // Merge when checks succeed: an option of the merge body, and
        // DELETE on the merge endpoint to cancel.
        let merge = &spec["paths"]["/repos/{owner}/{repo}/pulls/{index}/merge"];
        let auto_in_spec = merge["delete"].is_object()
            && spec["definitions"]["MergePullRequestOption"]["properties"]
                ["merge_when_checks_succeed"]
                .is_object();
        check(
            available(&admin, "pull_request_auto_merge") == auto_in_spec,
            format!(
                "pull_request_auto_merge: reported {} but the API spec {} it",
                available(&admin, "pull_request_auto_merge"),
                if auto_in_spec { "has" } else { "lacks" }
            ),
            &mut failures,
        );

        let reader = report_for(target, "widgets", &target.reader_token).await;
        check(
            reader["status"] == "live",
            format!("reader status: {reader}"),
            &mut failures,
        );
        if reader["status"] == "live" {
            check(
                reader["user"]["login"] == "gfreader",
                format!("reader user {}", reader["user"]),
                &mut failures,
            );
            check(
                reader["repository"]["permissions"]["push"] == false,
                "reader can push".into(),
                &mut failures,
            );
            check(
                !available(&reader, "pull_request_write"),
                "reader offered pull_request_write".into(),
                &mut failures,
            );
            check(
                reason(&reader, "repo_settings") == "needs admin access to the repository",
                format!(
                    "reader repo_settings reason: {}",
                    reason(&reader, "repo_settings")
                ),
                &mut failures,
            );
            check(
                available(&reader, "pull_request_read"),
                "reader lacks pull_request_read".into(),
                &mut failures,
            );
        }

        let off = report_for(target, "no-actions", &target.admin_token).await;
        check(
            off["repository"]["actions_enabled"] == false,
            format!("no-actions repo: {}", off["repository"]),
            &mut failures,
        );
        check(
            reason(&off, "ci_runs") == "Actions is disabled for this repository",
            format!("no-actions ci_runs reason: {}", reason(&off, "ci_runs")),
            &mut failures,
        );

        // Its CI is outside the forge, in commit statuses on main. Its
        // admin has its settings too, which don't depend on Actions.
        let (client, _dir) = client_for(target, "no-actions", &target.admin_token).await;
        let tools = common::tool_names(&client).await;
        check(
            tools
                == [
                    "ci_status",
                    "ci_wait",
                    "forge_context",
                    "pull_request",
                    "repo_settings",
                ],
            format!("no-actions tools: {tools:?}"),
            &mut failures,
        );
        let external = common::try_call(
            &client,
            "ci_status",
            args(json!({ "branch": "main", "jobs": "all" })),
        )
        .await;
        check(
            external.as_ref().is_ok_and(|s| {
                s["verdict"] == "failure"
                    && s["runs"][0]["external"] == "ci/woodpecker/push"
                    && s["runs"][0]["jobs"].as_array().map(Vec::len) == Some(2)
            }),
            format!("CI from commit statuses: {external:?}"),
            &mut failures,
        );
        client.cancel().await.unwrap();
    }
    assert!(
        failures.is_empty(),
        "contract failures:\n  {}",
        failures.join("\n  ")
    );
}

#[tokio::test]
async fn ci_and_pull_request_tools_match_every_forge() {
    let Some(targets) = targets() else { return };
    let mut failures = Vec::new();
    let expected_jobs: BTreeSet<String> = [
        "passes success",
        "fails failure",
        "skipped skipped",
        "after success",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for target in &targets {
        eprintln!("== {} (CI tools)", target.name);
        let mut check = |ok: bool, what: String| {
            if !ok {
                failures.push(format!("{}: {what}", target.name));
            }
        };
        let (client, _dir) = client_for(target, "widgets", &target.admin_token).await;
        let tools: BTreeSet<String> = common::tool_names(&client).await.into_iter().collect();
        let logs_expected = report_for(target, "widgets", &target.admin_token).await;
        let logs_available = available(&logs_expected, "ci_logs");
        for tool in ["forge_context", "ci_status", "ci_runners", "pull_request"] {
            check(
                tools.contains(tool),
                format!("{tool} not offered to the admin: {tools:?}"),
            );
        }
        check(
            tools.contains("ci_logs") == logs_available,
            format!(
                "ci_logs offered={} but capability available={logs_available}",
                tools.contains("ci_logs")
            ),
        );

        // ci_status: the workflow's run, by commit, by run number, by branch.
        let status = finished_status(&client, &target.ci_sha).await;
        check(
            status["verdict"] == "failure",
            format!("verdict {} ({})", status["verdict"], status["summary"]),
        );
        check(
            job_states(&status) == expected_jobs,
            format!("jobs {:?}", job_states(&status)),
        );
        let number = status["runs"][0]["number"].as_u64().unwrap_or_default();
        let by_run = common::call(&client, "ci_status", args(json!({ "run": number }))).await;
        check(
            by_run["commit"] == target.ci_sha.as_str(),
            format!("run {number} resolved to {}", by_run["commit"]),
        );
        let by_branch = common::call(&client, "ci_status", args(json!({ "branch": "main" }))).await;
        check(
            by_branch["commit"] == target.ci_sha.as_str(),
            format!("main resolved to {}", by_branch["commit"]),
        );

        // ci_logs: failed jobs by default, the end of the log, markers intact.
        if logs_available {
            let logs =
                common::call(&client, "ci_logs", args(json!({ "sha": target.ci_sha }))).await;
            let jobs = logs["jobs"].as_array().cloned().unwrap_or_default();
            check(
                jobs.len() == 1 && jobs[0]["name"] == "fails",
                format!("default ci_logs jobs: {jobs:?}"),
            );
            let text = jobs
                .first()
                .map(|j| j["log"]["text"].to_string())
                .unwrap_or_default();
            check(
                text.contains("contract-fail-marker"),
                format!("fail log lacks its marker: {text}"),
            );
            let passing = common::call(
                &client,
                "ci_logs",
                args(json!({ "sha": target.ci_sha, "job": "passes", "grep": "pass-marker", "tail": 5 })),
            )
            .await;
            let text = passing["jobs"][0]["log"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            check(
                text.lines().all(|l| l.contains("contract-pass-marker")) && !text.is_empty(),
                format!("grep for the pass marker returned: {text:?}"),
            );
        }

        // ci_runners: the runner registered to the repository.
        let runners = common::call(&client, "ci_runners", args(json!({}))).await;
        let found = runners["runners"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["name"] == target.runner_name.as_str());
        match found {
            Some(r) => {
                check(
                    r["labels"] == json!(["contract"]),
                    format!("runner labels {}", r["labels"]),
                );
                check(
                    r["state"] != "offline",
                    format!("runner state {}", r["state"]),
                );
                check(
                    r["scope"] == "repository",
                    format!("runner scope {}", r["scope"]),
                );
            }
            None => check(
                false,
                format!(
                    "runner {} not listed: {}",
                    target.runner_name, runners["summary"]
                ),
            ),
        }

        // pull_request: the open PR, with its head commit's CI verdict.
        let list = common::call(&client, "pull_request", args(json!({}))).await;
        check(
            list["pull_requests"]
                .as_array()
                .is_some_and(|prs| prs.iter().any(|p| p["number"] == target.pr_number)),
            format!("PR #{} not listed: {list}", target.pr_number),
        );
        finished_status(&client, &target.pr_sha).await;
        let pr = common::call(
            &client,
            "pull_request",
            args(json!({ "action": "get", "number": target.pr_number })),
        )
        .await;
        check(
            pr["state"] == "open" && pr["head"] == "feature/contract",
            format!("PR {pr}"),
        );
        check(
            pr["head_sha"] == target.pr_sha.as_str(),
            format!("PR head {}", pr["head_sha"]),
        );
        check(
            pr["mergeable"] == true,
            format!("PR mergeable {}", pr["mergeable"]),
        );
        check(
            pr["ci"]["verdict"] == "failure",
            format!("PR ci {}", pr["ci"]),
        );
        check(
            pr["reviews"]["open_comments"] == 1,
            format!("PR reviews {}", pr["reviews"]),
        );

        // pull_request reviews: provision.sh left a comment review with a
        // comment on the pull request's one changed line.
        let reviews = common::call(
            &client,
            "pull_request",
            args(json!({ "action": "reviews", "number": target.pr_number })),
        )
        .await;
        let review = reviews["reviews"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| {
                r["body"]
                    .as_str()
                    .is_some_and(|b| b.contains("contract-review-marker"))
            });
        check(
            review.is_some_and(|r| r["state"] == "comment" && r["author"] == "gfadmin"),
            format!("review: {reviews}"),
        );
        let comment = review
            .and_then(|r| r["comments"].as_array())
            .and_then(|c| c.first());
        check(
            comment.is_some_and(|c| {
                c["path"] == "notes.txt"
                    && c["line"] == 1
                    && c["author"] == "gfadmin"
                    && c["body"]
                        .as_str()
                        .is_some_and(|b| b.contains("contract-comment-marker"))
                    && c["diff_hunk"]
                        .as_str()
                        .is_some_and(|h| h.contains("+a change for review"))
            }),
            format!("review comment: {reviews}"),
        );
        check(
            reviews["open_comments"] == 1 && reviews.get("omitted").is_none(),
            format!("open review comments: {reviews}"),
        );
        client.cancel().await.unwrap();

        // A read-only collaborator gets CI status but not runners.
        let (reader, _rdir) = client_for(target, "widgets", &target.reader_token).await;
        let reader_tools = common::tool_names(&reader).await;
        check(
            reader_tools.contains(&"ci_status".to_owned())
                && !reader_tools.contains(&"ci_runners".to_owned()),
            format!("reader tools {reader_tools:?}"),
        );
        reader.cancel().await.unwrap();
    }
    assert!(
        failures.is_empty(),
        "contract failures:\n  {}",
        failures.join("\n  ")
    );
}

/// The `shared` pool in `report`.
fn pool<'a>(report: &'a Value, label: &str) -> Option<&'a Value> {
    report["pools"]
        .as_array()?
        .iter()
        .find(|p| p["labels"] == json!([label]))
}

/// Each repository's (running, waiting) in a pool.
fn loads(pool: &Value) -> BTreeSet<(String, u64, u64)> {
    pool["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            (
                r["repo"].as_str().unwrap_or("?").to_owned(),
                r["running"].as_u64().unwrap_or(9),
                r["waiting"].as_u64().unwrap_or(9),
            )
        })
        .collect()
}

/// ci_runners in queue-a, once the shared runner has taken one `hold`,
/// which it does within seconds of the push.
async fn settled_runners(target: &Target) -> Value {
    let (client, _dir) = client_for(target, "queue-a", &target.admin_token).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let report = loop {
        let report = common::call(&client, "ci_runners", args(json!({}))).await;
        let settled =
            pool(&report, "shared").is_some_and(|p| p["running"] == 1 && p["waiting"] == 1);
        if settled || std::time::Instant::now() > deadline {
            break report;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    };
    client.cancel().await.unwrap();
    report
}

/// The repository whose (running, waiting) in `loads` is `want`.
fn repo_with(loads: &BTreeSet<(String, u64, u64)>, want: (u64, u64)) -> Option<String> {
    loads
        .iter()
        .find(|(_, running, waiting)| (*running, *waiting) == want)
        .map(|(repo, ..)| repo.clone())
}

/// What's wrong with the pools in `report`, and which repository's `hold`
/// waits and which holds the runner.
fn pool_failures(report: &Value) -> (Vec<String>, Option<(String, String)>) {
    let mut failures = Vec::new();
    let scopes: Vec<String> = report["job_scopes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            format!(
                "{} {}",
                s["scope"].as_str().unwrap_or("?"),
                s["outcome"].as_str().unwrap_or("?")
            )
        })
        .collect();
    if scopes != ["repository listed", "owner listed", "instance listed"] {
        failures.push(format!("job scopes {scopes:?}"));
    }
    let stranded = pool(report, "no-such-runner");
    let both_waiting = BTreeSet::from([
        ("gfadmin/queue-a".to_owned(), 0, 1),
        ("gfadmin/queue-b".to_owned(), 0, 1),
    ]);
    if !stranded.is_some_and(|p| {
        loads(p) == both_waiting
            && p["summary"]
                .as_str()
                .is_some_and(|s| s.contains("no visible runner carries these labels"))
    }) {
        failures.push(format!("stranded pool {stranded:?}"));
    }
    let Some(shared) = pool(report, "shared") else {
        failures.push(format!("no shared pool: {}", report["summary"]));
        return (failures, None);
    };
    let shared_loads = loads(shared);
    let roles = repo_with(&shared_loads, (0, 1)).zip(repo_with(&shared_loads, (1, 0)));
    if shared_loads.len() != 2
        || roles.is_none()
        || shared["runners_busy"] != 1
        || shared["runners_idle"] != 0
    {
        failures.push(format!("shared pool {shared}"));
    }
    (failures, roles)
}

/// What's wrong with how ci_status in `waiting_repo` describes its `hold`
/// job's wait: it should name `held_by`. Nothing to check before Forgejo
/// 16, whose jobs come from commit statuses named after the workflow, so a
/// wait can't be tied to one.
async fn wait_failure(target: &Target, waiting_repo: &str, held_by: &str) -> Option<String> {
    let name = waiting_repo.trim_start_matches("gfadmin/");
    if !available(
        &report_for(target, name, &target.admin_token).await,
        "ci_jobs",
    ) {
        return None;
    }
    let (client, _dir) = client_for(target, name, &target.admin_token).await;
    let status = common::call(&client, "ci_status", args(json!({ "branch": "main" }))).await;
    client.cancel().await.unwrap();
    let waits: Vec<String> = status["estimates"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["queue"].as_array().into_iter().flatten())
        .filter_map(|q| q["summary"].as_str().map(str::to_owned))
        .collect();
    let named = waits.iter().any(|w| {
        w.starts_with("hold waits for a runner: all 1 with labels shared are busy")
            && w.contains(&format!("running {held_by}"))
    });
    (!named).then(|| format!("{waiting_repo}'s waits: {waits:?}"))
}

/// scripts/contract/queue.yml on queue-a and queue-b: one instance runner
/// labelled `shared`, which one repository's `hold` job runs on while the
/// other's waits, and a `stranded` job in each that no runner can take.
/// ci_runners names both repositories in each pool, read through the
/// owner and instance scopes, and ci_status says who holds the runner the
/// waiting job wants.
#[tokio::test]
async fn pools_name_the_repositories_holding_runners() {
    let Some(targets) = targets() else { return };
    let mut failures = Vec::new();
    for target in &targets {
        eprintln!("== {} (runner pools)", target.name);
        let report = settled_runners(target).await;
        let (mut found, roles) = pool_failures(&report);
        if let Some((waiting_repo, held_by)) = roles {
            found.extend(wait_failure(target, &waiting_repo, &held_by).await);
        }
        failures.extend(found.into_iter().map(|f| format!("{}: {f}", target.name)));
    }
    assert!(
        failures.is_empty(),
        "contract failures:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn erases_parameter_names() {
    assert_eq!(
        common::erase_params("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs"),
        "/repos/{}/{}/actions/runs/{}/jobs"
    );
}
