//! repo_settings through the real binary, against a fake Forgejo: plan,
//! apply and revert with their fingerprints, what each sends, and that a
//! webhook secret appears in no reply, not even when the forge echoes it.

mod common;

use common::forge::{TOKEN, fake_forgejo, one_commit, token_env};
use serde_json::{Map, Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "/api/v1/repos/acme/widgets";
const SECRET: &str = "hunter2-webhook-secret";

fn args(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

/// The repository as its admin sees it, with its settings.
async fn as_admin(forge: &MockServer, admin: bool) {
    Mock::given(method("GET"))
        .and(path(REPO))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "acme/widgets", "default_branch": "main", "private": true,
            "archived": false, "has_actions": true,
            "permissions": { "admin": admin, "push": true, "pull": true },
            "description": "old words", "website": "", "has_issues": true, "has_wiki": true,
            "allow_merge_commits": true, "allow_rebase": true, "allow_rebase_explicit": false,
            "allow_squash_merge": false, "allow_fast_forward_only_merge": false,
            "default_merge_style": "merge", "default_delete_branch_after_merge": false,
        })))
        .with_priority(1)
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/hooks")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 7, "type": "forgejo", "active": true, "branch_filter": "main",
            "events": ["push"], "config": {"url": "https://ci.example.com/hook", "content_type": "json"},
            "created_at": "2026-11-01T00:00:00Z", "updated_at": "2026-11-01T00:00:00Z",
        }])))
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/hooks/7")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 7, "type": "forgejo", "active": true, "branch_filter": "main",
            "events": ["push"], "config": {"url": "https://ci.example.com/hook", "content_type": "json"},
        })))
        .mount(forge)
        .await;
}

async fn start(forge: &MockServer, extra: &[(&str, &str)]) -> (common::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    one_commit(dir.path());
    common::forge::git(
        dir.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/acme/widgets.git", forge.uri()),
        ],
    );
    let var = token_env(forge);
    let mut env = vec![(var.as_str(), TOKEN)];
    env.extend_from_slice(extra);
    (common::start(dir.path(), &env).await, dir)
}

/// The JSON bodies of the requests `forge` received with `verb` to `to`.
async fn sent(forge: &MockServer, verb: &str, to: &str) -> Vec<Value> {
    forge
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r: &&Request| r.method.as_str() == verb && r.url.path() == to)
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

#[tokio::test]
async fn plan_apply_and_revert_check_their_fingerprints() {
    let forge = fake_forgejo("15.0.11").await;
    as_admin(&forge, true).await;
    Mock::given(method("PATCH"))
        .and(path(REPO))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&forge)
        .await;
    let (client, _dir) = start(&forge, &[]).await;

    let got = common::call(&client, "repo_settings", args(json!({"section": "merge"}))).await;
    assert_eq!(
        got["merge"]["allowed_styles"],
        json!(["merge", "rebase"]),
        "{got}"
    );

    let changes = json!({"description": "new words", "has_wiki": true});
    let plan = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "plan", "section": "general", "changes": changes})),
    )
    .await;
    assert_eq!(
        plan["changes"],
        json!([{"field": "description", "from": "old words", "to": "new words"}]),
        "{plan}"
    );
    let fingerprint = plan["fingerprint"].as_str().unwrap().to_owned();

    let stale = common::try_call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "general", "changes": changes, "fingerprint": "0123456789abcdef"})),
    )
    .await;
    assert!(stale.unwrap_err().contains("changed since the plan"));
    assert!(sent(&forge, "PATCH", REPO).await.is_empty());

    let applied = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "general", "changes": changes, "fingerprint": fingerprint})),
    )
    .await;
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(
        sent(&forge, "PATCH", REPO).await,
        vec![json!({"description": "new words"})]
    );
    let revert = applied["revert"].clone();
    assert_eq!(
        revert["ops"],
        json!([{"op": "set_repo", "fields": {"description": "old words"}}])
    );

    let mut tampered = revert.clone();
    tampered["fingerprint"] = json!("0123456789abcdef");
    let refused = common::try_call(
        &client,
        "repo_settings",
        args(json!({"action": "revert", "revert": tampered})),
    )
    .await;
    assert!(refused.unwrap_err().contains("changed after the apply"));

    let reverted = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "revert", "revert": revert})),
    )
    .await;
    assert_eq!(reverted["reverted"], true, "{reverted}");
    assert_eq!(
        sent(&forge, "PATCH", REPO).await,
        vec![
            json!({"description": "new words"}),
            json!({"description": "old words"})
        ]
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_webhook_secret_is_sent_but_never_shown() {
    let forge = fake_forgejo("15.0.11").await;
    as_admin(&forge, true).await;
    // The forge refuses the edit and quotes the secret back.
    Mock::given(method("PATCH"))
        .and(path(format!("{REPO}/hooks/7")))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(json!({"message": format!("secret {SECRET} is too weak")})),
        )
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{REPO}/hooks")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 9})))
        .mount(&forge)
        .await;
    let (client, _dir) = start(&forge, &[]).await;
    let mut replies = Vec::new();

    let update = json!({"update": [{"id": 7, "secret": SECRET}]});
    let plan = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "plan", "section": "webhooks", "changes": update})),
    )
    .await;
    replies.push(plan.to_string());
    let applied = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "webhooks", "changes": update, "fingerprint": plan["fingerprint"]})),
    )
    .await;
    assert_eq!(applied["applied"], false, "{applied}");
    assert!(
        applied["error"]
            .as_str()
            .unwrap()
            .contains("(secret) is too weak")
    );
    replies.push(applied.to_string());
    let edit = sent(&forge, "PATCH", &format!("{REPO}/hooks/7")).await;
    assert_eq!(edit[0]["config"]["secret"], SECRET);
    assert_eq!(
        edit[0]["branch_filter"], "main",
        "the edit keeps what it doesn't change"
    );

    let create = json!({"create": [{"url": "https://chat.example.com/in", "secret": SECRET, "events": ["release"]}]});
    let plan = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "plan", "section": "webhooks", "changes": create})),
    )
    .await;
    replies.push(plan.to_string());
    let created = common::call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "webhooks", "changes": create, "fingerprint": plan["fingerprint"]})),
    )
    .await;
    assert_eq!(
        created["revert"]["ops"],
        json!([{"op": "delete_webhook", "id": 9}])
    );
    replies.push(created.to_string());
    let body = &sent(&forge, "POST", &format!("{REPO}/hooks")).await[0];
    assert_eq!(body["type"], "forgejo");
    assert_eq!(body["config"]["secret"], SECRET);

    replies.push(
        common::call(&client, "repo_settings", args(json!({})))
            .await
            .to_string(),
    );
    for reply in &replies {
        assert!(
            !reply.contains(SECRET),
            "a reply showed the secret: {reply}"
        );
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn only_admins_get_settings_and_read_only_mode_keeps_just_the_reads() {
    let forge = fake_forgejo("15.0.11").await;
    as_admin(&forge, false).await;
    let (client, _dir) = start(&forge, &[]).await;
    assert!(
        !common::tool_names(&client)
            .await
            .contains(&"repo_settings".to_owned())
    );
    client.cancel().await.unwrap();

    let forge = fake_forgejo("15.0.11").await;
    as_admin(&forge, true).await;
    let (client, _dir) = start(&forge, &[("GITFORGE_MCP_READ_ONLY", "true")]).await;
    let tools = common::tools(&client).await;
    let tool = tools
        .iter()
        .find(|t| t.name == "repo_settings")
        .expect("offered");
    let schema = Value::Object((*tool.input_schema).clone());
    let actions: Vec<&str> = schema["$defs"]["SettingsAction"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["const"].as_str())
        .collect();
    assert_eq!(actions, ["get", "plan"], "{schema}");
    let refused = common::try_call(
        &client,
        "repo_settings",
        args(json!({"action": "apply", "section": "general", "changes": {}, "fingerprint": "x"})),
    )
    .await;
    assert!(refused.unwrap_err().contains("read-only mode"));
    client.cancel().await.unwrap();
}
