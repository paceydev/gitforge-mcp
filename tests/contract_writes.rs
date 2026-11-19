//! Contract tests for the tools that change things, against the same real
//! forges as tests/contract.rs. A separate test binary, so it runs after
//! the read-only checks there have finished: these merge, cancel and rerun.
//!
//! For each forge: a pull request is opened, a merge is refused for a moved
//! head and for CI that hasn't passed, then merged; another is scheduled to
//! merge when a required check passes that never reports, stays unmerged,
//! and the schedule is cancelled; a workflow that can
//! never start is dispatched and cancelled; failed jobs are rerun; and a
//! read-only collaborator, and the server's read-only mode, get none of it.
//! Where a forge version lacks an action, the test checks the action is
//! refused with the version as the reason.

mod common;

use std::time::Duration;

use base64::Engine;
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
    Some(serde_json::from_str(&std::fs::read_to_string(path).expect("targets")).expect("JSON"))
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn version_at_least(target: &Target, major: u64, minor: u64) -> bool {
    let mut parts = target
        .version
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let v = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    v >= (major, minor)
}

/// Commits one file through the forge's API, optionally on a new branch.
async fn put_file(target: &Target, path: &str, content: &str, new_branch: Option<&str>) {
    let mut body = json!({
        "content": base64::engine::general_purpose::STANDARD.encode(content),
        "message": format!("Add {path}"),
        "branch": "main",
    });
    if let Some(b) = new_branch {
        body["new_branch"] = json!(b);
    }
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/repos/gfadmin/widgets/contents/{path}",
            target.base_url
        ))
        .header("Authorization", format!("token {}", target.admin_token))
        .header("User-Agent", "gitforge-mcp-contract")
        .json(&body)
        .send()
        .await
        .expect("contents API");
    assert!(
        resp.status().is_success(),
        "{path}: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// POSTs `body` to the repository API as the admin.
async fn admin_post(target: &Target, path: &str, body: Value) {
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/repos/gfadmin/widgets{path}",
            target.base_url
        ))
        .header("Authorization", format!("token {}", target.admin_token))
        .header("User-Agent", "gitforge-mcp-contract")
        .json(&body)
        .send()
        .await
        .expect("repository API");
    assert!(
        resp.status().is_success(),
        "{path}: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn client_for(
    target: &Target,
    token: &str,
    extra: &[(&str, &str)],
) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(
        dir.path(),
        &format!("{}/gfadmin/widgets.git", target.base_url),
    );
    let host = target.base_url.trim_start_matches("http://");
    let var = gitforge_mcp::auth::default_env_var(host);
    let mut env = vec![(var.as_str(), token)];
    env.extend_from_slice(extra);
    let client = common::start(dir.path(), &env).await;
    (client, dir)
}

const STUCK: &str = "name: stuck
on: [workflow_dispatch]
jobs:
  never:
    runs-on: no-such-runner-label
    steps:
      - run: echo never
";

#[tokio::test]
async fn writes_match_every_forge() {
    let Some(targets) = targets() else { return };
    let mut failures = Vec::new();
    for target in &targets {
        eprintln!("== {} (writes)", target.name);
        let mut check = |ok: bool, what: String| {
            if !ok {
                failures.push(format!("{}: {what}", target.name));
            }
        };
        let forgejo = target.kind == "forgejo";
        let can_cancel = if forgejo {
            version_at_least(target, 16, 0)
        } else {
            version_at_least(target, 28, 0)
        };
        let can_rerun = if forgejo {
            version_at_least(target, 17, 0)
        } else {
            version_at_least(target, 1, 26)
        };
        let workflows = if forgejo {
            ".forgejo/workflows"
        } else {
            ".gitea/workflows"
        };

        put_file(target, &format!("{workflows}/stuck.yml"), STUCK, None).await;
        put_file(
            target,
            "write-test.txt",
            "a change to merge\n",
            Some("feature/write-test"),
        )
        .await;

        let (client, _dir) = client_for(target, &target.admin_token, &[]).await;

        // pull_request: create, refuse a moved head, refuse unfinished CI, merge.
        let created = common::try_call(
            &client,
            "pull_request",
            args(json!({"action": "create", "head": "feature/write-test", "title": "Write test"})),
        )
        .await;
        let pr = match created {
            Ok(v) => v["pull_request"].clone(),
            Err(e) => {
                check(false, format!("create failed: {e}"));
                continue;
            }
        };
        let number = pr["number"].as_u64().unwrap_or_default();
        let head = pr["head_sha"].as_str().unwrap_or_default().to_owned();
        check(
            pr["base"] == "main" && pr["state"] == "open",
            format!("created {pr}"),
        );

        let moved = common::try_call(
            &client,
            "pull_request",
            args(json!({"action": "merge", "number": number, "expected_head_sha": "0000000000"})),
        )
        .await;
        check(
            moved.as_ref().is_err_and(|e| e.contains("has moved")),
            format!("merge with a wrong head: {moved:?}"),
        );
        let unready = common::try_call(
            &client,
            "pull_request",
            args(json!({"action": "merge", "number": number, "expected_head_sha": head})),
        )
        .await;
        check(
            unready.as_ref().is_err_and(|e| e.contains("isn't ready")),
            format!("merge before CI passed: {unready:?}"),
        );
        let merged = common::try_call(
            &client,
            "pull_request",
            args(json!({"action": "merge", "number": number, "expected_head_sha": head, "require_ci": false, "delete_branch": true})),
        )
        .await;
        check(
            merged.as_ref().is_ok_and(|v| v["merged"] == true),
            format!("merge: {merged:?}"),
        );
        check(
            merged.as_ref().is_ok_and(|v| {
                v["merge_commit_sha"]
                    .as_str()
                    .is_some_and(|sha| sha.len() == 40 && sha != head)
            }),
            format!("merge commit sha: {merged:?}"),
        );
        check(
            merged.as_ref().is_ok_and(|v| v["branch_deleted"] == true),
            format!("branch deleted after merge: {merged:?}"),
        );

        // pull_request merge_when_ready: a base branch whose protection
        // requires a check nobody reports, so CI on the pull request can
        // never finish. The merge is scheduled, stays unmerged, and is
        // cancelled.
        put_file(
            target,
            "auto-base.txt",
            "the auto-merge base\n",
            Some("auto-base"),
        )
        .await;
        admin_post(
            target,
            "/branch_protections",
            json!({
                "rule_name": "auto-base", "branch_name": "auto-base",
                "enable_status_check": true,
                "status_check_contexts": ["contract / never-reported"]
            }),
        )
        .await;
        put_file(
            target,
            "auto-merge.txt",
            "a change to merge when ready\n",
            Some("feature/auto-merge"),
        )
        .await;
        let auto = common::try_call(
            &client,
            "pull_request",
            args(json!({
                "action": "create", "head": "feature/auto-merge", "base": "auto-base",
                "title": "Merge when ready"
            })),
        )
        .await;
        match auto {
            Ok(created) => {
                let auto_number = created["pull_request"]["number"]
                    .as_u64()
                    .unwrap_or_default();
                let auto_head = created["pull_request"]["head_sha"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let moved = common::try_call(
                    &client,
                    "pull_request",
                    args(json!({
                        "action": "merge_when_ready", "number": auto_number,
                        "expected_head_sha": "0000000000"
                    })),
                )
                .await;
                check(
                    moved.as_ref().is_err_and(|e| e.contains("has moved")),
                    format!("merge_when_ready with a wrong head: {moved:?}"),
                );
                let scheduled = common::try_call(
                    &client,
                    "pull_request",
                    args(json!({
                        "action": "merge_when_ready", "number": auto_number,
                        "expected_head_sha": auto_head
                    })),
                )
                .await;
                check(
                    scheduled
                        .as_ref()
                        .is_ok_and(|v| v["scheduled"] == true && v["merged"] == false),
                    format!("merge_when_ready: {scheduled:?}"),
                );
                // Gitea looks at the checks as soon as it schedules; Forgejo
                // when a status arrives. Neither may merge.
                tokio::time::sleep(Duration::from_secs(5)).await;
                let still = common::call(
                    &client,
                    "pull_request",
                    args(json!({"action": "get", "number": auto_number})),
                )
                .await;
                check(
                    still["state"] == "open",
                    format!("scheduled merge went ahead: {}", still["state"]),
                );
                let cancelled = common::try_call(
                    &client,
                    "pull_request",
                    args(json!({"action": "cancel_auto_merge", "number": auto_number})),
                )
                .await;
                check(
                    cancelled.as_ref().is_ok_and(|v| v["cancelled"] == true),
                    format!("cancel_auto_merge: {cancelled:?}"),
                );
                let again = common::try_call(
                    &client,
                    "pull_request",
                    args(json!({"action": "cancel_auto_merge", "number": auto_number})),
                )
                .await;
                check(
                    again.as_ref().is_ok_and(|v| v["cancelled"] == false),
                    format!("cancel_auto_merge with nothing scheduled: {again:?}"),
                );
            }
            Err(e) => check(false, format!("create for merge_when_ready: {e}")),
        }

        // ci_control: dispatch a run that can never start, then cancel it.
        let dispatched = common::try_call(
            &client,
            "ci_control",
            args(json!({"action": "dispatch", "workflow": "stuck.yml"})),
        )
        .await;
        check(dispatched.is_ok(), format!("dispatch: {dispatched:?}"));
        let mut stuck_run = dispatched
            .as_ref()
            .ok()
            .and_then(|v| v["started"]["run_number"].as_u64());
        if stuck_run.is_none() {
            // Gitea before 1.26 doesn't say which run it started.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let status = common::call(&client, "ci_status", args(json!({"branch": "main"}))).await;
            stuck_run = status["runs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["workflow"].as_str().is_some_and(|w| w.contains("stuck")))
                .and_then(|r| r["number"].as_u64());
        }
        let cancel = match stuck_run {
            Some(n) => {
                common::try_call(
                    &client,
                    "ci_control",
                    args(json!({"action": "cancel", "run": n})),
                )
                .await
            }
            None => Err("the dispatched run wasn't found".into()),
        };
        if can_cancel {
            check(cancel.is_ok(), format!("cancel: {cancel:?}"));
            if let Some(n) = stuck_run {
                let waited = common::call(
                    &client,
                    "ci_wait",
                    args(json!({"run": n, "timeout_seconds": 60})),
                )
                .await;
                check(
                    waited["verdict"] == "failure" && waited["runs"][0]["state"] == "cancelled",
                    format!(
                        "cancelled run: {} {}",
                        waited["verdict"], waited["runs"][0]["state"]
                    ),
                );
            }
        } else {
            check(
                cancel
                    .as_ref()
                    .is_err_and(|e| e.contains("isn't available") && e.contains("or later")),
                format!("cancel on a version without it: {cancel:?}"),
            );
        }

        // Rerun the failed jobs of a run that is the newest on its branch,
        // and check the job really ran again (failed, not cancelled).
        put_file(
            target,
            "rerun-test.txt",
            "a run to rerun\n",
            Some("feature/rerun-test"),
        )
        .await;
        let first = common::call(
            &client,
            "ci_wait",
            args(
                json!({"branch": "feature/rerun-test", "timeout_seconds": 240, "fail_fast": false}),
            ),
        )
        .await;
        let run = first["runs"][0]["number"].as_u64().unwrap_or_default();
        check(
            first["verdict"] == "failure",
            format!("branch run before rerun: {}", first["verdict"]),
        );
        let rerun = common::try_call(
            &client,
            "ci_control",
            args(json!({"action": "rerun_failed", "run": run})),
        )
        .await;
        if can_rerun {
            check(
                rerun.as_ref().is_ok_and(|v| {
                    v["done"].to_string().contains("fails") && v["notes"] == json!([])
                }),
                format!("rerun_failed: {rerun:?}"),
            );
            let waited = common::call(
                &client,
                "ci_wait",
                args(json!({"run": run, "timeout_seconds": 240, "fail_fast": false})),
            )
            .await;
            let fails = waited["runs"][0]["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|j| j["name"] == "fails")
                .map(|j| j["state"].clone());
            check(
                waited["timed_out"] == false && fails == Some(json!("failure")),
                format!("after rerun: verdict {} fails={fails:?}", waited["verdict"]),
            );
        } else {
            check(
                rerun.as_ref().is_err_and(|e| e.contains("isn't available")),
                format!("rerun on a version without it: {rerun:?}"),
            );
        }

        // ci_wait on a finished run returns at once.
        let quick = common::call(
            &client,
            "ci_wait",
            args(json!({"sha": target.ci_sha, "timeout_seconds": 30})),
        )
        .await;
        check(
            quick["waited_seconds"].as_u64().is_some_and(|s| s < 10),
            format!(
                "wait on a finished commit took {}s",
                quick["waited_seconds"]
            ),
        );
        client.cancel().await.unwrap();

        // A read-only collaborator: no ci_control, no create or merge.
        let (reader, _rdir) = client_for(target, &target.reader_token, &[]).await;
        let names: Vec<String> = common::tools(&reader)
            .await
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        check(
            !names.contains(&"ci_control".to_owned()),
            format!("reader tools {names:?}"),
        );
        let refused = common::try_call(
            &reader,
            "pull_request",
            args(json!({"action": "merge", "number": number})),
        )
        .await;
        check(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("isn't available")),
            format!("reader merge: {refused:?}"),
        );
        let refused = common::try_call(
            &reader,
            "pull_request",
            args(json!({"action": "merge_when_ready", "number": number})),
        )
        .await;
        check(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("isn't available")),
            format!("reader merge_when_ready: {refused:?}"),
        );
        reader.cancel().await.unwrap();

        // Read-only mode: the admin gets no writes either.
        let (ro, _rodir) = client_for(
            target,
            &target.admin_token,
            &[("GITFORGE_MCP_READ_ONLY", "true")],
        )
        .await;
        let names: Vec<String> = common::tools(&ro)
            .await
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        check(
            !names.contains(&"ci_control".to_owned()),
            format!("read-only tools {names:?}"),
        );
        let refused = common::try_call(
            &ro,
            "pull_request",
            args(json!({"action": "create", "title": "x"})),
        )
        .await;
        check(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("read-only mode")),
            format!("read-only create: {refused:?}"),
        );
        let refused = common::try_call(
            &ro,
            "pull_request",
            args(json!({"action": "cancel_auto_merge", "number": number})),
        )
        .await;
        check(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("read-only mode")),
            format!("read-only cancel_auto_merge: {refused:?}"),
        );
        ro.cancel().await.unwrap();
    }
    assert!(
        failures.is_empty(),
        "contract failures:\n  {}",
        failures.join("\n  ")
    );
}
