//! The real binary against a fake Gitea whose repository runs its CI on a
//! fake Woodpecker: the statuses lead to Woodpecker's API, which gives the
//! pipeline's workflows and steps, step logs, cancel, restart, manual
//! pipelines and agents, each offered only with the access it needs.

mod common;

use common::forge::{TOKEN, fake_gitea, git, one_commit, statuses, token_env, without_actions};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WP_TOKEN: &str = "woodpecker-token-value";

/// Who the test signs in to Woodpecker as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Caller {
    Anonymous,
    Writer,
    Admin,
}

struct Setup {
    dir: TempDir,
    forge: MockServer,
    wp: MockServer,
}

impl Setup {
    fn wp_host(&self) -> String {
        format!("127.0.0.1:{}", self.wp.address().port())
    }

    async fn start(&self, caller: Caller, extra: &[(&str, &str)]) -> common::Client {
        let forge_var = token_env(&self.forge);
        let wp_var = token_env(&self.wp);
        let mut env = vec![(forge_var.as_str(), TOKEN)];
        if caller != Caller::Anonymous {
            env.push((wp_var.as_str(), WP_TOKEN));
        }
        env.extend_from_slice(extra);
        common::start(self.dir.path(), &env).await
    }

    /// `GET` `route` on Woodpecker answers `body`.
    async fn get(&self, route: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.wp)
            .await;
    }

    /// `POST` `route` on Woodpecker answers `body`, exactly once, and only
    /// with the token.
    async fn post_once(&self, route: &str, body: Option<Value>) {
        let response = match body {
            Some(b) => ResponseTemplate::new(200).set_body_json(b),
            None => ResponseTemplate::new(204),
        };
        Mock::given(method("POST"))
            .and(path(route))
            .and(header(
                "authorization",
                format!("Bearer {WP_TOKEN}").as_str(),
            ))
            .respond_with(response)
            .expect(1)
            .mount(&self.wp)
            .await;
    }
}

fn pipeline(number: u64, status: &str, test_state: &str) -> Value {
    json!({
        "id": 900 + number, "number": number, "event": "push", "status": status,
        "commit": "abc", "branch": "main", "ref": "refs/heads/main", "message": "one",
        "started": 1_794_000_000, "finished": if status == "running" { 0 } else { 1_794_000_090 },
        "workflows": [
            { "id": 7, "pid": 1, "name": "build", "state": "success",
              "started": 1_794_000_000, "finished": 1_794_000_030,
              "children": [{ "id": 70, "name": "compile", "state": "success" }] },
            { "id": 8, "pid": 2, "name": "test", "state": test_state,
              "started": 1_794_000_030,
              "children": [
                  { "id": 80, "name": "clone", "state": "success" },
                  { "id": 81, "name": "unit", "state": test_state, "started": 1_794_000_031 }
              ] }
        ]
    })
}

/// A Gitea repository with Actions off, whose HEAD (the head of `main`)
/// Woodpecker pipeline 42 reported on, and that Woodpecker, with what
/// `caller` may do there.
async fn woodpecker_repo(caller: Caller, pipeline_42: Value) -> Setup {
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
    let wp = MockServer::start().await;
    let status = |id: u64, workflow: &str, pid: u64| {
        json!({
            "id": id, "status": "pending", "context": format!("ci/woodpecker/push/{workflow}"),
            "target_url": format!("{}/repos/12/pipeline/42/{pid}", wp.uri()),
            "description": "Pipeline is pending", "created_at": "2026-11-09T10:00:00Z"
        })
    };
    statuses(
        &forge,
        &sha,
        json!([status(1, "build", 1), status(2, "test", 2)]),
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
    let setup = Setup { dir, forge, wp };
    setup
        .get(
            "/version",
            json!({ "source": "https://github.com/woodpecker-ci/woodpecker", "version": "3.20.1" }),
        )
        .await;
    setup
        .get("/api/repos/lookup/acme/widgets", json!({ "id": 12 }))
        .await;
    setup.get("/api/repos/12/pipelines/42", pipeline_42).await;
    if caller != Caller::Anonymous {
        let bearer = format!("Bearer {WP_TOKEN}");
        for (route, body) in [
            (
                "/api/user",
                json!({ "login": "dev", "admin": caller == Caller::Admin }),
            ),
            (
                "/api/repos/12/permissions",
                json!({ "pull": true, "push": true, "admin": false }),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .and(header("authorization", bearer.as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&setup.wp)
                .await;
        }
    }
    setup
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn row<'a>(report: &'a Value, feature: &str, provider: &str) -> &'a Value {
    report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == feature && c["provider"] == provider)
        .unwrap_or_else(|| panic!("no {feature} for {provider} in {report}"))
}

#[tokio::test]
async fn pipelines_steps_and_logs_come_from_woodpecker() {
    let setup = woodpecker_repo(Caller::Writer, pipeline(42, "failure", "failure")).await;
    setup
        .get(
            "/api/repos/12/logs/42/81",
            json!([
                { "time": 1, "data": "Z28gdGVzdA==", "type": 0 },
                { "time": 2, "data": "", "type": 2 },
                { "time": 3, "data": "RkFJTDogVGVzdFdpZGdldA==", "type": 1 }
            ]),
        )
        .await;
    let client = setup.start(Caller::Writer, &[]).await;
    let name = format!("woodpecker {}", setup.wp_host());

    // Actions is off, yet Woodpecker gives logs and controls; agents need
    // a Woodpecker admin.
    assert_eq!(
        common::tool_names(&client).await,
        [
            "ci_control",
            "ci_logs",
            "ci_status",
            "ci_wait",
            "forge_context",
            "pull_request"
        ]
    );
    let report = common::forge_context(&client, false).await;
    let system = &report["ci_systems"][0];
    assert_eq!(system["name"], name.as_str(), "{report}");
    assert_eq!(system["version"], "3.20.1");
    assert_eq!(system["found_by"], "commit status link");
    assert!(
        system["credential"]
            .as_str()
            .unwrap()
            .starts_with("env GITFORGE_TOKEN_127_0_0_1_")
    );
    assert_eq!(row(&report, "ci_cancel", &name)["available"], true);
    assert_eq!(
        row(&report, "ci_rerun_failed", &name)["reason"],
        "Woodpecker has no API for this"
    );

    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["verdict"], "failure", "{status}");
    let run = &status["runs"][0];
    assert_eq!(
        (run["number"].as_u64(), run["state"].as_str()),
        (Some(42), Some("failure"))
    );
    assert_eq!(run["external"], "ci/woodpecker/push");
    let test = &run["jobs"][0];
    assert_eq!(
        (test["name"].as_str(), test["source"].as_str()),
        (Some("test"), Some("api"))
    );
    assert_eq!(
        test["steps"][1],
        json!({ "name": "unit", "state": "failure" })
    );
    assert_eq!(
        test["url"],
        format!("{}/repos/12/pipeline/42/2", setup.wp.uri())
    );

    let logs = common::call(&client, "ci_logs", Default::default()).await;
    let job = &logs["jobs"][0];
    assert_eq!(job["name"], "test", "{logs}");
    assert_eq!(
        job["log"]["text"], "--- step unit (failure) ---\ngo test\nFAIL: TestWidget",
        "{logs}"
    );

    let refused = common::try_call(
        &client,
        "ci_control",
        args(json!({ "action": "rerun_failed" })),
    )
    .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("isn't available")),
        "{refused:?}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn woodpecker_cancels_restarts_dispatches_and_lists_agents() {
    let setup = woodpecker_repo(Caller::Admin, pipeline(42, "running", "running")).await;
    setup
        .post_once("/api/repos/12/pipelines/42/cancel", None)
        .await;
    // run=41 is Woodpecker's pipeline 41: the forge runs no CI of its own.
    setup
        .get(
            "/api/repos/12/pipelines/41",
            pipeline(41, "failure", "failure"),
        )
        .await;
    setup
        .post_once(
            "/api/repos/12/pipelines/41",
            Some(pipeline(43, "pending", "pending")),
        )
        .await;
    setup
        .get(
            "/api/repos/12/pipelines/43",
            pipeline(43, "running", "running"),
        )
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos/12/pipelines"))
        .and(body_partial_json(
            json!({ "branch": "main", "variables": { "DEPLOY": "yes", "N": "2" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(pipeline(44, "pending", "pending")))
        .expect(1)
        .mount(&setup.wp)
        .await;
    setup
        .get(
            "/api/agents",
            json!([{ "id": 1, "name": "runner-1", "platform": "linux/amd64", "backend": "local",
                     "last_contact": chrono_now(), "org_id": -1, "token": "never shown" }]),
        )
        .await;
    setup
        .get(
            "/api/queue/info",
            json!({ "pending": [{ "name": "test", "repo_id": 12, "labels": { "platform": "linux/arm64" } }],
                    "running": [], "waiting_on_deps": [], "stats": {}, "paused": false }),
        )
        .await;
    let client = setup.start(Caller::Admin, &[]).await;

    let cancelled = common::call(&client, "ci_control", args(json!({ "action": "cancel" }))).await;
    assert_eq!(cancelled["run"], 42, "{cancelled}");

    let rerun = common::call(
        &client,
        "ci_control",
        args(json!({ "action": "rerun", "run": 41 })),
    )
    .await;
    assert_eq!(
        rerun["done"],
        json!(["pipeline 41 restarted as pipeline 43"]),
        "{rerun}"
    );
    assert_eq!(rerun["run_now"]["state"], "running");

    let dispatched = common::call(
        &client,
        "ci_control",
        args(json!({ "action": "dispatch", "inputs": { "DEPLOY": "yes", "N": 2 } })),
    )
    .await;
    assert_eq!(dispatched["started"]["run_number"], 44, "{dispatched}");
    assert_eq!(dispatched["ci"], format!("woodpecker {}", setup.wp_host()));

    let runners = common::call(&client, "ci_runners", Default::default()).await;
    let agents = &runners["ci_systems"][0];
    assert_eq!(agents["runners"][0]["state"], "idle", "{runners}");
    assert_eq!(agents["waiting"][0]["matching_idle"], 0);
    assert!(!runners.to_string().contains("never shown"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn without_a_woodpecker_token_its_pipelines_are_read_only() {
    let setup = woodpecker_repo(Caller::Anonymous, pipeline(42, "success", "success")).await;
    let client = setup.start(Caller::Anonymous, &[]).await;
    let name = format!("woodpecker {}", setup.wp_host());

    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_control".to_owned())
    );
    let report = common::forge_context(&client, false).await;
    assert!(
        row(&report, "ci_cancel", &name)["reason"]
            .as_str()
            .unwrap()
            .contains("sign in with a Woodpecker token"),
        "{report}"
    );
    let status = common::call(&client, "ci_status", Default::default()).await;
    assert_eq!(status["verdict"], "success", "{status}");
    assert_eq!(status["runs"][0]["number"], 42);
    client.cancel().await.unwrap();

    // A token in read-only mode reads the same and changes nothing.
    let setup = woodpecker_repo(Caller::Writer, pipeline(42, "running", "running")).await;
    let client = setup
        .start(Caller::Writer, &[("GITFORGE_MCP_READ_ONLY", "true")])
        .await;
    let report = common::forge_context(&client, false).await;
    assert_eq!(
        row(
            &report,
            "ci_cancel",
            &format!("woodpecker {}", setup.wp_host())
        )["reason"],
        "the server is in read-only mode (--read-only)"
    );
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"ci_control".to_owned())
    );
    client.cancel().await.unwrap();
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
