//! Contract tests against github.com itself, which can't run in a
//! container like the Gitea and Forgejo ones. Opt-in, and never in CI: they
//! need a token, and the writes change a repository.
//!
//! - `GITFORGE_GITHUB_CONTRACT_REPO`: `owner/name` of a disposable
//!   repository with Actions runs on its default branch. Nothing else.
//! - `GITFORGE_GITHUB_CONTRACT_TOKEN`: a token with admin on it.
//! - `GITFORGE_GITHUB_CONTRACT_WRITES=1` also runs the writes: a branch is
//!   pushed and a pull request opened from it, refused for a moved head,
//!   then squash-merged into the default branch (which must not require
//!   checks) and its branch deleted.
//! - `GITFORGE_GITHUB_CONTRACT_WORKFLOW`: with the writes, a workflow file
//!   with `workflow_dispatch` to dispatch and then cancel.
//!
//! ```bash
//! GITFORGE_GITHUB_CONTRACT_REPO=acme/widgets-scratch \
//! GITFORGE_GITHUB_CONTRACT_TOKEN=... \
//!   cargo test --test contract_github -- --nocapture
//! ```

mod common;

use base64::Engine;
use serde_json::{Value, json};

struct Target {
    repo: String,
    token: String,
}

fn target() -> Option<Target> {
    match (
        std::env::var("GITFORGE_GITHUB_CONTRACT_REPO"),
        std::env::var("GITFORGE_GITHUB_CONTRACT_TOKEN"),
    ) {
        (Ok(repo), Ok(token)) if !repo.is_empty() && !token.is_empty() => {
            Some(Target { repo, token })
        }
        _ => {
            eprintln!(
                "skipping GitHub contract tests: GITFORGE_GITHUB_CONTRACT_REPO and _TOKEN are not set"
            );
            None
        }
    }
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

async fn client_for(target: &Target) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(
        dir.path(),
        &format!("https://github.com/{}.git", target.repo),
    );
    let var = gitforge_mcp::auth::default_env_var("github.com");
    let client = common::start(dir.path(), &[(var.as_str(), target.token.as_str())]).await;
    (client, dir)
}

/// Calls GitHub's REST API as the test's token, for setting things up.
async fn rest(target: &Target, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    let mut req = reqwest::Client::new()
        .request(
            method,
            format!("https://api.github.com/repos/{}{path}", target.repo),
        )
        .bearer_auth(&target.token)
        .header("User-Agent", "gitforge-mcp-contract")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.expect("GitHub API");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "{path}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

#[tokio::test]
async fn reads_match_github() {
    let Some(target) = target() else { return };
    let (client, _dir) = client_for(&target).await;

    let report = common::forge_context(&client, false).await;
    assert_eq!(report["status"], "live", "{report}");
    assert_eq!(report["forge"]["kind"], "github");
    for feature in [
        "ci_runs",
        "ci_jobs",
        "ci_logs",
        "ci_cancel",
        "ci_rerun",
        "ci_rerun_failed",
        "ci_dispatch",
        "ci_runners",
        "ci_queue",
        "ci_statuses",
        "pull_request_read",
        "pull_request_write",
    ] {
        let cap = report["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["feature"] == feature)
            .unwrap();
        assert_eq!(cap["available"], true, "{feature}: {cap}");
    }
    let branch = report["repository"]["default_branch"]
        .as_str()
        .unwrap()
        .to_owned();

    let status = common::call(
        &client,
        "ci_status",
        args(json!({"branch": branch, "jobs": "all"})),
    )
    .await;
    let runs = status["runs"].as_array().unwrap();
    assert!(
        !runs.is_empty(),
        "the default branch needs CI runs: {status}"
    );
    let actions = runs
        .iter()
        .find(|r| r["external"].is_null())
        .expect("an Actions run");
    // A run is known by its id, as its URL is.
    assert_eq!(actions["number"], actions["id"], "{actions}");
    if let Some(job) = actions["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["state"] == "success" || j["state"] == "failure")
    {
        let logs = common::call(
            &client,
            "ci_logs",
            args(json!({"run": actions["id"], "job": job["name"], "all": true})),
        )
        .await;
        assert!(logs["jobs"][0]["log"].get("error").is_none(), "{logs}");
    }
    let prs = common::call(&client, "pull_request", args(json!({"state": "all"}))).await;
    assert!(prs.is_object(), "{prs}");
    let runners = common::call(&client, "ci_runners", Default::default()).await;
    assert_eq!(runners["scopes"][0]["outcome"], "listed", "{runners}");
    client.cancel().await.unwrap();
}

/// Pushes a new branch with one new file through the API; returns its name.
async fn push_branch(target: &Target, base: &str) -> String {
    let stamp = chrono::Utc::now().timestamp();
    let branch = format!("gitforge-contract-{stamp}");
    let head = rest(
        target,
        reqwest::Method::GET,
        &format!("/git/ref/heads/{base}"),
        None,
    )
    .await;
    rest(
        target,
        reqwest::Method::POST,
        "/git/refs",
        Some(json!({"ref": format!("refs/heads/{branch}"), "sha": head["object"]["sha"]})),
    )
    .await;
    rest(
        target,
        reqwest::Method::PUT,
        &format!("/contents/gitforge-contract/{stamp}.txt"),
        Some(json!({
            "message": "gitforge-mcp contract test",
            "content": base64::engine::general_purpose::STANDARD.encode("contract\n"),
            "branch": branch,
        })),
    )
    .await;
    branch
}

#[tokio::test]
async fn writes_match_github() {
    let Some(target) = target() else { return };
    if std::env::var("GITFORGE_GITHUB_CONTRACT_WRITES").as_deref() != Ok("1") {
        eprintln!("skipping GitHub write contract tests: GITFORGE_GITHUB_CONTRACT_WRITES is not 1");
        return;
    }
    let (client, _dir) = client_for(&target).await;
    let report = common::forge_context(&client, false).await;
    let base = report["repository"]["default_branch"]
        .as_str()
        .unwrap()
        .to_owned();
    let branch = push_branch(&target, &base).await;

    let created = common::call(
        &client,
        "pull_request",
        args(json!({"action": "create", "head": branch, "base": base, "title": "gitforge-mcp contract test"})),
    )
    .await;
    let number = created["pull_request"]["number"].as_u64().unwrap();
    let moved = common::try_call(
        &client,
        "pull_request",
        args(json!({"action": "merge", "number": number, "expected_head_sha": "0000000", "require_ci": false})),
    )
    .await;
    assert!(
        moved.as_ref().is_err_and(|e| e.contains("has moved")),
        "{moved:?}"
    );
    let merged = common::call(
        &client,
        "pull_request",
        args(json!({"action": "merge", "number": number, "style": "squash", "delete_branch": true, "require_ci": false})),
    )
    .await;
    assert_eq!(merged["merged"], true, "{merged}");
    assert_eq!(merged["branch_deleted"], true, "{merged}");

    if let Ok(workflow) = std::env::var("GITFORGE_GITHUB_CONTRACT_WORKFLOW") {
        let started = common::call(
            &client,
            "ci_control",
            args(json!({"action": "dispatch", "workflow": workflow})),
        )
        .await;
        let id = started["started"]["run_id"]
            .as_u64()
            .expect("github.com names the run");
        let cancel = common::try_call(
            &client,
            "ci_control",
            args(json!({"action": "cancel", "run": id})),
        )
        .await;
        assert!(
            cancel.is_ok()
                || cancel
                    .as_ref()
                    .is_err_and(|e| e.contains("already finished")),
            "{cancel:?}"
        );
    }
    client.cancel().await.unwrap();
}
