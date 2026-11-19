//! Contract tests against a real Woodpecker beside a real Forgejo, started
//! by scripts/contract/provision.sh (woodpecker.sh): a repository whose CI
//! is Woodpecker's alone, with pipeline 1 from a push (one workflow passes,
//! one fails) and an agent on the local backend.
//!
//! Reads first: what forge_context, ci_status, ci_logs and ci_runners make
//! of it, and that every Woodpecker endpoint the driver uses is in the
//! server's own API spec. Then the writes: a manual pipeline is started and
//! cancelled, and pipeline 1 is restarted; rerun_failed is refused, and so
//! is everything in read-only mode and without a Woodpecker token.
//!
//! Skipped unless GITFORGE_CONTRACT_TARGETS names the targets file;
//! GITFORGE_CONTRACT_REQUIRED=1 turns the skip into a failure, as CI does.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    base_url: String,
    admin_token: String,
    woodpecker: Option<Woodpecker>,
}

#[derive(Debug, Deserialize)]
struct Woodpecker {
    version: String,
    base_url: String,
    token: String,
    repo: String,
    push_sha: String,
}

fn targets() -> Option<Vec<Target>> {
    let required = std::env::var("GITFORGE_CONTRACT_REQUIRED").as_deref() == Ok("1");
    let Ok(path) = std::env::var("GITFORGE_CONTRACT_TARGETS") else {
        assert!(
            !required,
            "GITFORGE_CONTRACT_REQUIRED=1 but GITFORGE_CONTRACT_TARGETS is not set"
        );
        eprintln!("skipping Woodpecker contract tests: GITFORGE_CONTRACT_TARGETS is not set");
        return None;
    };
    let targets: Vec<Target> =
        serde_json::from_str(&std::fs::read_to_string(path).expect("targets")).expect("JSON");
    if targets.iter().all(|t| t.woodpecker.is_none()) {
        // `provision.sh --only` can leave Woodpecker's forge out.
        eprintln!("skipping Woodpecker contract tests: no target has a Woodpecker");
        return None;
    }
    Some(targets)
}

fn host_of(base_url: &str) -> &str {
    base_url.trim_start_matches("http://").trim_end_matches('/')
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// The server for the Woodpecker repository, signed in to the forge as
/// its admin and, unless `wp_token` is `None`, to Woodpecker too.
async fn client_for(
    target: &Target,
    wp: &Woodpecker,
    wp_token: Option<&str>,
    extra: &[(&str, &str)],
) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(
        dir.path(),
        &format!("{}/gfadmin/{}.git", target.base_url, wp.repo),
    );
    let forge_var = gitforge_mcp::auth::default_env_var(host_of(&target.base_url));
    let wp_var = gitforge_mcp::auth::default_env_var(host_of(&wp.base_url));
    let mut env = vec![(forge_var.as_str(), target.admin_token.as_str())];
    if let Some(token) = wp_token {
        env.push((wp_var.as_str(), token));
    }
    env.extend_from_slice(extra);
    let client = common::start(dir.path(), &env).await;
    (client, dir)
}

fn provider_row<'a>(report: &'a Value, feature: &str) -> &'a Value {
    report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == feature && c["provider"].is_string())
        .unwrap_or_else(|| panic!("no Woodpecker {feature} in {report}"))
}

/// ci_status for `selector` once the verdict is final.
async fn settled(client: &common::Client, selector: Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let mut a = args(selector.clone());
        a.insert("jobs".into(), json!("all"));
        let status = common::call(client, "ci_status", a).await;
        let verdict = status["verdict"].as_str().unwrap_or_default().to_owned();
        if !matches!(verdict.as_str(), "pending" | "failing" | "no_runs")
            || Instant::now() > deadline
        {
            return status;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn job_states(run: &Value) -> BTreeSet<String> {
    run["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            format!(
                "{} {}",
                j["name"].as_str().unwrap_or("?"),
                j["state"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

/// Every path in Woodpecker's OpenAPI document, parameters erased.
async fn spec_paths(wp: &Woodpecker) -> BTreeSet<String> {
    let spec: Value = reqwest::Client::new()
        .get(format!("{}/swagger/doc.json", wp.base_url))
        .send()
        .await
        .expect("Woodpecker's spec is served")
        .json()
        .await
        .expect("spec JSON");
    spec["paths"]
        .as_object()
        .expect("spec paths")
        .keys()
        .map(|p| common::erase_params(p))
        .collect()
}

/// The endpoints src/external/woodpecker.rs calls, under `/api`.
const USED: &[&str] = &[
    "/user",
    "/repos/lookup/{}",
    "/repos/{}/permissions",
    "/repos/{}/pipelines",
    "/repos/{}/pipelines/{}",
    "/repos/{}/pipelines/{}/cancel",
    "/repos/{}/logs/{}/{}",
    "/agents",
    "/queue/info",
];

async fn reads(target: &Target, wp: &Woodpecker) {
    let name = format!("woodpecker {}", host_of(&wp.base_url));
    let paths = spec_paths(wp).await;
    for used in USED {
        assert!(
            paths.contains(*used),
            "{}: {used} isn't in Woodpecker's spec",
            target.name
        );
    }

    let (client, _dir) = client_for(target, wp, Some(&wp.token), &[]).await;
    let report = common::forge_context(&client, false).await;
    let system = &report["ci_systems"][0];
    assert_eq!(system["name"], name.as_str(), "{report}");
    assert_eq!(system["version"], wp.version.as_str());
    assert_eq!(system["found_by"], "commit status link");
    assert_eq!(
        system["user"],
        json!({ "login": "gfadmin", "site_admin": true })
    );
    for feature in [
        "ci_jobs",
        "ci_logs",
        "ci_cancel",
        "ci_rerun",
        "ci_dispatch",
        "ci_runners",
        "ci_queue",
    ] {
        assert_eq!(
            provider_row(&report, feature)["available"],
            true,
            "{feature}: {report}"
        );
    }
    assert_eq!(provider_row(&report, "ci_rerun_failed")["available"], false);

    // The push's pipeline (pipeline 1, or its restart when the writes
    // below have run before), beside any manual ones on the same commit.
    let status = settled(&client, json!({ "sha": wp.push_sha })).await;
    assert_eq!(status["verdict"], "failure", "{status}");
    let run = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["event"] == "push")
        .unwrap_or_else(|| panic!("no push pipeline in {status}"));
    assert_eq!(run["external"], "ci/woodpecker/push");
    assert_eq!(
        job_states(run),
        BTreeSet::from(["build success".to_owned(), "test failure".to_owned()]),
        "{status}"
    );
    let test = run["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["name"] == "test")
        .unwrap();
    assert_eq!(test["source"], "api");
    assert_eq!(test["steps"][1]["name"], "unit", "{status}");
    assert_eq!(test["steps"][1]["state"], "failure");

    let logs = common::call(
        &client,
        "ci_logs",
        args(json!({ "sha": wp.push_sha, "job": "test" })),
    )
    .await;
    let text = logs["jobs"][0]["log"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("contract-woodpecker-failure"), "{logs}");
    assert!(text.starts_with("--- step unit (failure) ---"), "{logs}");

    let runners = common::call(&client, "ci_runners", Default::default()).await;
    let agents = &runners["ci_systems"][0]["runners"];
    assert!(
        agents
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "contract-agent"
                && matches!(a["state"].as_str(), Some("idle" | "busy"))),
        "{runners}"
    );
    client.cancel().await.unwrap();
}

async fn writes(target: &Target, wp: &Woodpecker) {
    let (client, _dir) = client_for(target, wp, Some(&wp.token), &[]).await;

    // A manual pipeline: build, and a workflow that sleeps until cancelled.
    let started = common::call(
        &client,
        "ci_control",
        args(json!({ "action": "dispatch", "inputs": { "CONTRACT": "yes" } })),
    )
    .await;
    let manual = started["started"]["run_number"]
        .as_u64()
        .unwrap_or_else(|| panic!("{started}"));
    // Cancel once the slow workflow is running, so it is stopped, not
    // merely never started.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !job_states(
        &common::call(
            &client,
            "ci_status",
            args(json!({ "run": manual, "jobs": "all" })),
        )
        .await["runs"][0],
    )
    .contains("slow running")
    {
        assert!(
            Instant::now() < deadline,
            "manual pipeline {manual} never ran"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let cancelled = common::call(
        &client,
        "ci_control",
        args(json!({ "action": "cancel", "run": manual })),
    )
    .await;
    assert_eq!(cancelled["run"], manual, "{cancelled}");
    let after = settled(&client, json!({ "run": manual })).await;
    assert_eq!(after["runs"][0]["state"], "cancelled", "{after}");
    assert!(
        job_states(&after["runs"][0]).contains("slow cancelled"),
        "{after}"
    );

    restart(&client).await;
    client.cancel().await.unwrap();

    // Read-only mode, and no Woodpecker token: nothing that writes. The
    // repository is private, so without a token Woodpecker refuses even to
    // look it up, and says why.
    let (client, _dir) = client_for(
        target,
        wp,
        Some(&wp.token),
        &[("GITFORGE_MCP_READ_ONLY", "true")],
    )
    .await;
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_control".to_owned())
    );
    client.cancel().await.unwrap();
    let (client, _dir) = client_for(target, wp, None, &[]).await;
    let report = common::forge_context(&client, false).await;
    assert!(
        report["ci_systems"][0]["problem"]
            .as_str()
            .is_some_and(|p| p.contains("no credential was found for this host")),
        "{report}"
    );
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_control".to_owned())
    );
    client.cancel().await.unwrap();
}

/// Pipeline 1 is restarted as a new pipeline, which fails the same way;
/// rerunning only its failed workflows is refused.
async fn restart(client: &common::Client) {
    let refused = common::try_call(
        client,
        "ci_control",
        args(json!({ "action": "rerun_failed", "run": 1 })),
    )
    .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("Woodpecker has no API for this")),
        "{refused:?}"
    );
    let rerun = common::call(
        client,
        "ci_control",
        args(json!({ "action": "rerun", "run": 1 })),
    )
    .await;
    let restarted = rerun["done"][0].as_str().unwrap_or_default().to_owned();
    assert!(
        restarted.starts_with("pipeline 1 restarted as pipeline "),
        "{rerun}"
    );
    let number: u64 = restarted.rsplit(' ').next().unwrap().parse().unwrap();
    let again = settled(client, json!({ "run": number })).await;
    assert_eq!(again["verdict"], "failure", "{again}");
    assert_eq!(
        job_states(&again["runs"][0]),
        BTreeSet::from(["build success".to_owned(), "test failure".to_owned()]),
        "{again}"
    );
}

#[tokio::test]
async fn woodpecker_matches_its_api() {
    let Some(targets) = targets() else {
        return;
    };
    for target in &targets {
        let Some(wp) = &target.woodpecker else {
            continue;
        };
        eprintln!("== {} beside {}", wp.base_url, target.name);
        reads(target, wp).await;
        writes(target, wp).await;
    }
}
