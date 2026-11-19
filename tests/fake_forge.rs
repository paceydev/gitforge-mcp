//! The real binary against a fake forge: detection, the credential on the
//! wire, falling back to the last probe when the forge goes away, and
//! explaining a HEAD with no runs.

mod common;

use common::forge::{
    TOKEN, fake_forgejo, fake_gitea, git, one_commit, statuses, token_env, without_actions,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Points the checkout in `dir` at the fake forge.
fn point_at(dir: &std::path::Path, forge: &MockServer) {
    let url = format!("{}/acme/widgets.git", forge.uri());
    git(dir, &["remote", "set-url", "origin", &url]);
}

/// The server in the checkout in `dir`, pointed at the fake forge, with its
/// token.
async fn start_on(dir: &std::path::Path, forge: &MockServer) -> common::Client {
    point_at(dir, forge);
    let var = token_env(forge);
    common::start(dir, &[(var.as_str(), TOKEN)]).await
}

fn capability<'a>(report: &'a serde_json::Value, feature: &str) -> &'a serde_json::Value {
    report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == feature)
        .unwrap_or_else(|| panic!("no {feature} in {report}"))
}

#[tokio::test]
async fn detects_forgejo_and_reads_permissions() {
    let forge = fake_forgejo("15.0.11+gitea-1.22.0").await;
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let report = common::forge_context(&client, false).await;
    assert_eq!(report["status"], "live", "{report}");
    assert_eq!(report["forge"]["kind"], "forgejo");
    assert_eq!(report["forge"]["version"], "15.0.11");
    assert_eq!(report["user"]["login"], "dev");
    assert_eq!(report["repository"]["permissions"]["push"], true);
    assert_eq!(capability(&report, "ci_runs")["available"], true);
    assert_eq!(
        capability(&report, "ci_cancel")["reason"],
        "needs Forgejo 16 or later; this server runs 15.0.11"
    );
    assert_eq!(
        capability(&report, "repo_settings")["reason"],
        "needs admin access to the repository"
    );
    assert!(!report.to_string().contains(TOKEN));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn detects_gitea_by_the_missing_forgejo_endpoint() {
    let forge = fake_gitea("1.25.3").await;
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let report = common::forge_context(&client, false).await;
    assert_eq!(report["forge"]["kind"], "gitea", "{report}");
    assert_eq!(report["forge"]["detected_by"], "/api/v1/version");
    assert_eq!(capability(&report, "ci_runs")["available"], true);
    assert_eq!(
        capability(&report, "ci_rerun")["reason"],
        "needs Gitea 1.26 or later; this server runs 1.25.3"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn falls_back_to_the_last_probe_when_the_forge_goes_away() {
    let forge = fake_forgejo("17.0.1+gitea-1.22.0").await;
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let live = common::forge_context(&client, false).await;
    assert_eq!(live["status"], "live", "{live}");

    drop(forge);
    let cached = common::forge_context(&client, true).await;
    assert_eq!(cached["status"], "cached", "{cached}");
    assert_eq!(cached["forge"]["version"], "17.0.1");
    assert_eq!(cached["repository"], live["repository"]);
    assert_eq!(capability(&cached, "ci_rerun")["available"], true);
    assert!(
        cached["cached"]["age"]
            .as_str()
            .unwrap()
            .ends_with("seconds")
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_refused_credential_is_an_error_not_a_fallback() {
    let forge = fake_forgejo("17.0.1+gitea-1.22.0").await;
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), "wrong-token")]).await;

    let report = common::forge_context(&client, false).await;
    // The repo endpoint only matches the right token; anything else is 404,
    // which a forge also returns for a private repo the caller can't see.
    assert_eq!(report["status"], "error", "{report}");
    assert!(
        report["problems"]
            .to_string()
            .contains("or the credential cannot see it")
    );
    assert_eq!(common::tool_names(&client).await, ["forge_context"]);
    client.cancel().await.unwrap();
}

/// A checkout of acme/widgets on `main` with two commits; returns their shas.
fn two_commits(dir: &std::path::Path, forge: &MockServer) -> (String, String) {
    common::git_checkout(dir, &format!("{}/acme/widgets.git", forge.uri()));
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let first = git(dir, &["rev-parse", "HEAD"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "two"]);
    (first, git(dir, &["rev-parse", "HEAD"]))
}

/// A Gitea with no runs at all, whose `main` is at `main` (or missing).
async fn gitea_without_runs(main: Option<&str>) -> MockServer {
    let forge = fake_gitea("1.25.3").await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "workflow_runs": [], "total_count": 0 })),
        )
        .mount(&forge)
        .await;
    if let Some(main) = main {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/acme/widgets/branches/main"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "name": "main", "commit": { "id": main } })),
            )
            .mount(&forge)
            .await;
    }
    forge
}

/// ci_status on HEAD, returning its verdict and notes.
async fn status_notes(dir: &std::path::Path, forge: &MockServer) -> (String, String) {
    let var = token_env(forge);
    let client = common::start(dir, &[(var.as_str(), TOKEN)]).await;
    let status = common::call(&client, "ci_status", Default::default()).await;
    client.cancel().await.unwrap();
    (
        status["verdict"].as_str().unwrap().to_owned(),
        status["notes"].to_string(),
    )
}

#[tokio::test]
async fn no_runs_on_a_head_behind_the_forge_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let probe = MockServer::start().await;
    let (first, second) = two_commits(dir.path(), &probe);
    // The second commit stays in the object store, as after a fetch.
    git(dir.path(), &["reset", "-q", "--hard", &first]);
    let forge = gitea_without_runs(Some(&second)).await;
    point_at(dir.path(), &forge);

    let (verdict, notes) = status_notes(dir.path(), &forge).await;
    assert_eq!(verdict, "no_runs");
    assert!(
        notes.contains("HEAD is 1 commit behind main on the forge"),
        "{notes}"
    );
    assert!(notes.contains("no runs either"), "{notes}");
}

#[tokio::test]
async fn no_runs_on_a_head_ahead_of_the_forge_means_unpushed() {
    let dir = tempfile::tempdir().unwrap();
    let probe = MockServer::start().await;
    let (first, _) = two_commits(dir.path(), &probe);
    let forge = gitea_without_runs(Some(&first)).await;
    point_at(dir.path(), &forge);

    let (_, notes) = status_notes(dir.path(), &forge).await;
    assert!(
        notes.contains("HEAD is 1 commit ahead of main on the forge"),
        "{notes}"
    );
    assert!(notes.contains("once they're pushed"), "{notes}");
}

#[tokio::test]
async fn no_runs_with_unfetched_commits_on_the_forge() {
    let dir = tempfile::tempdir().unwrap();
    let forge = gitea_without_runs(Some(&"f".repeat(40))).await;
    two_commits(dir.path(), &forge);

    let (_, notes) = status_notes(dir.path(), &forge).await;
    assert!(
        notes.contains("hasn't fetched, so HEAD is behind it"),
        "{notes}"
    );
}

#[tokio::test]
async fn no_runs_on_a_branch_the_forge_has_never_seen() {
    let dir = tempfile::tempdir().unwrap();
    let forge = gitea_without_runs(None).await;
    two_commits(dir.path(), &forge);

    let (_, notes) = status_notes(dir.path(), &forge).await;
    assert!(
        notes.contains("there is no branch main on the forge"),
        "{notes}"
    );
}

/// A Gitea where `sha`, the head of `main`, has one run (run 41) whose
/// state is the Gitea `status` and `conclusion`, with `jobs`.
async fn gitea_with_run(
    dir: &std::path::Path,
    sha: &str,
    status: &str,
    conclusion: Option<&str>,
    jobs: serde_json::Value,
) -> MockServer {
    let forge = fake_gitea("1.25.3").await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 1,
            "workflow_runs": [{
                "id": 41, "run_number": 7, "head_sha": sha, "head_branch": "main",
                "event": "push", "status": status, "conclusion": conclusion,
                "display_title": "one", "path": "ci.yml@refs/heads/main",
                "started_at": "2026-11-09T10:00:00Z"
            }]
        })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs/41/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "jobs": jobs })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/branches/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "main", "commit": { "id": sha } })),
        )
        .mount(&forge)
        .await;
    point_at(dir, &forge);
    forge
}

fn finished_job(conclusion: &str) -> serde_json::Value {
    json!({
        "id": 5, "name": "build", "status": "completed", "conclusion": conclusion,
        "started_at": "2026-11-09T10:00:00Z", "completed_at": "2026-11-09T10:02:00Z",
        "steps": [
            {"name": "Set up job", "number": 1, "status": "completed", "conclusion": "success"},
            {"name": "Compile", "number": 2, "status": "completed", "conclusion": conclusion}
        ]
    })
}

/// `gitforge-mcp wait ARGS` in `dir`: exit code, stdout, stderr.
async fn cli_wait(
    dir: &std::path::Path,
    forge: &MockServer,
    args: &[&str],
) -> (i32, String, String) {
    let var = token_env(forge);
    let out = common::command(dir, &[(var.as_str(), TOKEN)])
        .arg("wait")
        .args(args)
        .output()
        .await
        .expect("runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn cli_wait_exits_by_verdict() {
    // A pass ends on its summary, which names the jobs it skipped.
    let mut skipped = finished_job("skipped");
    skipped["id"] = json!(6);
    skipped["name"] = json!("prod deploy");
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = gitea_with_run(
        dir.path(),
        &sha,
        "completed",
        Some("success"),
        json!([finished_job("success"), skipped]),
    )
    .await;
    let (code, stdout, stderr) = cli_wait(dir.path(), &forge, &[]).await;
    assert_eq!(code, 0, "{stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["verdict"], "success");
    assert!(
        stderr.contains("success: 1 run, 2 jobs: 1 success, 1 skipped (prod deploy)"),
        "{stderr}"
    );

    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = gitea_with_run(
        dir.path(),
        &sha,
        "completed",
        Some("failure"),
        json!([finished_job("failure")]),
    )
    .await;
    let (code, _, stderr) = cli_wait(dir.path(), &forge, &[]).await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("failed: build (at step 2: Compile)"),
        "{stderr}"
    );
}

#[tokio::test]
async fn cli_usage_errors_exit_64_not_wait_codes() {
    let dir = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (&["wait", "--sha", "abc", "--branch", "main"][..], 64),
        (&["--help"][..], 0),
    ] {
        let out = common::command(dir.path(), &[])
            .args(args)
            .output()
            .await
            .expect("runs");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(expected), "{args:?}: {stderr}");
    }
}

#[tokio::test]
async fn cli_wait_shows_the_step_a_running_job_is_on() {
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let running = json!([{
        "id": 5, "name": "test", "status": "in_progress", "started_at": "2026-11-09T10:00:00Z",
        "steps": [
            {"name": "Set up job", "number": 1, "status": "completed", "conclusion": "success"},
            {"name": "cargo test", "number": 2, "status": "in_progress"},
            {"name": "Upload", "number": 3, "status": "queued"}
        ]
    }]);
    let forge = gitea_with_run(dir.path(), &sha, "in_progress", None, running).await;
    let (code, stdout, stderr) = cli_wait(dir.path(), &forge, &["--timeout", "1"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("running: test (step 2 of 3: cargo test)"),
        "{stderr}"
    );
    let result: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["runs"][0]["jobs"][0]["steps"][1]["state"], "running");
}

#[tokio::test]
async fn ci_wait_is_a_task_for_clients_that_support_tasks() {
    use rmcp::model::{CallToolRequestParams, CallToolResponse, GetTaskParams, TaskPayload};

    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = gitea_with_run(
        dir.path(),
        &sha,
        "completed",
        Some("success"),
        json!([finished_job("success")]),
    )
    .await;
    let var = token_env(&forge);

    // A client without tasks gets the result directly.
    let plain = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;
    let instructions = plain
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    let command = format!("\"{}\" wait --dir", env!("CARGO_BIN_EXE_gitforge-mcp"));
    assert!(instructions.contains(&command), "{instructions}");
    let ci_wait = common::tools(&plain)
        .await
        .into_iter()
        .find(|t| t.name == "ci_wait")
        .expect("ci_wait offered");
    let description = ci_wait.description.as_deref().unwrap_or_default();
    assert!(description.contains(&command), "{description}");
    assert!(
        description.contains("exit code 0 means CI passed"),
        "{description}"
    );
    let result = common::call(&plain, "ci_wait", Default::default()).await;
    assert_eq!(result["verdict"], "success");
    assert_eq!(result["progress"], "1 of 1 jobs finished (1 success)");
    plain.cancel().await.unwrap();

    // One with tasks gets a task, and the result once it's done.
    let client = common::start_with_tasks(dir.path(), &[(var.as_str(), TOKEN)]).await;
    let response = client
        .call_tool_once(CallToolRequestParams::new("ci_wait"))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task, got {response:?}");
    };
    let task_id = created.task.task_id.clone();
    let task = loop {
        let got = client
            .peer()
            .get_task(GetTaskParams::new(task_id.clone()))
            .await
            .unwrap();
        if got.task.status().is_terminal() {
            break got.task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let TaskPayload::Completed { result } = task.payload else {
        panic!("expected a completed task, got {:?}", task.payload);
    };
    assert_eq!(
        result["structuredContent"]["verdict"], "success",
        "{result:?}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn ci_status_lists_only_jobs_needing_attention_unless_asked() {
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let mut failed = finished_job("failure");
    failed["id"] = json!(6);
    failed["name"] = json!("test");
    let jobs = json!([finished_job("success"), failed]);
    let forge = gitea_with_run(dir.path(), &sha, "completed", Some("failure"), jobs).await;
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(
        status["runs"][0]["jobs"].as_array().unwrap().len(),
        1,
        "{status}"
    );
    assert_eq!(status["runs"][0]["jobs"][0]["name"], "test");
    assert_eq!(
        status["jobs_omitted"],
        "1 success: pass jobs=\"all\" to list them"
    );
    assert_eq!(status["summary"], "1 run, 2 jobs: 1 failure, 1 success");

    let mut all = serde_json::Map::new();
    all.insert("jobs".into(), json!("all"));
    let status = common::call(&client, "ci_status", all).await;
    assert_eq!(status["runs"][0]["jobs"].as_array().unwrap().len(), 2);
    assert!(status["jobs_omitted"].is_null());

    let mut none = serde_json::Map::new();
    none.insert("jobs".into(), json!("none"));
    let status = common::call(&client, "ci_status", none).await;
    assert_eq!(status["runs"][0]["jobs"], json!([]));
    assert_eq!(status["verdict"], "failure");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_running_run_gets_an_estimate_from_its_history() {
    use wiremock::matchers::query_param;

    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let started = (chrono::Utc::now() - chrono::Duration::minutes(4)).to_rfc3339();
    let forge = gitea_with_run(
        dir.path(),
        &sha,
        "in_progress",
        None,
        json!([{ "id": 5, "name": "build", "status": "in_progress", "started_at": started }]),
    )
    .await;
    // The history: five passing runs of ci.yml on main, 600 to 640 seconds.
    let history: Vec<serde_json::Value> = (0..5)
        .map(|i| {
            json!({
                "id": 30 + i, "run_number": 2 + i, "head_sha": "b".repeat(40),
                "head_branch": "main", "event": "push", "status": "completed",
                "conclusion": "success", "display_title": "old",
                "path": "ci.yml@refs/heads/main",
                "started_at": "2026-11-01T10:00:00Z",
                "completed_at": format!("2026-11-01T10:{:02}:00Z", 10 + i / 2),
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .and(query_param("branch", "main"))
        .and(query_param("event", "push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": history })))
        .with_priority(1)
        .mount(&forge)
        .await;
    // The running run itself started four minutes ago.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .and(query_param("head_sha", sha.as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": [{
                "id": 41, "run_number": 7, "head_sha": sha, "head_branch": "main",
                "event": "push", "status": "in_progress", "display_title": "one",
                "path": "ci.yml@refs/heads/main", "started_at": started
            }]})),
        )
        .with_priority(1)
        .mount(&forge)
        .await;
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let status = common::call(&client, "ci_status", Default::default()).await;
    let eta = &status["estimates"][0];
    assert_eq!(eta["run"], 7, "{status}");
    assert_eq!(eta["pass"]["samples"], 5);
    assert_eq!(eta["pass"]["median_seconds"], 660);
    assert!(eta["fail"].is_null());
    assert!(
        status["progress"]
            .as_str()
            .unwrap()
            .contains("4m in; a pass usually takes 11m"),
        "{status}"
    );
    client.cancel().await.unwrap();
}

/// A Gitea with open pull request #3 from feature-x at `head` into main,
/// which merges when asked. `history` is what the repository's run list
/// holds; the head commit itself has no runs.
async fn gitea_with_pull_request(head: &str, history: serde_json::Value) -> MockServer {
    use wiremock::matchers::query_param;

    let forge = fake_gitea("1.25.3").await;
    let pr = |merged: bool| {
        json!({
            "number": 3, "title": "t", "state": if merged { "closed" } else { "open" },
            "merged": merged, "mergeable": true, "user": {"login": "dev"},
            "head": {"ref": "feature-x", "sha": head}, "base": {"ref": "main", "sha": "c".repeat(40)},
            "html_url": "u", "merge_commit_sha": if merged { "d".repeat(40) } else { String::new() }
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr(false)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr(true)))
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/merge"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .and(query_param("head_sha", head))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": [] })))
        .with_priority(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": history })))
        .mount(&forge)
        .await;
    // GET /branches/feature-x isn't mounted: 404, so the branch is gone.
    forge
}

fn merge_args(head: &str) -> serde_json::Map<String, serde_json::Value> {
    json!({"action": "merge", "number": 3, "expected_head_sha": head, "delete_branch": true})
        .as_object()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn merging_in_a_repository_without_ci_needs_no_override() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let forge = gitea_with_pull_request(&head, json!([])).await;
    let client = start_on(dir.path(), &forge).await;

    let merged = common::call(&client, "pull_request", merge_args(&head)).await;
    assert_eq!(merged["merged"], true, "{merged}");
    assert_eq!(merged["branch_deleted"], true, "{merged}");
    assert_eq!(merged["merge_commit_sha"], "d".repeat(40));
    assert!(
        merged["notes"].to_string().contains("never run CI"),
        "{merged}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn merging_still_waits_for_ci_in_a_repository_that_has_it() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let old_run = json!([{
        "id": 1, "run_number": 1, "head_sha": "e".repeat(40), "head_branch": "main",
        "event": "push", "status": "completed", "conclusion": "success",
        "display_title": "old", "path": "ci.yml@refs/heads/main"
    }]);
    let forge = gitea_with_pull_request(&head, old_run).await;
    let client = start_on(dir.path(), &forge).await;

    let refused = common::try_call(&client, "pull_request", merge_args(&head)).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("no CI run exists")),
        "{refused:?}"
    );
    client.cancel().await.unwrap();
}

/// A Gitea with open pull request #3 from feature-x at `head` into main,
/// whose CI is still running. `requires_checks` is whether main's branch
/// protection makes merges wait for status checks. A scheduled merge is
/// accepted `schedules` times and can then be cancelled once.
async fn gitea_awaiting_ci(
    dir: &std::path::Path,
    head: &str,
    requires_checks: bool,
    schedules: u64,
) -> MockServer {
    use wiremock::matchers::body_partial_json;

    let forge = gitea_with_run(
        dir,
        head,
        "in_progress",
        None,
        json!([{"id": 5, "name": "build", "status": "in_progress", "steps": []}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 3, "title": "t", "state": "open", "merged": false, "mergeable": true,
            "user": {"login": "dev"}, "head": {"ref": "feature-x", "sha": head},
            "base": {"ref": "main", "sha": "c".repeat(40)}, "html_url": "u"
        })))
        .mount(&forge)
        .await;
    // Ahead of gitea_with_run's plain branch answer.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "main", "commit": {"id": "c".repeat(40)}, "protected": requires_checks,
            "enable_status_check": requires_checks,
            "status_check_contexts": ["ci / build (push)"]
        })))
        .with_priority(1)
        .mount(&forge)
        .await;
    // Gitea before 1.26 spells the style key `Do`.
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/merge"))
        .and(body_partial_json(json!({
            "Do": "squash", "head_commit_id": head, "merge_when_checks_succeed": true
        })))
        .respond_with(ResponseTemplate::new(201))
        .expect(schedules)
        .mount(&forge)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/merge"))
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&forge)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/merge"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&forge)
        .await;
    forge
}

fn pr_args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test]
async fn merge_when_ready_schedules_while_ci_runs_and_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let forge = gitea_awaiting_ci(dir.path(), &head, true, 1).await;
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let scheduled = common::call(
        &client,
        "pull_request",
        pr_args(json!({
            "action": "merge_when_ready", "number": 3, "expected_head_sha": head, "style": "squash"
        })),
    )
    .await;
    assert_eq!(scheduled["scheduled"], true, "{scheduled}");
    assert_eq!(scheduled["merged"], false, "{scheduled}");
    assert_eq!(scheduled["ci"], "pending", "{scheduled}");
    assert_eq!(scheduled["required_checks"], json!(["ci / build (push)"]));
    assert!(
        scheduled["notes"]
            .to_string()
            .contains("cancel_auto_merge stops it"),
        "{scheduled}"
    );

    let moved = common::try_call(
        &client,
        "pull_request",
        pr_args(json!({"action": "merge_when_ready", "number": 3, "expected_head_sha": "fedcba9"})),
    )
    .await;
    assert!(
        moved.as_ref().is_err_and(|e| e.contains("has moved")),
        "{moved:?}"
    );

    let cancelled = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "cancel_auto_merge", "number": 3})),
    )
    .await;
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    let again = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "cancel_auto_merge", "number": 3})),
    )
    .await;
    assert_eq!(again["cancelled"], false, "{again}");
    assert!(
        again["notes"].to_string().contains("no merge scheduled"),
        "{again}"
    );
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn merge_when_ready_refuses_a_base_that_would_not_wait_for_ci() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let forge = gitea_awaiting_ci(dir.path(), &head, false, 0).await;
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let refused = common::try_call(
        &client,
        "pull_request",
        pr_args(json!({"action": "merge_when_ready", "number": 3, "style": "squash"})),
    )
    .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("doesn't require status checks")),
        "{refused:?}"
    );
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn merge_when_ready_merges_at_once_without_ci() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let forge = gitea_with_pull_request(&head, json!([])).await;
    let client = start_on(dir.path(), &forge).await;

    let merged = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "merge_when_ready", "number": 3, "expected_head_sha": head})),
    )
    .await;
    assert_eq!(merged["merged"], true, "{merged}");
    assert_eq!(merged["scheduled"], false, "{merged}");
    assert_eq!(merged["merge_commit_sha"], "d".repeat(40));
    assert!(
        merged["notes"].to_string().contains("merged at once"),
        "{merged}"
    );
    client.cancel().await.unwrap();
}

/// A Forgejo 17 whose pull request #3 has a review asking for changes with
/// one open and one resolved line comment, a dismissed review, and an
/// approval.
async fn forgejo_with_reviews(dir: &std::path::Path) -> MockServer {
    use wiremock::matchers::query_param;

    let forge = fake_forgejo("17.0.1").await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 3, "title": "t", "state": "open", "merged": false, "mergeable": true,
            "user": {"login": "dev"}, "head": {"ref": "feature-x", "sha": "a".repeat(40)},
            "base": {"ref": "main", "sha": "c".repeat(40)}, "html_url": "u"
        })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/reviews"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 12, "user": {"login": "alice"}, "team": null, "state": "REQUEST_CHANGES",
                "body": "Two things.", "commit_id": "a".repeat(40), "stale": false,
                "official": true, "dismissed": false, "comments_count": 2,
                "submitted_at": "2026-11-09T10:00:00Z", "updated_at": "2026-11-09T10:00:00Z"
            },
            {
                "id": 13, "user": {"login": "bob"}, "state": "REQUEST_CHANGES", "body": "No.",
                "stale": true, "dismissed": true, "comments_count": 0,
                "submitted_at": "2026-11-08T10:00:00Z"
            },
            {
                "id": 14, "user": {"login": "carol"}, "state": "APPROVED", "body": "",
                "stale": false, "dismissed": false, "comments_count": 0,
                "submitted_at": "2026-11-09T11:00:00Z"
            }
        ])))
        .with_priority(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v1/repos/acme/widgets/pulls/3/reviews/12/comments",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 40, "body": "Why this line?", "user": {"login": "alice"}, "resolver": null,
                "pull_request_review_id": 12, "path": "src/lib.rs", "commit_id": "a".repeat(40),
                "diff_hunk": "@@ -1,2 +1,3 @@\n fn a() {}\n+fn b() {}", "position": 2,
                "original_position": 0, "extra_lines_count": 0
            },
            {
                "id": 41, "body": "Rename this.", "user": {"login": "alice"},
                "resolver": {"login": "dev"}, "path": "src/lib.rs",
                "diff_hunk": "@@ -1 +1 @@\n+fn a() {}", "position": 1, "original_position": 0
            }
        ])))
        .mount(&forge)
        .await;
    point_at(dir, &forge);
    forge
}

#[tokio::test]
async fn reviews_list_what_is_still_open() {
    let dir = tempfile::tempdir().unwrap();
    one_commit(dir.path());
    let forge = forgejo_with_reviews(dir.path()).await;
    let var = token_env(&forge);
    let client = common::start(dir.path(), &[(var.as_str(), TOKEN)]).await;

    let open = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "reviews", "number": 3})),
    )
    .await;
    assert_eq!(open["approved_by"], json!(["carol"]), "{open}");
    assert_eq!(open["changes_requested_by"], json!(["alice"]), "{open}");
    assert_eq!(open["open_comments"], 1, "{open}");
    let reviews = open["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1, "{open}");
    assert_eq!(reviews[0]["state"], "request_changes");
    assert_eq!(
        reviews[0]["comments"],
        json!([{
            "id": 40, "path": "src/lib.rs", "line": 2, "author": "alice",
            "body": "Why this line?", "diff_hunk": "@@ -1,2 +1,3 @@\n fn a() {}\n+fn b() {}"
        }])
    );
    assert!(
        open["omitted"]
            .as_str()
            .unwrap()
            .starts_with("1 dismissed review, 1 resolved comment, 1 review with nothing to read"),
        "{open}"
    );

    let all = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "reviews", "number": 3, "include_resolved": true})),
    )
    .await;
    assert_eq!(all["reviews"].as_array().unwrap().len(), 3, "{all}");
    assert_eq!(all["reviews"][0]["comments"][1]["resolved_by"], "dev");

    let pr = common::call(
        &client,
        "pull_request",
        pr_args(json!({"action": "get", "number": 3})),
    )
    .await;
    assert_eq!(
        pr["reviews"],
        json!({
            "approved_by": ["carol"], "changes_requested_by": ["alice"],
            "review_requested": [], "open_comments": 1
        }),
        "{pr}"
    );
    client.cancel().await.unwrap();
}

fn woodpecker(id: u64, state: &str, step: &str) -> serde_json::Value {
    json!({
        "id": id, "status": state, "context": format!("ci/woodpecker/push/{step}"),
        "target_url": "https://ci.example.com/repos/1/pipeline/7", "description": "",
        "created_at": "2026-11-09T10:00:00Z", "updated_at": "2026-11-09T10:03:00Z"
    })
}

#[tokio::test]
async fn ci_outside_the_forge_is_read_from_commit_statuses() {
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = fake_gitea("1.25.3").await;
    without_actions(&forge).await;
    statuses(
        &forge,
        &sha,
        json!([
            woodpecker(1, "success", "build"),
            woodpecker(2, "failure", "lint")
        ]),
    )
    .await;
    let client = start_on(dir.path(), &forge).await;

    // Actions is off, so nothing that needs it is offered.
    assert_eq!(
        common::tool_names(&client).await,
        ["ci_status", "ci_wait", "forge_context", "pull_request"]
    );
    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["verdict"], "failure", "{status}");
    let run = &status["runs"][0];
    assert_eq!(run["external"], "ci/woodpecker/push");
    assert_eq!(run["url"], "https://ci.example.com/repos/1/pipeline/7");
    assert_eq!(run["jobs"][0]["name"], "lint");
    assert_eq!(
        status["progress"],
        "2 of 2 jobs finished (1 success, 1 failure); failed: lint"
    );
    assert!(
        !status["notes"].to_string().contains("no jobs endpoint"),
        "{status}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn merging_waits_for_ci_outside_the_forge() {
    let dir = tempfile::tempdir().unwrap();
    let head = one_commit(dir.path());
    let forge = gitea_with_pull_request(&head, json!([])).await;
    without_actions(&forge).await;
    // The base branch has Woodpecker statuses, so the repository has CI;
    // the pull request's head has none yet.
    let base = "c".repeat(40);
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/branches/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "main", "commit": { "id": base } })),
        )
        .mount(&forge)
        .await;
    statuses(&forge, &base, json!([woodpecker(1, "success", "build")])).await;
    statuses(&forge, &head, json!([])).await;
    let client = start_on(dir.path(), &forge).await;

    let refused = common::try_call(&client, "pull_request", merge_args(&head)).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("no CI run exists")),
        "{refused:?}"
    );
    client.cancel().await.unwrap();
}

/// Gitea 1.26 on returns the dispatched run's id and page, and the page
/// names the run by id from some version on: run 41 here, though it's the
/// repository's run 7. The number comes from the run itself.
#[tokio::test]
async fn a_dispatched_run_is_numbered_by_the_run_not_its_page() {
    use wiremock::matchers::query_param;

    let dir = tempfile::tempdir().unwrap();
    one_commit(dir.path());
    let forge = fake_gitea("28.3.0").await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/repos/acme/widgets/actions/workflows/ci.yml/dispatches",
        ))
        .and(query_param("return_run_details", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "workflow_run_id": 41,
            "run_url": format!("{}/api/v1/repos/acme/widgets/actions/runs/41", forge.uri()),
            "html_url": format!("{}/acme/widgets/actions/runs/41", forge.uri()),
        })))
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs/41"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 41, "run_number": 7, "head_sha": "a".repeat(40), "event": "workflow_dispatch",
            "status": "queued", "path": "ci.yml@refs/heads/main"
        })))
        .mount(&forge)
        .await;
    let client = start_on(dir.path(), &forge).await;
    let args = json!({"action": "dispatch", "workflow": "ci.yml", "ref": "main"})
        .as_object()
        .unwrap()
        .clone();
    let started = common::call(&client, "ci_control", args).await;
    assert_eq!(started["started"]["run_number"], 7, "{started}");
    assert_eq!(started["started"]["run_id"], 41, "{started}");
    client.cancel().await.unwrap();
}
