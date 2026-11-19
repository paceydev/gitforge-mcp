//! Contract tests against a real Jenkins LTS beside a real Forgejo, started
//! by scripts/contract/provision.sh (jenkins.sh): core Jenkins with no
//! plugins, two freestyle jobs (widgets-deploy takes TARGET and FAST;
//! widgets-slow sleeps until stopped), and a repository whose CI is
//! Jenkins's alone, through one commit status linking to widgets-deploy
//! build 1.
//!
//! The writes, each checked afterwards in Jenkins's own API: build 1 is
//! rebuilt with its parameters; widgets-slow is started by dispatch and,
//! once a status links to it, stopped by cancel; a dispatch signed in with
//! the password rather than the API token carries the CSRF crumb and the
//! session it is tied to, which Jenkins insists on for a password; a
//! parameter the job lacks and a branch for a lone job are refused before
//! anything is sent; and read-only mode offers no writes.
//!
//! Skipped unless GITFORGE_CONTRACT_TARGETS names the targets file;
//! GITFORGE_CONTRACT_REQUIRED=1 turns the skip into a failure, as CI does.

mod common;

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    base_url: String,
    admin_token: String,
    #[serde(default)]
    jenkins: Option<Jenkins>,
}

#[derive(Debug, Deserialize)]
struct Jenkins {
    version: String,
    base_url: String,
    user: String,
    token: String,
    password: String,
    repo: String,
    sha: String,
}

fn targets() -> Option<Vec<Target>> {
    let required = std::env::var("GITFORGE_CONTRACT_REQUIRED").as_deref() == Ok("1");
    let Ok(path) = std::env::var("GITFORGE_CONTRACT_TARGETS") else {
        assert!(
            !required,
            "GITFORGE_CONTRACT_REQUIRED=1 but GITFORGE_CONTRACT_TARGETS is not set"
        );
        eprintln!("skipping Jenkins contract tests: GITFORGE_CONTRACT_TARGETS is not set");
        return None;
    };
    let targets: Vec<Target> =
        serde_json::from_str(&std::fs::read_to_string(path).expect("targets")).expect("JSON");
    if targets.iter().all(|t| t.jenkins.is_none()) {
        // `provision.sh --only` can leave Jenkins's forge out.
        eprintln!("skipping Jenkins contract tests: no target has a Jenkins");
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

/// The server for the Jenkins repository, signed in to the forge as its
/// admin and to Jenkins as `credential` (`user:secret`).
async fn client_for(
    target: &Target,
    jk: &Jenkins,
    credential: &str,
    extra: &[(&str, &str)],
) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::git_checkout(
        dir.path(),
        &format!("{}/gfadmin/{}.git", target.base_url, jk.repo),
    );
    let forge_var = gitforge_mcp::auth::default_env_var(host_of(&target.base_url));
    let jk_var = gitforge_mcp::auth::default_env_var(host_of(&jk.base_url));
    let mut env = vec![
        (forge_var.as_str(), target.admin_token.as_str()),
        (jk_var.as_str(), credential),
    ];
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
        .unwrap_or_else(|| panic!("no Jenkins {feature} in {report}"))
}

/// `path` from Jenkins's JSON API, read with the API token.
async fn jenkins_json(jk: &Jenkins, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{}{path}", jk.base_url))
        .basic_auth(&jk.user, Some(&jk.token))
        .send()
        .await
        .expect("Jenkins answers")
        .error_for_status()
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .json()
        .await
        .expect("JSON")
}

/// Build `number` of `job` once it has a result, with its parameters as
/// name=value.
async fn finished(jk: &Jenkins, job: &str, number: u64) -> (String, Vec<String>) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let build = jenkins_json(
            jk,
            &format!("/job/{job}/{number}/api/json?tree=result,actions[parameters[name,value]]"),
        )
        .await;
        if let Some(result) = build["result"].as_str() {
            let params = build["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|a| a["parameters"].as_array().into_iter().flatten())
                .map(|p| format!("{}={}", p["name"].as_str().unwrap_or("?"), p["value"]))
                .collect();
            return (result.to_owned(), params);
        }
        assert!(
            Instant::now() < deadline,
            "{job} #{number} never finished: {build}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The number of `job`'s newest build, once it has one.
async fn last_build(jk: &Jenkins, job: &str) -> Option<u64> {
    jenkins_json(jk, &format!("/job/{job}/api/json?tree=lastBuild[number]"))
        .await["lastBuild"]["number"]
        .as_u64()
}

/// The build a dispatch started: from its reply, or, if it was still in
/// the queue, the job's newest once there is one past `before`.
async fn started_build(jk: &Jenkins, job: &str, reply: &Value, before: Option<u64>) -> u64 {
    if let Some(n) = reply["started"]["run_number"].as_u64() {
        return n;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(n) = last_build(jk, job).await.filter(|n| Some(*n) > before) {
            return n;
        }
        assert!(Instant::now() < deadline, "{job} never started: {reply}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn post_status(target: &Target, jk: &Jenkins, status: Value) {
    reqwest::Client::new()
        .post(format!(
            "{}/api/v1/repos/gfadmin/{}/statuses/{}",
            target.base_url, jk.repo, jk.sha
        ))
        .header("Authorization", format!("token {}", target.admin_token))
        .json(&status)
        .send()
        .await
        .expect("the forge answers")
        .error_for_status()
        .expect("the status is posted");
}

#[tokio::test]
async fn jenkins_writes_match_a_real_jenkins() {
    let Some(targets) = targets() else { return };
    for target in &targets {
        let Some(jk) = &target.jenkins else { continue };
        eprintln!("== Jenkins {} beside {}", jk.version, target.name);
        let credential = format!("{}:{}", jk.user, jk.token);
        let (client, _dir) = client_for(target, jk, &credential, &[]).await;
        let at = |extra: Value| {
            let mut a = args(extra);
            a.insert("sha".into(), json!(jk.sha));
            a
        };

        let report = common::forge_context(&client, false).await;
        let system = &report["ci_systems"][0];
        assert_eq!(system["kind"], "jenkins", "{report}");
        assert_eq!(system["version"], jk.version.as_str(), "{report}");
        assert_eq!(system["user"]["login"], jk.user.as_str(), "{report}");
        for feature in ["ci_cancel", "ci_rerun", "ci_dispatch"] {
            assert_eq!(
                provider_row(&report, feature)["available"],
                true,
                "{report}"
            );
        }

        // Rerun: build 1 had TARGET=staging; the new build has it too.
        let before = last_build(jk, "widgets-deploy").await;
        let rerun = common::call(&client, "ci_control", at(json!({ "action": "rerun" }))).await;
        let done = rerun["done"][0].as_str().unwrap_or_default();
        assert!(done.starts_with("head: build 1 built again as "), "{rerun}");
        let rebuilt = started_build(jk, "widgets-deploy", &json!({}), before).await;
        let (result, params) = finished(jk, "widgets-deploy", rebuilt).await;
        assert_eq!(result, "SUCCESS");
        assert!(
            params.contains(&"TARGET=\"staging\"".to_owned()),
            "{params:?}"
        );

        // Dispatch a lone job; it takes no branch, so `ref` is refused.
        let refused = common::try_call(
            &client,
            "ci_control",
            args(json!({ "action": "dispatch", "workflow": "widgets-slow", "ref": "main" })),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("isn't a multibranch project"), "{refused}");
        let before = last_build(jk, "widgets-slow").await;
        let slow = common::call(
            &client,
            "ci_control",
            args(json!({ "action": "dispatch", "ci": "jenkins", "workflow": "widgets-slow" })),
        )
        .await;
        let n = started_build(jk, "widgets-slow", &slow, before).await;

        // Cancel, once a status links the commit to the running build.
        post_status(
            target,
            jk,
            json!({
                "state": "pending",
                "context": format!("gfadmin/{}/pipeline/slow", jk.repo),
                "target_url": format!("{}/job/widgets-slow/{n}/display/redirect", jk.base_url),
                "description": "This commit is being built",
            }),
        )
        .await;
        let cancelled =
            common::call(&client, "ci_control", at(json!({ "action": "cancel" }))).await;
        assert_eq!(
            cancelled["done"],
            json!([format!(
                "slow: build {n} of {}/job/widgets-slow/",
                jk.base_url
            )]),
            "{cancelled}"
        );
        assert_eq!(finished(jk, "widgets-slow", n).await.0, "ABORTED");

        // A parameter the job doesn't define: Jenkins would drop it without
        // a word, so nothing is sent.
        let before = last_build(jk, "widgets-deploy").await;
        let unknown = common::try_call(
            &client,
            "ci_control",
            args(json!({ "action": "dispatch", "workflow": "widgets-deploy",
                         "inputs": { "TAGRET": "prod" } })),
        )
        .await
        .unwrap_err();
        assert!(unknown.contains("has no parameter TAGRET"), "{unknown}");
        assert_eq!(last_build(jk, "widgets-deploy").await, before);
        client.cancel().await.unwrap();

        // With the password, Jenkins wants the crumb and its session.
        let with_password = format!("{}:{}", jk.user, jk.password);
        let (client, _dir) = client_for(target, jk, &with_password, &[]).await;
        let deployed = common::call(
            &client,
            "ci_control",
            args(json!({ "action": "dispatch", "workflow": "widgets-deploy",
                         "inputs": { "TARGET": "prod", "FAST": true } })),
        )
        .await;
        let m = started_build(jk, "widgets-deploy", &deployed, before).await;
        let (result, params) = finished(jk, "widgets-deploy", m).await;
        assert_eq!(result, "SUCCESS");
        assert!(
            params.contains(&"TARGET=\"prod\"".to_owned())
                && params.contains(&"FAST=true".to_owned()),
            "{params:?}"
        );
        client.cancel().await.unwrap();

        // Read-only mode offers no writes.
        let (client, _dir) = client_for(
            target,
            jk,
            &credential,
            &[("GITFORGE_MCP_READ_ONLY", "true")],
        )
        .await;
        assert!(
            !common::tool_names(&client)
                .await
                .contains(&"ci_control".to_owned())
        );
        let report = common::forge_context(&client, false).await;
        assert_eq!(
            provider_row(&report, "ci_cancel")["reason"],
            "the server is in read-only mode (--read-only)"
        );
        client.cancel().await.unwrap();
    }
}
