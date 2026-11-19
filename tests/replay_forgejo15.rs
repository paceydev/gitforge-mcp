//! Real Forgejo 15 API snapshots, replayed through the real binary.
//!
//! Forgejo 15 has no jobs endpoint, so a job list has to be pieced together
//! from commit statuses and runner tasks, and that is where status checks
//! go wrong. These snapshots (tests/fixtures/forgejo15, names replaced)
//! record the cases that once produced a confident wrong answer:
//!
//! - real-2026-10-02: one job green, the other still queued with no runner
//!   task, which a task-based check called a pass; and a red commit called
//!   "nothing ran".
//! - all-skipped: every job skipped, whose statuses all say `success`.
//! - no-statuses: a run with no job listed anywhere yet.
//! - matrix-name: a job name with parentheses of its own.
//! - the rest: the steps of a push from queued to green or cancelled.

mod common;

use std::path::Path;

use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "replay-token";
const E8: &str = "e8b5aecb9e638a95c745fa43a30922673cf00c37";
const E4: &str = "e4bb8b810046d5fc4cb5dc9af968eaf54b33a88a";
const B0: &str = "0b4ed9815b7780b26e6b6c35133fb07cd4799089";
const REPO: &str = "/api/v1/repos/acme/widgets";

async fn json_mock(server: &MockServer, p: &str, query: &[(&str, &str)], body: Value) {
    let mut mock = Mock::given(method("GET")).and(path(p));
    for (k, v) in query {
        mock = mock.and(query_param(*k, *v));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A Forgejo 15 serving one snapshot directory.
async fn replay(snapshot: &str) -> MockServer {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/forgejo15")
        .join(snapshot);
    let server = MockServer::builder().start().await;
    json_mock(
        &server,
        "/api/forgejo/v1/version",
        &[],
        json!({"version": "15.0.11+gitea-1.22.0"}),
    )
    .await;
    json_mock(
        &server,
        "/api/v1/user",
        &[],
        json!({"login": "dev", "is_admin": false}),
    )
    .await;
    json_mock(
        &server,
        REPO,
        &[],
        json!({
            "full_name": "acme/widgets", "default_branch": "main", "private": true,
            "archived": false, "has_actions": true,
            "permissions": {"admin": false, "push": true, "pull": true}
        }),
    )
    .await;
    // Nothing beyond the first page in these snapshots.
    json_mock(
        &server,
        &format!("{REPO}/actions/tasks"),
        &[("page", "2")],
        json!({"workflow_runs": []}),
    )
    .await;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let file = entry.unwrap().path();
        let name = file.file_name().unwrap().to_str().unwrap().to_owned();
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        if let Some(sha) = name
            .strip_prefix("runs-")
            .and_then(|n| n.strip_suffix(".json"))
        {
            json_mock(
                &server,
                &format!("{REPO}/actions/runs"),
                &[("head_sha", sha)],
                body,
            )
            .await;
        } else if let Some(sha) = name
            .strip_prefix("statuses-")
            .and_then(|n| n.strip_suffix(".json"))
        {
            json_mock(
                &server,
                &format!("{REPO}/commits/{sha}/statuses"),
                &[("page", "1")],
                body,
            )
            .await;
        } else if name == "tasks.json" {
            json_mock(
                &server,
                &format!("{REPO}/actions/tasks"),
                &[("page", "1")],
                body,
            )
            .await;
        }
    }
    server
}

/// ci_status for `sha` against `snapshot`.
async fn ci_status(snapshot: &str, sha: &str) -> Value {
    let forge = replay(snapshot).await;
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let var = format!("GITFORGE_TOKEN_127_0_0_1_{}", forge.address().port());
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;
    let mut args = serde_json::Map::new();
    args.insert("sha".into(), json!(sha));
    args.insert("jobs".into(), json!("all"));
    let result = common::call(&client, "ci_status", args).await;
    client.cancel().await.unwrap();
    result
}

/// Every job as "name state task", in run order, for compact assertions.
fn jobs(status: &Value) -> Vec<String> {
    status["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("no runs in {status}"))
        .iter()
        .flat_map(|r| r["jobs"].as_array().unwrap().iter())
        .map(|j| {
            format!(
                "{} {} {}",
                j["name"].as_str().unwrap(),
                j["state"].as_str().unwrap(),
                j["task_id"].as_u64().map_or("-".into(), |t| t.to_string())
            )
        })
        .collect()
}

fn check(status: &Value, verdict: &str, expected_jobs: &[&str]) {
    assert_eq!(status["verdict"], verdict, "{status:#}");
    let got = jobs(status);
    for want in expected_jobs {
        assert!(
            got.iter().any(|g| g == want),
            "missing job `{want}` in {got:?}"
        );
    }
}

#[tokio::test]
async fn a_queued_job_without_a_task_is_not_a_pass() {
    // The 2026-10-02 false pass: frontend green, macos still waiting.
    let s = ci_status("real-2026-10-02", E8).await;
    check(&s, "pending", &["frontend success 974", "macos waiting -"]);
}

#[tokio::test]
async fn a_failed_job_is_a_failure_not_nothing_ran() {
    let s = ci_status("real-2026-10-02", E4).await;
    check(
        &s,
        "failure",
        &["frontend failure 918", "macos success 920"],
    );
}

#[tokio::test]
async fn a_second_run_of_the_same_commit_still_running() {
    let s = ci_status("real-2026-10-02", B0).await;
    check(&s, "pending", &["macos success 956", "macos running 975"]);
    assert_eq!(s["runs"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn both_runs_green() {
    let s = ci_status("green-0b4ed98", B0).await;
    check(
        &s,
        "success",
        &[
            "frontend success 955",
            "macos success 956",
            "frontend success 973",
            "macos success 975",
        ],
    );
}

#[tokio::test]
async fn a_job_behind_needs_is_blocked() {
    let s = ci_status("blocked", E8).await;
    check(&s, "pending", &["frontend running 974", "macos blocked -"]);
}

#[tokio::test]
async fn every_job_skipped_is_nothing_ran() {
    let s = ci_status("all-skipped", E8).await;
    check(&s, "nothing_ran", &["macos skipped -"]);
    assert!(s["notes"].to_string().contains("not a pass"));
}

#[tokio::test]
async fn a_matrix_job_name_keeps_its_parentheses() {
    let s = ci_status("matrix-name", E8).await;
    check(
        &s,
        "pending",
        &["frontend success 974", "test (pg 17, arm) waiting -"],
    );
}

#[tokio::test]
async fn a_run_with_no_statuses_yet_cannot_pass() {
    let s = ci_status("no-statuses", E8).await;
    check(
        &s,
        "pending",
        &["frontend success 974", "(jobs not yet listed) waiting -"],
    );
}

#[tokio::test]
async fn queued_then_running_then_green() {
    check(
        &ci_status("queued-then-green/0", E8).await,
        "pending",
        &["macos waiting -"],
    );
    check(
        &ci_status("queued-then-green/1", E8).await,
        "pending",
        &["macos running 984"],
    );
    check(
        &ci_status("queued-then-green/2", E8).await,
        "success",
        &["frontend success 974", "macos success 984"],
    );
}

#[tokio::test]
async fn cancelled_while_still_queued() {
    check(
        &ci_status("cancelled-while-queued/1", E8).await,
        "failure",
        &["macos cancelled -"],
    );
}

#[tokio::test]
async fn a_fresh_push_from_no_run_to_green() {
    check(&ci_status("fresh-push/0", E8).await, "no_runs", &[]);
    check(
        &ci_status("fresh-push/1", E8).await,
        "pending",
        &["macos waiting -"],
    );
    check(
        &ci_status("fresh-push/2", E8).await,
        "success",
        &["macos success 984"],
    );
}
