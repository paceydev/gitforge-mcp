//! The real binary against a fake GitHub Enterprise Server: GitHub's API
//! shapes, and the CI verdict rules on them. Skipped jobs, check runs
//! from outside Actions, rerun attempts and runs awaiting approval; a log
//! behind a redirect; pull requests whose mergeability isn't known yet,
//! merged, scheduled through GraphQL and cancelled; runners; dispatch.
//!
//! github.com itself can't be faked (its API host is fixed), so these are
//! Enterprise Server's paths: `/api/v3` and `/api/graphql`.

mod common;

use serde_json::{Value, json};
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "test-token-value";
const REPO: &str = "/api/v3/repos/acme/widgets";

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Dev", "-c", "user.email=dev@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

async fn get(forge: &MockServer, at: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(forge)
        .await;
}

/// A GitHub Enterprise Server 3.19 holding acme/widgets, and a checkout of
/// it on `main` with one commit, whose sha is returned. The user and
/// repository answer only to `Authorization: Bearer <token>`.
async fn fake_github(dir: &std::path::Path, admin: bool) -> (MockServer, String) {
    fake_github_at(dir, admin, "3.19.2").await
}

async fn fake_github_at(dir: &std::path::Path, admin: bool, version: &str) -> (MockServer, String) {
    let forge = MockServer::start().await;
    let bearer = format!("Bearer {TOKEN}");
    get(
        &forge,
        "/api/v3/meta",
        json!({ "installed_version": version }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/user"))
        .and(header("authorization", bearer.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "dev" })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path(REPO))
        .and(header("authorization", bearer.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "acme/widgets", "default_branch": "main", "private": true,
            "archived": false, "allow_auto_merge": true, "delete_branch_on_merge": false,
            "permissions": { "admin": admin, "maintain": false, "push": true, "triage": true, "pull": true }
        })))
        .mount(&forge)
        .await;
    common::git_checkout(dir, &format!("{}/acme/widgets.git", forge.uri()));
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let host = forge.uri().trim_start_matches("http://").to_owned();
    common::write_config(dir, &format!("[hosts.\"{host}\"]\nkind = \"github\"\n"));
    let sha = git(dir, &["rev-parse", "HEAD"]);
    (forge, sha)
}

async fn start(dir: &std::path::Path, forge: &MockServer) -> common::Client {
    let var = format!("GITFORGE_TOKEN_127_0_0_1_{}", forge.address().port());
    common::start(dir, &[(var.as_str(), TOKEN)]).await
}

fn run(id: u64, sha: &str, status: &str, conclusion: Option<&str>, attempt: u64) -> Value {
    json!({
        "id": id, "name": "CI", "run_number": 12, "run_attempt": attempt, "event": "push",
        "status": status, "conclusion": conclusion, "head_branch": "main", "head_sha": sha,
        "path": ".github/workflows/ci.yml", "display_title": "one",
        "html_url": format!("https://github.example.com/acme/widgets/actions/runs/{id}"),
        "run_started_at": "2026-11-09T10:00:00Z", "updated_at": "2026-11-09T10:05:00Z"
    })
}

fn job(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({
        "id": id, "name": name, "status": status, "conclusion": conclusion,
        "started_at": "2026-11-09T10:00:00Z",
        "completed_at": if conclusion.is_some() { json!("2026-11-09T10:02:00Z") } else { Value::Null },
        "labels": ["ubuntu-latest"],
        "steps": [{"name": "Test", "number": 1, "status": status, "conclusion": conclusion}]
    })
}

/// CI on `sha`: its Actions runs, each run's jobs (latest attempts only),
/// its commit statuses and its check runs.
async fn commit_ci(
    forge: &MockServer,
    sha: &str,
    runs: Vec<Value>,
    jobs: &[(u64, Value)],
    statuses: Value,
    checks: Value,
) {
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/runs")))
        .and(query_param("head_sha", sha))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": runs.len(), "workflow_runs": runs })),
        )
        .mount(forge)
        .await;
    for (id, list) in jobs {
        Mock::given(method("GET"))
            .and(path(format!("{REPO}/actions/runs/{id}/jobs")))
            .and(query_param("filter", "latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "total_count": list.as_array().map_or(0, Vec::len), "jobs": list }),
            ))
            .mount(forge)
            .await;
    }
    get(forge, &format!("{REPO}/commits/{sha}/statuses"), statuses).await;
    get(
        forge,
        &format!("{REPO}/commits/{sha}/check-runs"),
        json!({ "total_count": checks.as_array().map_or(0, Vec::len), "check_runs": checks }),
    )
    .await;
}

fn check(name: &str, app: &str, conclusion: Option<&str>) -> Value {
    json!({
        "id": 1, "name": name, "status": if conclusion.is_some() { "completed" } else { "in_progress" },
        "conclusion": conclusion, "details_url": "https://ci.example.com/build/1",
        "app": { "slug": app.to_lowercase().replace(' ', "-"), "name": app }
    })
}

#[tokio::test]
async fn detects_github_enterprise_and_offers_every_tool() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), true).await;
    let client = start(dir.path(), &forge).await;

    let report = common::forge_context(&client, false).await;
    assert_eq!(report["status"], "live", "{report}");
    assert_eq!(report["forge"]["kind"], "github");
    assert_eq!(report["forge"]["version"], "3.19.2");
    assert_eq!(report["user"]["login"], "dev");
    let caps = report["capabilities"].as_array().unwrap();
    let unavailable: Vec<&str> = caps
        .iter()
        .filter(|c| c["available"] == false)
        .map(|c| c["feature"].as_str().unwrap())
        .collect();
    assert_eq!(unavailable, ["ci_log_tail"], "{report}");
    let mut tools = common::tool_names(&client).await;
    tools.sort();
    assert_eq!(
        tools,
        [
            "ci_control",
            "ci_estimate",
            "ci_logs",
            "ci_runners",
            "ci_status",
            "ci_wait",
            "forge_context",
            "pull_request",
            "repo_settings"
        ]
    );
    assert!(!report.to_string().contains(TOKEN));
    client.cancel().await.unwrap();
}

/// A run rerun once, whose deploy job was skipped by its `if:`, beside
/// Actions' own check runs (one from the first, failed attempt) and an
/// outside CI that reports through check runs and a commit status.
#[tokio::test]
async fn verdicts_hold_for_reruns_skips_and_outside_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), false).await;
    commit_ci(
        &forge,
        &sha,
        vec![run(9001, &sha, "completed", Some("success"), 2)],
        &[(9001, json!([job(1, "test", "completed", Some("success")), job(2, "prod deploy", "completed", Some("skipped"))]))],
        json!([{ "id": 3, "state": "success", "context": "ci/jenkins/build", "description": "ok",
                 "target_url": "https://jenkins.example.com/1", "created_at": "2026-11-09T10:00:00Z",
                 "updated_at": "2026-11-09T10:03:00Z" }]),
        json!([check("test", "GitHub Actions", Some("failure")), check("lint", "Acme CI", Some("success"))]),
    )
    .await;
    let client = start(dir.path(), &forge).await;

    let status = common::call(&client, "ci_status", args(json!({"jobs": "all"}))).await;
    assert_eq!(status["verdict"], "success", "{status}");
    assert_eq!(
        status["summary"], "3 runs, 4 jobs: 3 success, 1 skipped (prod deploy)",
        "{status}"
    );
    let runs = status["runs"].as_array().unwrap();
    assert_eq!(
        (runs[0]["number"].clone(), runs[0]["attempt"].clone()),
        (json!(9001), json!(2))
    );
    let systems: Vec<&str> = runs[1..]
        .iter()
        .map(|r| r["external"].as_str().unwrap())
        .collect();
    assert_eq!(systems, ["ci/jenkins", "Acme CI"]);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_outside_check_failing_fails_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), false).await;
    commit_ci(
        &forge,
        &sha,
        vec![run(9002, &sha, "completed", Some("success"), 1)],
        &[(9002, json!([job(1, "test", "completed", Some("success"))]))],
        json!([]),
        json!([check("lint", "Acme CI", Some("failure"))]),
    )
    .await;
    let client = start(dir.path(), &forge).await;

    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["verdict"], "failure", "{status}");
    let logs = common::call(&client, "ci_logs", Default::default()).await;
    assert!(
        logs["notes"]
            .to_string()
            .contains("https://ci.example.com/build/1"),
        "{logs}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn every_job_skipped_is_nothing_ran_and_approval_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), false).await;
    commit_ci(
        &forge,
        &sha,
        vec![
            run(9003, &sha, "completed", Some("skipped"), 1),
            run(9004, &sha, "completed", Some("action_required"), 1),
        ],
        &[
            (9003, json!([job(1, "docs", "completed", Some("skipped"))])),
            (9004, json!([])),
        ],
        json!([]),
        json!([]),
    )
    .await;
    // A run is asked for by the id in its URL.
    get(
        &forge,
        &format!("{REPO}/actions/runs/9003"),
        run(9003, &sha, "completed", Some("skipped"), 1),
    )
    .await;
    let client = start(dir.path(), &forge).await;

    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["verdict"], "pending", "{status}");
    assert!(
        status["progress"]
            .to_string()
            .contains("waiting for a maintainer to approve the run"),
        "{status}"
    );
    let skipped = common::call(&client, "ci_status", args(json!({"run": 9003}))).await;
    assert_eq!(skipped["verdict"], "nothing_ran", "{skipped}");
    client.cancel().await.unwrap();
}

/// GitHub answers a log request with a redirect to storage on another
/// host. The log arrives; the credential doesn't go with it.
#[tokio::test]
async fn logs_follow_the_redirect_without_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), false).await;
    let storage = MockServer::start().await;
    commit_ci(
        &forge,
        &sha,
        vec![run(9005, &sha, "in_progress", None, 1)],
        &[(
            9005,
            json!([
                job(7, "build", "completed", Some("failure")),
                job(8, "test", "in_progress", None)
            ]),
        )],
        json!([]),
        json!([]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/jobs/7/logs")))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/logs/7.txt?sig=x", storage.uri())),
        )
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/logs/7.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "2026-11-09T10:01:00.0000000Z compiling\n2026-11-09T10:01:59.0000000Z error[E0425]: not found\n",
        ))
        .mount(&storage)
        .await;
    let client = start(dir.path(), &forge).await;

    let logs = common::call(&client, "ci_logs", args(json!({"all": true}))).await;
    let jobs = logs["jobs"].as_array().unwrap();
    assert!(
        jobs[0]["log"].to_string().contains("error[E0425]"),
        "{logs}"
    );
    assert!(
        jobs[1]["log"]["error"]
            .as_str()
            .unwrap()
            .contains("once the job has finished"),
        "{logs}"
    );
    for request in storage.received_requests().await.unwrap() {
        assert!(
            !request.headers.contains_key("authorization"),
            "{request:?}"
        );
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn runners_say_what_github_does_not_list() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), true).await;
    get(
        &forge,
        &format!("{REPO}/actions/runners"),
        json!({ "total_count": 1, "runners": [{ "id": 3, "name": "mac", "os": "macOS",
            "status": "online", "busy": false,
            "labels": [{"id": 1, "name": "self-hosted", "type": "read-only"}, {"id": 2, "name": "macOS", "type": "read-only"}] }] }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/orgs/acme/actions/runners"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "message": "You must be an org admin to list runners"
        })))
        .mount(&forge)
        .await;
    let mut deploy = job(12, "deploy", "queued", None);
    deploy["labels"] = json!(["self-hosted", "linux"]);
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/runs")))
        .and(query_param("status", "queued"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 1, "workflow_runs": [run(9006, &sha, "queued", None, 1)]
        })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/runs")))
        .and(query_param("status", "in_progress"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": 0, "workflow_runs": [] })),
        )
        .mount(&forge)
        .await;
    get(
        &forge,
        &format!("{REPO}/actions/runs/9006/jobs"),
        json!({ "jobs": [job(11, "build", "queued", None), deploy] }),
    )
    .await;
    let client = start(dir.path(), &forge).await;

    let report = common::call(&client, "ci_runners", Default::default()).await;
    let scopes = report["scopes"].as_array().unwrap();
    assert_eq!(scopes[0]["outcome"], "listed", "{report}");
    assert!(
        scopes[1]["outcome"]
            .as_str()
            .unwrap()
            .starts_with("not_permitted: You must be an org admin")
    );
    assert!(
        scopes[2]["outcome"]
            .as_str()
            .unwrap()
            .contains("GitHub-hosted runners")
    );
    assert_eq!(report["waiting"][0]["hosted"], true, "{report}");
    assert!(report["waiting"][1].get("hosted").is_none(), "{report}");
    assert!(
        report["summary"]
            .as_str()
            .unwrap()
            .ends_with("2 jobs waiting; 1 of them for hosted runners, which aren't listed; 1 of them match no visible online runner"),
        "{report}"
    );
    client.cancel().await.unwrap();
}

/// Pull request #7 from feature-x at `head`: GitHub doesn't yet know
/// whether it can merge, then does; once merged, it says so.
async fn mount_pull_request(forge: &MockServer, head: &str, auto_merge: bool) {
    let pr = |mergeable: Value, merged: bool| {
        json!({
            "number": 7, "node_id": "PR_kwDOAcme7", "title": "Add x",
            "state": if merged { "closed" } else { "open" }, "merged": merged,
            "mergeable": mergeable, "draft": false, "user": {"login": "dev"},
            "head": {"ref": "feature-x", "sha": head, "repo": {"full_name": "acme/widgets"}},
            "base": {"ref": "main", "sha": "c".repeat(40), "repo": {"full_name": "acme/widgets"}},
            "html_url": "https://github.example.com/acme/widgets/pull/7",
            "merged_at": if merged { json!("2026-11-09T11:00:00Z") } else { Value::Null },
            "merge_commit_sha": "d".repeat(40),
            "auto_merge": if auto_merge { json!({"merge_method": "squash"}) } else { Value::Null }
        })
    };
    let at = format!("{REPO}/pulls/7");
    for (body, times) in [(pr(Value::Null, false), 1), (pr(json!(true), false), 1)] {
        Mock::given(method("GET"))
            .and(path(at.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(times)
            .with_priority(1)
            .mount(forge)
            .await;
    }
    get(forge, &at, pr(json!(true), !auto_merge)).await;
}

#[tokio::test]
async fn merging_waits_for_mergeability_and_deletes_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), false).await;
    let head = "a".repeat(40);
    mount_pull_request(&forge, &head, false).await;
    commit_ci(
        &forge,
        &head,
        vec![run(9007, &head, "completed", Some("success"), 1)],
        &[(9007, json!([job(1, "test", "completed", Some("success"))]))],
        json!([]),
        json!([]),
    )
    .await;
    Mock::given(method("PUT"))
        .and(path(format!("{REPO}/pulls/7/merge")))
        .and(body_partial_json(
            json!({"merge_method": "squash", "sha": head}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"merged": true, "sha": "d".repeat(40)})),
        )
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{REPO}/git/refs/heads/feature-x")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&forge)
        .await;
    let client = start(dir.path(), &forge).await;

    let merged = common::call(
        &client,
        "pull_request",
        args(json!({"action": "merge", "number": 7, "expected_head_sha": head, "style": "squash", "delete_branch": true})),
    )
    .await;
    assert_eq!(merged["merged"], true, "{merged}");
    assert_eq!(merged["branch_deleted"], true, "{merged}");
    assert_eq!(merged["merge_commit_sha"], "d".repeat(40));
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn merge_when_ready_goes_through_graphql_and_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), false).await;
    let head = "b".repeat(40);
    mount_pull_request(&forge, &head, true).await;
    commit_ci(
        &forge,
        &head,
        vec![run(9008, &head, "in_progress", None, 1)],
        &[(9008, json!([job(1, "test", "in_progress", None)]))],
        json!([]),
        json!([]),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/branches/main"),
        json!({ "name": "main", "commit": {"sha": "c".repeat(40)}, "protected": true,
                "protection": {"enabled": true, "required_status_checks": {
                    "enforcement_level": "non_admins", "contexts": ["test"], "checks": [{"context": "test"}]}} }),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/rules/branches/main"),
        json!([{"type": "required_status_checks", "parameters": {
            "strict_required_status_checks_policy": false,
            "required_status_checks": [{"context": "Acme CI / lint"}]}}]),
    )
    .await;
    for (mutation, variables) in [
        (
            "enablePullRequestAutoMerge",
            json!({"id": "PR_kwDOAcme7", "method": "SQUASH", "head": head}),
        ),
        ("disablePullRequestAutoMerge", json!({"id": "PR_kwDOAcme7"})),
    ] {
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .and(body_string_contains(mutation))
            .and(body_partial_json(json!({ "variables": variables })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .expect(1)
            .mount(&forge)
            .await;
    }
    let client = start(dir.path(), &forge).await;

    let scheduled = common::call(
        &client,
        "pull_request",
        args(json!({"action": "merge_when_ready", "number": 7, "expected_head_sha": head, "style": "squash"})),
    )
    .await;
    assert_eq!(scheduled["scheduled"], true, "{scheduled}");
    assert_eq!(
        scheduled["required_checks"],
        json!(["test", "Acme CI / lint"])
    );
    let refused = common::try_call(
        &client,
        "pull_request",
        args(json!({"action": "merge_when_ready", "number": 7, "delete_branch": true})),
    )
    .await;
    assert!(refused.is_err_and(|e| e.contains("Automatically delete head branches")));
    let cancelled = common::call(
        &client,
        "pull_request",
        args(json!({"action": "cancel_auto_merge", "number": 7})),
    )
    .await;
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    // A style GitHub doesn't have is refused before anything is sent.
    let refused = common::try_call(
        &client,
        "pull_request",
        args(json!({"action": "merge", "number": 7, "style": "fast-forward-only", "require_ci": false})),
    )
    .await;
    assert!(refused.is_err_and(|e| e.contains("merge, squash or rebase")));
    let requests = forge.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method.as_str() == "PUT"));
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn cancel_and_dispatch_name_runs_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), false).await;
    commit_ci(
        &forge,
        &sha,
        vec![run(9009, &sha, "in_progress", None, 1)],
        &[(9009, json!([job(1, "test", "in_progress", None)]))],
        json!([]),
        json!([]),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/actions/runs/9009"),
        run(9009, &sha, "completed", Some("cancelled"), 1),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("{REPO}/actions/runs/9009/cancel")))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({})))
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{REPO}/actions/workflows/release.yml/dispatches"
        )))
        .and(body_partial_json(
            json!({"ref": "main", "inputs": {"level": "minor"}}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&forge)
        .await;
    let client = start(dir.path(), &forge).await;

    let cancelled = common::call(&client, "ci_control", args(json!({"action": "cancel"}))).await;
    assert_eq!(cancelled["run"], 9009, "{cancelled}");
    assert_eq!(cancelled["run_now"]["state"], "cancelled", "{cancelled}");
    // GHES 3.19 predates dispatch's run details, so none is asked for.
    let dispatched = common::call(
        &client,
        "ci_control",
        args(
            json!({"action": "dispatch", "workflow": "release.yml", "inputs": {"level": "minor"}}),
        ),
    )
    .await;
    assert!(
        dispatched["notes"]
            .to_string()
            .contains("doesn't say which run"),
        "{dispatched}"
    );
    let requests = forge.received_requests().await.unwrap();
    let dispatch = requests
        .iter()
        .find(|r| r.url.path().ends_with("/dispatches"))
        .unwrap();
    assert!(!String::from_utf8_lossy(&dispatch.body).contains("return_run_details"));
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn a_rate_limit_says_when_it_lifts() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), false).await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/runs")))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1794360000")
                .set_body_json(json!({"message": "API rate limit exceeded for user ID 1."})),
        )
        .mount(&forge)
        .await;
    let client = start(dir.path(), &forge).await;

    let status = common::try_call(&client, "ci_status", Default::default()).await;
    let error = status.unwrap_err();
    assert!(error.contains("rate-limiting"), "{error}");
    assert!(error.contains("01:20 UTC"), "{error}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn dispatch_reports_the_run_it_started_from_3_21() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github_at(dir.path(), false, "3.21.0").await;
    Mock::given(method("POST"))
        .and(path(format!(
            "{REPO}/actions/workflows/release.yml/dispatches"
        )))
        .and(body_partial_json(
            json!({"ref": "main", "return_run_details": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "workflow_run_id": 9010,
            "run_url": "https://github.example.com/api/v3/repos/acme/widgets/actions/runs/9010",
            "html_url": "https://github.example.com/acme/widgets/actions/runs/9010"
        })))
        .expect(1)
        .mount(&forge)
        .await;
    let client = start(dir.path(), &forge).await;

    let dispatched = common::call(
        &client,
        "ci_control",
        args(json!({"action": "dispatch", "workflow": "release.yml"})),
    )
    .await;
    assert_eq!(dispatched["started"]["run_number"], 9010, "{dispatched}");
    assert_eq!(dispatched["notes"], json!([]), "{dispatched}");
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn reviews_read_resolution_from_graphql() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), false).await;
    let head = "e".repeat(40);
    mount_pull_request(&forge, &head, false).await;
    get(
        &forge,
        &format!("{REPO}/pulls/7/reviews"),
        json!([
            {"id": 20, "user": {"login": "alice"}, "state": "CHANGES_REQUESTED", "body": "Two things.",
             "commit_id": head, "submitted_at": "2026-11-09T10:00:00Z"},
            {"id": 21, "user": {"login": "bob"}, "state": "APPROVED", "body": "",
             "commit_id": "f".repeat(40), "submitted_at": "2026-11-09T10:05:00Z"}
        ]),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/pulls/7/comments"),
        json!([
            {"id": 50, "pull_request_review_id": 20, "user": {"login": "alice"}, "body": "Rename this.",
             "path": "src/lib.rs", "line": 3, "side": "RIGHT", "diff_hunk": "@@ -1 +1,3 @@\n+a\n+b\n+c"},
            {"id": 51, "pull_request_review_id": 20, "user": {"login": "alice"}, "body": "And this.",
             "path": "src/lib.rs", "line": 9, "side": "RIGHT", "diff_hunk": "@@ -1 +1,9 @@\n+i"}
        ]),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/pulls/7/requested_reviewers"),
        json!({"users": [], "teams": []}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .and(body_string_contains("reviewThreads"))
        .and(body_partial_json(json!({"variables": {"owner": "acme", "name": "widgets", "number": 7}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {
            "reviewThreads": {"nodes": [
                {"isResolved": true, "resolvedBy": {"login": "dev"}, "comments": {"nodes": [{"databaseId": 50}]}},
                {"isResolved": false, "resolvedBy": null, "comments": {"nodes": [{"databaseId": 51}]}}
            ]}
        }}}})))
        .expect(1)
        .mount(&forge)
        .await;
    let client = start(dir.path(), &forge).await;

    let reviews = common::call(
        &client,
        "pull_request",
        args(json!({"action": "reviews", "number": 7})),
    )
    .await;
    let text = reviews.to_string();
    assert!(
        text.contains("\"changes_requested_by\":[\"alice\"]"),
        "{reviews}"
    );
    assert!(text.contains("\"open_comments\":1"), "{reviews}");
    assert!(
        text.contains("And this.") && !text.contains("Rename this."),
        "{reviews}"
    );
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn a_running_run_gets_an_eta_from_its_workflow_history_and_a_queue_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, sha) = fake_github(dir.path(), true).await;
    let started = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
    let mut running = run(9020, &sha, "in_progress", None, 1);
    running["run_started_at"] = json!(started);
    let mut deploy = job(22, "deploy", "queued", None);
    deploy["labels"] = json!(["self-hosted", "linux"]);
    let jobs = json!([job(21, "build", "completed", Some("success")), deploy]);
    commit_ci(
        &forge,
        &sha,
        vec![running.clone()],
        &[(9020, jobs)],
        json!([]),
        json!([]),
    )
    .await;
    // Four passing runs of ci.yml, five minutes each, that built and
    // deployed.
    let history: Vec<Value> = (9010..9014)
        .rev()
        .map(|id| run(id, &"b".repeat(40), "completed", Some("success"), 1))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/actions/workflows/ci.yml/runs")))
        .and(query_param("branch", "main"))
        .and(query_param("event", "push"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": history.len(), "workflow_runs": history })),
        )
        .mount(&forge)
        .await;
    for id in 9010..9014u64 {
        get(
            &forge,
            &format!("{REPO}/actions/runs/{id}/jobs"),
            json!({ "jobs": [job(id * 10, "build", "completed", Some("success")),
                             job(id * 10 + 1, "deploy", "completed", Some("success"))] }),
        )
        .await;
    }
    // The only runner is a Mac: nothing can take deploy.
    get(
        &forge,
        &format!("{REPO}/actions/runners"),
        json!({ "total_count": 1, "runners": [{ "id": 3, "name": "mac", "os": "macOS",
            "status": "online", "busy": false,
            "labels": [{"id": 1, "name": "self-hosted"}, {"id": 2, "name": "macOS"}] }] }),
    )
    .await;
    for (status, runs) in [("queued", json!([])), ("in_progress", json!([running]))] {
        Mock::given(method("GET"))
            .and(path(format!("{REPO}/actions/runs")))
            .and(query_param("status", status))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": runs })),
            )
            .mount(&forge)
            .await;
    }
    let client = start(dir.path(), &forge).await;

    let status = common::call(&client, "ci_status", Default::default()).await;
    let eta = &status["estimates"][0];
    assert_eq!(eta["pass"]["median_seconds"], 300, "{status}");
    assert_eq!(eta["queue"][0]["job"], "deploy", "{status}");
    assert!(
        status["progress"]
            .as_str()
            .unwrap()
            .contains("2m in; a pass usually takes 5m")
            && status["progress"].as_str().unwrap().contains(
                "deploy waits for a runner with labels self-hosted, linux: none is online"
            ),
        "{status}"
    );
    client.cancel().await.unwrap();
}

/// What `forge` received with `verb` at `to`, as JSON.
async fn bodies(forge: &MockServer, verb: &str, to: &str) -> Vec<Value> {
    forge
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == to)
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

/// Classic branch protection is replaced whole, so an edit must send back
/// everything it wasn't asked to change, and a revert the rule as it was.
/// A webhook's secret goes to its config endpoint alone, and never back.
#[tokio::test]
async fn settings_keep_what_they_dont_change_on_github() {
    let dir = tempfile::tempdir().unwrap();
    let (forge, _) = fake_github(dir.path(), true).await;
    get(
        &forge,
        &format!("{REPO}/branches"),
        json!([{ "name": "main", "protected": true }]),
    )
    .await;
    get(
        &forge,
        &format!("{REPO}/branches/main/protection"),
        json!({
            "required_status_checks": {"strict": true, "contexts": ["test"], "checks": [{"context": "test", "app_id": 15368}]},
            "enforce_admins": {"enabled": true},
            "required_pull_request_reviews": {"dismiss_stale_reviews": false, "require_code_owner_reviews": true, "required_approving_review_count": 1},
            "restrictions": null,
            "required_signatures": {"enabled": false},
            "required_linear_history": {"enabled": true},
        }),
    )
    .await;
    for verb in ["PUT", "DELETE"] {
        Mock::given(method(verb))
            .and(path(format!("{REPO}/branches/main/protection")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&forge)
            .await;
    }
    Mock::given(method("DELETE"))
        .and(path(format!(
            "{REPO}/branches/main/protection/required_signatures"
        )))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&forge)
        .await;
    get(
        &forge,
        &format!("{REPO}/hooks"),
        json!([{ "id": 5, "name": "web", "active": true, "events": ["push"],
                 "config": {"url": "https://ci.example.com/hook", "content_type": "json", "secret": "********", "insecure_ssl": "0"} }]),
    )
    .await;
    Mock::given(method("PATCH"))
        .and(path(format!("{REPO}/hooks/5/config")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&forge)
        .await;
    get(
        &forge,
        &format!("{REPO}/hooks/5/deliveries"),
        json!([{ "id": 42, "guid": "g", "delivered_at": "2026-11-13T01:00:00Z", "redelivery": false,
                 "duration": 0.31, "status": "OK", "status_code": 200, "event": "push", "action": null }]),
    )
    .await;
    let client = start(dir.path(), &forge).await;

    let changes =
        json!({"set": [{"pattern": "main", "required_approvals": 2, "enforce_admins": false}]});
    let plan = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "plan", "section": "branch_protection", "changes": changes})),
    )
    .await;
    let applied = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "branch_protection", "changes": changes, "fingerprint": plan["fingerprint"]})),
    )
    .await;
    assert_eq!(applied["applied"], true, "{applied}");
    let put = &bodies(&forge, "PUT", &format!("{REPO}/branches/main/protection")).await[0];
    assert_eq!(
        put["required_pull_request_reviews"]["required_approving_review_count"],
        2
    );
    assert_eq!(
        put["required_pull_request_reviews"]["require_code_owner_reviews"],
        true
    );
    assert_eq!(put["enforce_admins"], false);
    assert_eq!(put["required_linear_history"], true);
    assert_eq!(
        put["required_status_checks"]["checks"],
        json!([{"context": "test", "app_id": 15368}])
    );

    let reverted = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "revert", "revert": applied["revert"]})),
    )
    .await;
    assert_eq!(reverted["reverted"], true, "{reverted}");
    let back = &bodies(&forge, "PUT", &format!("{REPO}/branches/main/protection")).await[1];
    assert_eq!(
        back["required_pull_request_reviews"]["required_approving_review_count"],
        1
    );
    assert_eq!(back["enforce_admins"], true);

    let secret = "gh-hook-secret-value";
    let update = json!({"update": [{"id": 5, "secret": secret}]});
    let plan = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "plan", "section": "webhooks", "changes": update})),
    )
    .await;
    let applied = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "webhooks", "changes": update, "fingerprint": plan["fingerprint"]})),
    )
    .await;
    assert_eq!(
        bodies(&forge, "PATCH", &format!("{REPO}/hooks/5/config")).await,
        vec![json!({"secret": secret})]
    );
    assert!(
        bodies(&forge, "PATCH", &format!("{REPO}/hooks/5"))
            .await
            .is_empty()
    );
    for reply in [&plan, &applied] {
        assert!(!reply.to_string().contains(secret) && !reply.to_string().contains("****"));
    }

    let deliveries = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "webhook_deliveries", "webhook_id": 5})),
    )
    .await;
    assert_eq!(
        deliveries["deliveries"][0]["status_code"], 200,
        "{deliveries}"
    );
    client.cancel().await.unwrap();
}
