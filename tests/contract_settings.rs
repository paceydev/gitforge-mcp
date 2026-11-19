//! Contract tests for repo_settings, against the same real forges as
//! tests/contract.rs: for each section, a change is planned, applied, seen
//! on the forge, and reverted, and the forge's own record of the section
//! must then be exactly what it was. A webhook secret goes in and must come
//! out of no reply.
//!
//! The repository is one of its own, set up with rules and a webhook made
//! through the forge's API with more fields than repo_settings names, so a
//! revert that recreated them from the shared names alone would fail.

mod common;

use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    kind: String,
    base_url: String,
    admin_token: String,
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

const REPO: &str = "settings-test";
const SECRET: &str = "contract-secret-6f1d2a";

/// The repository's settings as the forge has them, field by field.
const REPO_FIELDS: &[&str] = &[
    "description",
    "website",
    "default_branch",
    "private",
    "has_issues",
    "has_wiki",
    "has_projects",
    "has_pull_requests",
    "allow_merge_commits",
    "allow_rebase",
    "allow_rebase_explicit",
    "allow_squash_merge",
    "allow_fast_forward_only_merge",
    "allow_rebase_update",
    "default_merge_style",
    "default_delete_branch_after_merge",
    "default_allow_maintainer_edit",
    "ignore_whitespace_conflicts",
];

fn args(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

async fn api(
    target: &Target,
    verb: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(verb, format!("{}/api/v1{path}", target.base_url))
        .header("Authorization", format!("token {}", target.admin_token))
        .header("User-Agent", "gitforge-mcp-contract");
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.expect("forge API");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn must(target: &Target, verb: reqwest::Method, path: &str, body: Value) -> Value {
    let (status, reply) = api(target, verb, path, Some(body)).await;
    assert!((200..300).contains(&status), "{path}: {status} {reply}");
    reply
}

fn repo_path(rest: &str) -> String {
    format!("/repos/gfadmin/{REPO}{rest}")
}

/// A fresh repository with one commit, a simple rule, a rich one, and a
/// webhook with a secret and a branch filter.
async fn set_up(target: &Target) -> u64 {
    api(target, reqwest::Method::DELETE, &repo_path(""), None).await;
    must(
        target,
        reqwest::Method::POST,
        "/user/repos",
        json!({"name": REPO, "auto_init": true, "default_branch": "main"}),
    )
    .await;
    must(
        target,
        reqwest::Method::POST,
        &repo_path("/branch_protections"),
        json!({"rule_name": "main", "branch_name": "main", "required_approvals": 1}),
    )
    .await;
    must(
        target,
        reqwest::Method::POST,
        &repo_path("/branch_protections"),
        json!({
            "rule_name": "release/*", "enable_push": true, "enable_push_whitelist": true,
            "push_whitelist_usernames": ["gfadmin"], "required_approvals": 1,
            "enable_status_check": true, "status_check_contexts": ["ci / test"],
            "block_on_outdated_branch": true, "protected_file_patterns": "*.lock",
            "dismiss_stale_approvals": true,
        }),
    )
    .await;
    let hook = must(
        target,
        reqwest::Method::POST,
        &repo_path("/hooks"),
        json!({
            "type": target.kind, "active": true, "branch_filter": "main",
            "events": ["push", "pull_request"],
            "config": {"url": "http://127.0.0.1:9/baseline", "content_type": "json", "secret": "baseline"},
        }),
    )
    .await;
    hook["id"].as_u64().expect("hook id")
}

/// A record in the form that means the same whatever the forge's whims:
/// events in order (Gitea lists them in no fixed one), and a list that was
/// never set (`null`) the same as an empty one, which is how an edit has
/// to put it back.
fn settled(mut v: Value) -> Value {
    if let Some(map) = v.as_object_mut() {
        for (k, item) in map.iter_mut() {
            let list = ["_contexts", "_usernames", "_username", "_teams"]
                .iter()
                .any(|end| k.ends_with(end));
            if list && item.is_null() {
                *item = json!([]);
            }
            if k == "events"
                && let Some(events) = item.as_array_mut()
            {
                events.sort_by_key(Value::to_string);
            }
        }
    }
    v
}

fn drop_keys(mut v: Value, keys: &[&str]) -> Value {
    if let Some(map) = v.as_object_mut() {
        for k in keys {
            map.remove(*k);
        }
    }
    v
}

/// Each section as the forge records it, less what it stamps itself: times,
/// and a webhook's id, which a recreated one can't keep.
async fn snapshot(target: &Target) -> Value {
    let (_, repo) = api(target, reqwest::Method::GET, &repo_path(""), None).await;
    let repo: Map<String, Value> = REPO_FIELDS
        .iter()
        .filter_map(|k| repo.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect();
    let (_, rules) = api(
        target,
        reqwest::Method::GET,
        &repo_path("/branch_protections"),
        None,
    )
    .await;
    let mut rules: Vec<Value> = rules
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|r| settled(drop_keys(r, &["created_at", "updated_at"])))
        .collect();
    rules.sort_by_key(|r| r["rule_name"].to_string());
    let (_, hooks) = api(target, reqwest::Method::GET, &repo_path("/hooks"), None).await;
    let mut hooks: Vec<Value> = hooks
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|h| settled(drop_keys(h, &["id", "created_at", "updated_at"])))
        .collect();
    hooks.sort_by_key(|h| h["config"]["url"].to_string());
    json!({ "general_and_merge": repo, "branch_protection": rules, "webhooks": hooks })
}

/// The part of a snapshot a section changes.
fn part(snapshot: &Value, section: &str) -> Value {
    match section {
        "general" | "merge" => snapshot["general_and_merge"].clone(),
        other => snapshot[other].clone(),
    }
}

#[tokio::test]
async fn apply_then_revert_restores_every_section_exactly() {
    let Some(targets) = targets() else { return };
    let mut failures = Vec::new();
    for target in &targets {
        eprintln!("== {} (settings)", target.name);
        let mut check = |ok: bool, what: String| {
            if !ok {
                failures.push(format!("{}: {what}", target.name));
            }
        };
        let hook = set_up(target).await;
        let dir = tempfile::tempdir().unwrap();
        common::git_checkout(
            dir.path(),
            &format!("{}/gfadmin/{REPO}.git", target.base_url),
        );
        let host = target.base_url.trim_start_matches("http://");
        let var = gitforge_mcp::auth::default_env_var(host);
        let client =
            common::start(dir.path(), &[(var.as_str(), target.admin_token.as_str())]).await;
        let mut replies = Vec::new();

        let cases = [
            (
                "general",
                json!({"description": "changed by the contract test", "website": "https://example.com", "has_wiki": false}),
            ),
            (
                "merge",
                json!({"allowed_styles": ["squash", "rebase-merge"], "default_style": "squash", "delete_branch_after_merge": true}),
            ),
            (
                "branch_protection",
                json!({
                    "set": [
                        {"pattern": "main", "required_approvals": 2, "required_checks": ["ci / build"], "require_up_to_date": true},
                        {"pattern": "hotfix/*", "require_pull_request": true, "require_signed_commits": true},
                    ],
                    "delete": ["release/*"],
                }),
            ),
            (
                "webhooks",
                json!({
                    "update": [{"id": hook, "active": false, "events": ["push", "release"], "branch_filter": "release/*"}],
                    "create": [{"url": "http://127.0.0.1:9/new", "secret": SECRET, "events": ["release"]}],
                }),
            ),
            ("webhooks", json!({"delete": [hook]})),
        ];
        for (section, changes) in cases {
            let before = snapshot(target).await;
            let plan = common::try_call(
                &client,
                "repo_settings",
                args(json!({"action": "plan", "section": section, "changes": changes})),
            )
            .await;
            let plan = match plan {
                Ok(p) => p,
                Err(e) => {
                    check(false, format!("{section} plan: {e}"));
                    continue;
                }
            };
            replies.push(plan.to_string());
            let applied = common::try_call(
                &client,
                "repo_settings",
                args(json!({"action": "apply", "section": section, "changes": changes, "fingerprint": plan["fingerprint"]})),
            )
            .await;
            let applied = match applied {
                Ok(a) if a["applied"] == true => a,
                other => {
                    check(false, format!("{section} apply: {other:?}"));
                    continue;
                }
            };
            replies.push(applied.to_string());
            let during = snapshot(target).await;
            check(
                part(&during, section) != part(&before, section),
                format!("{section}: apply changed nothing on the forge"),
            );
            // Applying again changes nothing: the plan is empty.
            let again = common::try_call(
                &client,
                "repo_settings",
                args(json!({"action": "plan", "section": section, "changes": changes})),
            )
            .await;
            if section != "webhooks" {
                check(
                    again.as_ref().is_ok_and(|p| p["changes"] == json!([])),
                    format!("{section}: planning the same change again: {again:?}"),
                );
            }
            let reverted = common::try_call(
                &client,
                "repo_settings",
                args(json!({"action": "revert", "revert": applied["revert"]})),
            )
            .await;
            check(
                reverted.as_ref().is_ok_and(|r| r["reverted"] == true),
                format!("{section} revert: {reverted:?}"),
            );
            replies.push(format!("{reverted:?}"));
            let after = snapshot(target).await;
            check(
                part(&after, section) == part(&before, section),
                format!(
                    "{section}: revert didn't restore it exactly\n  before: {}\n  after:  {}",
                    part(&before, section),
                    part(&after, section)
                ),
            );
        }

        let all = common::try_call(&client, "repo_settings", args(json!({}))).await;
        replies.push(format!("{all:?}"));
        for reply in &replies {
            check(
                !reply.contains(SECRET),
                format!("a reply showed the secret: {reply}"),
            );
        }
        client.cancel().await.unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
