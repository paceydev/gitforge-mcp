//! The real binary against a fake Gitea whose repository builds on a fake
//! Jenkins: the statuses lead to Jenkins's JSON API, which gives each
//! build's result, stages and timings, its console log, and the nodes and
//! queue; and builds are stopped, started and rebuilt through the forms
//! Jenkins's pages post, with the CSRF crumb and its session cookie.

mod common;

use common::forge::{TOKEN, fake_gitea, git, one_commit, statuses, token_env, without_actions};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `dev:jenkins-token-value`, as Basic auth sends it.
const BASIC: &str = "Basic ZGV2OmplbmtpbnMtdG9rZW4tdmFsdWU=";

const MAIN: &str = "/job/acme/job/widgets/job/main";
const PR: &str = "/job/acme/job/widgets/job/PR-3";

struct Setup {
    dir: TempDir,
    forge: MockServer,
    jk: MockServer,
}

impl Setup {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.jk.address().port())
    }

    /// Starts the server with the forge's token and, if given, `jenkins`
    /// as the Jenkins credential.
    async fn start(&self, jenkins: Option<&str>) -> common::Client {
        self.start_with(jenkins, &[]).await
    }

    async fn start_with(&self, jenkins: Option<&str>, extra: &[(&str, &str)]) -> common::Client {
        let forge_var = token_env(&self.forge);
        let jk_var = token_env(&self.jk);
        let mut env = vec![(forge_var.as_str(), TOKEN)];
        if let Some(credential) = jenkins {
            env.push((jk_var.as_str(), credential));
        }
        env.extend_from_slice(extra);
        common::start(self.dir.path(), &env).await
    }

    fn name(&self) -> String {
        format!("jenkins {}", self.host())
    }

    /// `GET` `route` on Jenkins answers `body`, only with the credential.
    async fn get(&self, route: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", BASIC))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Jenkins", "2.580.2")
                    .set_body_json(body),
            )
            .mount(&self.jk)
            .await;
    }
}

/// A Gitea repository with Actions off, whose HEAD Jenkins built twice:
/// `main` #7 and `PR-3` #2, as the Gitea plugin reports them. Jenkins
/// names itself on every response and refuses anyone without the
/// credential, as a locked-down Jenkins does.
async fn jenkins_repo() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let sha = one_commit(dir.path());
    let forge = fake_gitea("1.25.3").await;
    without_actions(&forge).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/branches/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "main", "commit": { "id": sha } })),
        )
        .mount(&forge)
        .await;
    let jk = MockServer::start().await;
    let status = |id: u64, context: &str, job: &str, number: u64, state: &str| {
        json!({
            "id": id, "status": state, "context": format!("acme/widgets/pipeline/{context}"),
            "target_url": format!("{}{job}/{number}/display/redirect", jk.uri()),
            "description": "This commit is being built", "created_at": "2026-11-09T10:00:00Z"
        })
    };
    statuses(
        &forge,
        &sha,
        json!([
            status(1, "head", MAIN, 7, "pending"),
            status(2, "pr-main", PR, 2, "pending")
        ]),
    )
    .await;
    git(
        dir.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/acme/widgets.git", forge.uri()),
        ],
    );
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Jenkins", "2.580.2"))
        .mount(&jk)
        .await;
    let setup = Setup { dir, forge, jk };
    setup
        .get(
            "/whoAmI/api/json",
            json!({ "name": "dev", "anonymous": false, "authenticated": true }),
        )
        .await;
    setup
        .get(&format!("{MAIN}/api/json"), json!({ "name": "main" }))
        .await;
    setup
}

/// Everything not mounted with the credential is refused, as Jenkins
/// refuses an anonymous caller, except asking who it is.
async fn refuse_the_rest(jk: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/whoAmI/api/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "anonymous", "anonymous": true })),
        )
        .with_priority(9)
        .mount(jk)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).insert_header("X-Jenkins", "2.580.2"))
        .with_priority(10)
        .mount(jk)
        .await;
}

fn row<'a>(report: &'a Value, feature: &str, provider: &str) -> &'a Value {
    report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == feature && c["provider"] == provider)
        .unwrap_or_else(|| panic!("no {feature} for {provider} in {report}"))
}

/// `main` #7 failed at its Test stage; `PR-3` #2 has started, but its
/// node is still in the queue, waiting for an arm64 node there isn't.
async fn mount_builds(setup: &Setup) {
    setup
        .get(
            &format!("{MAIN}/7/api/json"),
            json!({ "number": 7, "result": "FAILURE", "building": false,
                    "timestamp": 1_794_000_000_000_i64, "duration": 95_000 }),
        )
        .await;
    setup
        .get(
            &format!("{MAIN}/7/wfapi/describe"),
            json!({ "status": "FAILED", "stages": [
                { "name": "Build", "status": "SUCCESS", "durationMillis": 4_000 },
                { "name": "Test", "status": "FAILED", "durationMillis": 80_000,
                  "error": { "message": "script returned exit code 1" } },
                { "name": "Deploy", "status": "FAILED", "durationMillis": 10,
                  "error": { "message": "script returned exit code 1" } }
            ] }),
        )
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MAIN}/7/consoleText")))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "Started by an SCM change\n[Pipeline] sh\nFAIL: TestWidget\nFinished: FAILURE\n",
        ))
        .mount(&setup.jk)
        .await;
    setup
        .get(
            &format!("{PR}/2/api/json"),
            json!({ "number": 2, "result": null, "building": true,
                    "timestamp": 1_794_000_100_000_i64, "duration": 0 }),
        )
        .await;
    setup
        .get(
            "/queue/api/json",
            json!({ "items": [
                { "why": "‘Jenkins’ doesn’t have label ‘arm64’",
                  "task": { "name": "part of acme » widgets » PR-3 #2", "url": "job/acme/job/widgets/job/PR-3/2/" } },
                { "why": "Waiting for next available executor",
                  "task": { "name": "nightly", "url": "job/nightly/" } }
            ] }),
        )
        .await;
    setup
        .get(
            "/computer/api/json",
            json!({ "computer": [
                { "displayName": "Built-In Node", "idle": true, "offline": false,
                  "assignedLabels": [{ "name": "built-in" }, { "name": "linux" }] }
            ] }),
        )
        .await;
}

#[tokio::test]
async fn builds_stages_logs_and_nodes_come_from_jenkins() {
    let setup = jenkins_repo().await;
    mount_builds(&setup).await;
    refuse_the_rest(&setup.jk).await;
    let client = setup.start(Some("dev:jenkins-token-value")).await;
    let name = setup.name();

    // Actions is off; Jenkins gives logs, nodes and control of its builds.
    assert_eq!(
        common::tool_names(&client).await,
        [
            "ci_control",
            "ci_logs",
            "ci_runners",
            "ci_status",
            "ci_wait",
            "forge_context",
            "pull_request"
        ]
    );
    let report = common::forge_context(&client, false).await;
    let system = &report["ci_systems"][0];
    assert_eq!(system["name"], name.as_str(), "{report}");
    assert_eq!(system["kind"], "jenkins");
    assert_eq!(system["version"], "2.580.2");
    assert_eq!(system["user"]["login"], "dev");
    assert_eq!(system["found_by"], "commit status link");
    assert!(system.get("problem").is_none(), "{system}");
    assert_eq!(row(&report, "ci_logs", &name)["available"], true);
    let cancel = row(&report, "ci_cancel", &name);
    assert_eq!(cancel["available"], true, "{report}");
    assert_eq!(
        cancel["note"],
        "Jenkins doesn't say what a user may do; a refusal means it lacks Job/Cancel"
    );
    assert_eq!(
        row(&report, "ci_rerun_failed", &name)["reason"],
        "Jenkins has no API for this"
    );

    let status = common::call(&client, "ci_status", Default::default()).await;
    let run = &status["runs"][0];
    assert_eq!(run["external"], "acme/widgets/pipeline", "{status}");
    // One build failed and one waits for a node: not finished.
    assert_eq!(status["verdict"], "failing", "{status}");
    assert_eq!(run["state"], "waiting", "{status}");
    let jobs: Vec<_> = run["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| {
            (
                j["name"].as_str().unwrap(),
                j["state"].as_str().unwrap(),
                j["source"].as_str().unwrap(),
                j["job_id"].as_u64(),
            )
        })
        .collect();
    assert_eq!(
        jobs,
        [
            ("head", "failure", "api", Some(7)),
            ("pr-main", "waiting", "api", Some(2))
        ],
        "{status}"
    );
    let head = &run["jobs"][0];
    assert_eq!(head["duration_seconds"], 95);
    assert_eq!(head["url"], format!("{}{MAIN}/7/", setup.jk.uri()));
    assert_eq!(
        head["steps"],
        json!([
            { "name": "Build", "state": "success", "duration_seconds": 4 },
            { "name": "Test", "state": "failure", "duration_seconds": 80 },
            { "name": "Deploy", "state": "skipped", "duration_seconds": 0 }
        ])
    );

    let logs = common::call(&client, "ci_logs", Default::default()).await;
    let job = &logs["jobs"][0];
    assert_eq!(job["name"], "head", "{logs}");
    assert!(
        job["log"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("FAIL: TestWidget")),
        "{logs}"
    );

    let runners = common::call(&client, "ci_runners", Default::default()).await;
    let jenkins = &runners["ci_systems"][0];
    assert_eq!(jenkins["system"], name.as_str(), "{runners}");
    assert_eq!(jenkins["runners"][0]["name"], "Built-In Node");
    assert_eq!(jenkins["runners"][0]["labels"], json!(["linux"]));
    // Only this repository's builds are waiting here; the nightly job is
    // someone else's.
    let waiting = jenkins["waiting"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{runners}");
    assert_eq!(waiting[0]["labels"], json!(["arm64"]));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn without_a_usable_jenkins_credential_its_statuses_stay_as_they_are() {
    let setup = jenkins_repo().await;
    refuse_the_rest(&setup.jk).await;
    let host = setup.host();

    // A token alone: Jenkins wants the user it belongs to.
    let client = setup.start(Some("jenkins-token-value")).await;
    let report = common::forge_context(&client, false).await;
    let problem = report["ci_systems"][0]["problem"]
        .as_str()
        .unwrap_or_default();
    assert!(
        problem.contains(&format!("set username in [ci.\"{host}\"]")),
        "{report}"
    );
    client.cancel().await.unwrap();

    // The same token with the user from the config works.
    std::fs::write(
        setup.dir.path().join(".no-config.toml"),
        format!("[ci.\"{host}\"]\nusername = \"dev\"\n"),
    )
    .unwrap();
    std::fs::remove_file(setup.dir.path().join(".probes.json")).ok();
    let client = setup.start(Some("jenkins-token-value")).await;
    let report = common::forge_context(&client, false).await;
    assert_eq!(report["ci_systems"][0]["user"]["login"], "dev", "{report}");
    client.cancel().await.unwrap();
    std::fs::remove_file(setup.dir.path().join(".no-config.toml")).unwrap();
    std::fs::remove_file(setup.dir.path().join(".probes.json")).ok();

    // No credential: Jenkins refuses, and the statuses are all there is.
    let client = setup.start(None).await;
    let report = common::forge_context(&client, false).await;
    let problem = report["ci_systems"][0]["problem"]
        .as_str()
        .unwrap_or_default();
    assert!(
        problem.contains("no credential was found for this host"),
        "{report}"
    );
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_logs".to_owned())
    );
    let status = common::call(&client, "ci_status", Default::default()).await;
    let jobs = status["runs"][0]["jobs"].as_array().unwrap();
    assert!(
        jobs.iter()
            .all(|j| j["source"] == "statuses" && j["state"] == "running"),
        "{status}"
    );
    client.cancel().await.unwrap();
}

/// The crumb Jenkins issues, tied to the session its cookie names.
const CRUMB: &str = "c0ffee";
const SESSION: &str = "JSESSIONID.node0=abc123";

/// `crumbIssuer` answers a crumb and sets the session cookie, and `POST`
/// `route` answers `reply`, only with the credential, crumb and cookie,
/// `times` times.
async fn mount_post(setup: &Setup, route: &str, reply: ResponseTemplate, times: u64) {
    Mock::given(method("GET"))
        .and(path("/crumbIssuer/api/json"))
        .and(header("authorization", BASIC))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Jenkins", "2.580.2")
                .insert_header(
                    "Set-Cookie",
                    format!("{SESSION}; Path=/; HttpOnly").as_str(),
                )
                .set_body_json(json!({
                    "_class": "hudson.security.csrf.DefaultCrumbIssuer",
                    "crumb": CRUMB, "crumbRequestField": "Jenkins-Crumb"
                })),
        )
        .mount(&setup.jk)
        .await;
    Mock::given(method("POST"))
        .and(path(route))
        .and(header("authorization", BASIC))
        .and(header("Jenkins-Crumb", CRUMB))
        .and(header("cookie", SESSION))
        .respond_with(reply)
        .expect(times)
        .mount(&setup.jk)
        .await;
}

/// A queue item that has become build `number`. Jenkins names the item by
/// its configured root URL, not the one it was reached at.
async fn mount_queued(setup: &Setup, id: u64, number: u64) -> ResponseTemplate {
    setup
        .get(
            &format!("/queue/item/{id}/api/json"),
            json!({ "_class": "hudson.model.Queue$LeftItem", "cancelled": false,
                    "executable": { "number": number } }),
        )
        .await;
    ResponseTemplate::new(201).insert_header(
        "Location",
        format!("https://jenkins.example.com/queue/item/{id}/").as_str(),
    )
}

fn args(value: Value) -> rmcp::model::JsonObject {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn jenkins_builds_are_stopped_and_started() {
    let setup = jenkins_repo().await;
    mount_builds(&setup).await;
    // The multibranch project the statuses link to, and a lone job that
    // takes a parameter.
    setup
        .get(
            "/job/acme/job/widgets/api/json",
            json!({ "jobs": [
                { "name": "main", "url": "https://jenkins.example.com/job/acme/job/widgets/job/main/" },
                { "name": "PR-3", "url": "https://jenkins.example.com/job/acme/job/widgets/job/PR-3/" }
            ] }),
        )
        .await;
    setup
        .get(
            "/job/deploy/api/json",
            json!({ "buildable": true,
                    "property": [{ "parameterDefinitions": [{ "name": "TARGET" }] }] }),
        )
        .await;
    // Stopping: only PR-3 #2 is still going.
    mount_post(
        &setup,
        &format!("{PR}/2/stop"),
        ResponseTemplate::new(302).insert_header("Location", "https://jenkins.example.com/"),
        1,
    )
    .await;
    let built = mount_queued(&setup, 12, 8).await;
    mount_post(&setup, &format!("{MAIN}/build"), built, 1).await;
    let deployed = mount_queued(&setup, 13, 4).await;
    Mock::given(method("POST"))
        .and(path("/job/deploy/buildWithParameters"))
        .and(body_string_contains("TARGET=staging"))
        .and(header("Jenkins-Crumb", CRUMB))
        .respond_with(deployed)
        .expect(1)
        .mount(&setup.jk)
        .await;
    refuse_the_rest(&setup.jk).await;
    let client = setup.start(Some("dev:jenkins-token-value")).await;

    let cancelled = common::call(&client, "ci_control", args(json!({ "action": "cancel" }))).await;
    assert_eq!(
        cancelled["done"],
        json!([format!("pr-main: build 2 of {}{PR}/", setup.jk.uri())]),
        "{cancelled}"
    );

    // With no job named, the repository's one project: its branch job.
    let started = common::call(&client, "ci_control", args(json!({ "action": "dispatch" }))).await;
    assert_eq!(started["ci"], setup.name().as_str(), "{started}");
    assert_eq!(started["ref"], "main");
    assert_eq!(started["started"]["run_number"], 8, "{started}");
    assert_eq!(
        started["started"]["url"],
        format!("{}{MAIN}/8/", setup.jk.uri())
    );

    // A lone job, with its parameter; it builds what it is set up to.
    let deployed = common::call(
        &client,
        "ci_control",
        args(
            json!({ "action": "dispatch", "ci": "jenkins", "workflow": "deploy",
                     "inputs": { "TARGET": "staging" } }),
        ),
    )
    .await;
    assert_eq!(deployed["started"]["run_number"], 4, "{deployed}");
    assert!(
        deployed["notes"][0]
            .as_str()
            .is_some_and(|n| n.contains("builds what it is set up to")),
        "{deployed}"
    );

    // Refused before anything is sent: a parameter the job doesn't have
    // (Jenkins would drop it without a word), and a branch for a lone job.
    let unknown = common::try_call(
        &client,
        "ci_control",
        args(json!({ "action": "dispatch", "workflow": "deploy",
                     "inputs": { "TAGRET": "staging" } })),
    )
    .await
    .unwrap_err();
    assert!(
        unknown.contains("has no parameter TAGRET; it takes TARGET"),
        "{unknown}"
    );
    let branch = common::try_call(
        &client,
        "ci_control",
        args(json!({ "action": "dispatch", "workflow": "deploy", "ref": "release" })),
    )
    .await
    .unwrap_err();
    assert!(branch.contains("isn't a multibranch project"), "{branch}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn jenkins_builds_are_rebuilt_with_their_parameters() {
    let setup = jenkins_repo().await;
    // Both builds have finished; PR-3 #2 had a parameter.
    setup
        .get(
            &format!("{MAIN}/7/api/json"),
            json!({ "number": 7, "result": "FAILURE", "building": false,
                    "timestamp": 1_794_000_000_000_i64, "duration": 95_000,
                    "actions": [{}, { "_class": "hudson.model.CauseAction" }] }),
        )
        .await;
    setup
        .get(
            &format!("{PR}/2/api/json"),
            json!({ "number": 2, "result": "SUCCESS", "building": false,
                    "timestamp": 1_794_000_100_000_i64, "duration": 30_000,
                    "actions": [{ "_class": "hudson.model.ParametersAction",
                                  "parameters": [{ "name": "FAST", "value": true }] }] }),
        )
        .await;
    setup
        .get(
            &format!("{PR}/api/json"),
            json!({ "buildable": true,
                    "property": [{ "parameterDefinitions": [{ "name": "FAST" }] }] }),
        )
        .await;
    let main = mount_queued(&setup, 20, 8).await;
    mount_post(&setup, &format!("{MAIN}/build"), main, 1).await;
    let pr = mount_queued(&setup, 21, 3).await;
    Mock::given(method("POST"))
        .and(path(format!("{PR}/buildWithParameters")))
        .and(body_string_contains("FAST=true"))
        .respond_with(pr)
        .expect(1)
        .mount(&setup.jk)
        .await;
    refuse_the_rest(&setup.jk).await;
    let client = setup.start(Some("dev:jenkins-token-value")).await;

    let report = common::forge_context(&client, false).await;
    assert!(
        row(&report, "ci_rerun", &setup.name())["note"]
            .as_str()
            .is_some_and(|n| n.contains("Jenkins has no rebuild")),
        "{report}"
    );
    let rerun = common::call(&client, "ci_control", args(json!({ "action": "rerun" }))).await;
    let jk = setup.jk.uri();
    assert_eq!(
        rerun["done"],
        json!([
            format!("head: build 7 built again as build 8 of {jk}{MAIN}/"),
            format!("pr-main: build 2 built again as build 3 of {jk}{PR}/"),
        ]),
        "{rerun}"
    );
    assert!(
        rerun["notes"][0]
            .as_str()
            .is_some_and(|n| n.contains("may be a newer commit")),
        "{rerun}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_refused_jenkins_write_names_the_permission_and_read_only_mode_offers_none() {
    let setup = jenkins_repo().await;
    mount_builds(&setup).await;
    mount_post(
        &setup,
        &format!("{PR}/2/stop"),
        ResponseTemplate::new(403),
        1,
    )
    .await;
    refuse_the_rest(&setup.jk).await;
    let client = setup.start(Some("dev:jenkins-token-value")).await;
    let refused = common::try_call(&client, "ci_control", args(json!({ "action": "cancel" })))
        .await
        .unwrap_err();
    assert!(
        refused.contains("the Jenkins user lacks Job/Cancel on this job"),
        "{refused}"
    );
    client.cancel().await.unwrap();

    std::fs::remove_file(setup.dir.path().join(".probes.json")).ok();
    let client = setup
        .start_with(
            Some("dev:jenkins-token-value"),
            &[("GITFORGE_MCP_READ_ONLY", "true")],
        )
        .await;
    let report = common::forge_context(&client, false).await;
    assert_eq!(
        row(&report, "ci_cancel", &setup.name())["reason"],
        "the server is in read-only mode (--read-only)"
    );
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_control".to_owned())
    );
    client.cancel().await.unwrap();
}
