//! Estimates against a fake Gitea: matching on the jobs that ran, keeping
//! history on disk between processes, and `ci_estimate` before a push.

mod common;

use common::forge::{TOKEN, fake_gitea, git, one_commit, token_env};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Answers GET `at` with `body`; `times`, if given, is how often it must be
/// asked.
async fn get(forge: &MockServer, at: &str, body: serde_json::Value, times: Option<u64>) {
    let mock = Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body));
    match times {
        Some(n) => mock.expect(n).mount(forge).await,
        None => mock.mount(forge).await,
    }
}

/// Six passing push runs of ci.yml on main, ids 30 to 35: the even ones
/// ran build and deploy in 15 minutes, the odd ones skipped deploy and took
/// 5. Each run's jobs can be read exactly once.
async fn mount_history(forge: &MockServer) {
    let runs: Vec<serde_json::Value> = (30..36u64)
        .rev()
        .map(|id| {
            let minutes = if id % 2 == 0 { 15 } else { 5 };
            json!({
                "id": id, "run_number": id - 28, "head_sha": "b".repeat(40),
                "head_branch": "main", "event": "push", "status": "completed",
                "conclusion": "success", "display_title": "old",
                "path": "ci.yml@refs/heads/main",
                "started_at": "2026-11-01T10:00:00Z",
                "completed_at": format!("2026-11-01T10:{minutes:02}:00Z"),
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .and(query_param("branch", "main"))
        .and(query_param("event", "push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "workflow_runs": runs })))
        .with_priority(1)
        // A second process finds the history on disk, fresh enough.
        .expect(1)
        .mount(forge)
        .await;
    for id in 30..36u64 {
        let deploy = if id % 2 == 0 { "success" } else { "skipped" };
        let jobs = json!({ "jobs": [
            { "id": id * 10, "name": "build", "status": "completed", "conclusion": "success" },
            { "id": id * 10 + 1, "name": "deploy", "status": "completed", "conclusion": deploy },
        ]});
        let at = format!("/api/v1/repos/acme/widgets/actions/runs/{id}/jobs");
        get(forge, &at, jobs, Some(1)).await;
    }
}

/// A checkout of main, and a Gitea where its head has run 41 of ci.yml
/// going for four minutes (build passed, and deploy runs), with history.
async fn setup() -> (tempfile::TempDir, MockServer) {
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = gitea_with_history(dir.path(), &sha).await;
    (dir, forge)
}

async fn start(dir: &tempfile::TempDir, forge: &MockServer) -> common::Client {
    let var = token_env(forge);
    common::start(dir.path(), &[(var.as_str(), TOKEN)]).await
}

async fn gitea_with_history(dir: &std::path::Path, sha: &str) -> MockServer {
    let forge = fake_gitea("1.25.3").await;
    mount_history(&forge).await;
    let started = (chrono::Utc::now() - chrono::Duration::minutes(4)).to_rfc3339();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/actions/runs"))
        .and(query_param("head_sha", sha))
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
    let jobs = json!({ "jobs": [
        { "id": 410, "name": "build", "status": "completed", "conclusion": "success",
          "started_at": started, "completed_at": started },
        { "id": 411, "name": "deploy", "status": "in_progress", "started_at": started },
    ]});
    get(
        &forge,
        "/api/v1/repos/acme/widgets/actions/runs/41/jobs",
        jobs,
        None,
    )
    .await;
    let main = json!({ "name": "main", "commit": { "id": sha } });
    get(
        &forge,
        "/api/v1/repos/acme/widgets/branches/main",
        main,
        None,
    )
    .await;
    git(
        dir,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/acme/widgets.git", forge.uri()),
        ],
    );
    forge
}

#[tokio::test]
async fn a_running_run_is_estimated_from_runs_with_its_jobs_and_history_is_kept() {
    let (dir, forge) = setup().await;
    let client = start(&dir, &forge).await;
    let status = common::call(&client, "ci_status", Default::default()).await;
    let eta = &status["estimates"][0];
    // Deploy is running, so only the runs that deployed agree: 15
    // minutes, where the whole workflow would say 5.
    assert_eq!(eta["matched_job_set"], true, "{status}");
    assert_eq!(eta["pass"]["samples"], 3, "{status}");
    assert_eq!(eta["pass"]["median_seconds"], 900, "{status}");
    assert!(
        eta["basis"]
            .as_str()
            .unwrap()
            .starts_with("3 passed runs with the same 2 jobs"),
        "{status}"
    );
    client.cancel().await.unwrap();

    let kept = std::fs::read_dir(dir.path().join("history"))
        .unwrap()
        .count();
    assert_eq!(kept, 1, "one history file for the repository");

    // A new process reads the history from disk: no run list, no jobs.
    let client = start(&dir, &forge).await;
    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["estimates"][0]["pass"]["median_seconds"], 900);
    client.cancel().await.unwrap();
    forge.verify().await;
}

#[tokio::test]
async fn ci_estimate_gives_the_spread_by_job_set_before_a_push() {
    let (dir, forge) = setup().await;
    let client = start(&dir, &forge).await;
    assert!(
        common::tool_names(&client)
            .await
            .contains(&"ci_estimate".to_owned())
    );

    let estimate = common::call(&client, "ci_estimate", Default::default()).await;
    assert_eq!(estimate["branch"], "main", "{estimate}");
    let outlook = &estimate["estimates"][0];
    assert_eq!(outlook["workflow"], "ci.yml", "{estimate}");
    assert_eq!(outlook["pass"]["samples"], 6, "{estimate}");
    let sets = outlook["job_sets"].as_array().unwrap();
    assert_eq!(sets.len(), 2, "{estimate}");
    let medians: Vec<i64> = sets
        .iter()
        .map(|s| s["median_seconds"].as_i64().unwrap())
        .collect();
    assert!(
        medians.contains(&300) && medians.contains(&900),
        "{estimate}"
    );
    let summary = estimate["summary"].as_str().unwrap();
    assert!(summary.contains("by job set: "), "{summary}");
    assert!(summary.contains("1 job (3 runs): 5m"), "{summary}");
    assert!(summary.contains("2 jobs (3 runs): 15m"), "{summary}");
    client.cancel().await.unwrap();
}
