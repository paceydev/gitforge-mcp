//! A fake forge and a git checkout for tests that run the real binary
//! against wiremock servers.

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const TOKEN: &str = "test-token-value";

pub async fn fake_forgejo(version: &str) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/forgejo/v1/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": version })))
        .mount(&server)
        .await;
    mount_user_and_repo(&server).await;
    server
}

pub async fn fake_gitea(version: &str) -> MockServer {
    let server = MockServer::builder().start().await;
    // Gitea has no Forgejo endpoint; wiremock answers 404 for it by default.
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": version })))
        .mount(&server)
        .await;
    mount_user_and_repo(&server).await;
    server
}

/// The user and repository endpoints answer only with the right token, so a
/// test that passes proves the credential reached the wire as
/// `Authorization: token <value>`.
pub async fn mount_user_and_repo(server: &MockServer) {
    let auth = format!("token {TOKEN}");
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .and(header("authorization", auth.as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "login": "dev", "is_admin": false })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets"))
        .and(header("authorization", auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "acme/widgets",
            "default_branch": "main",
            "private": true,
            "archived": false,
            "permissions": { "admin": false, "push": true, "pull": true },
            "has_actions": true
        })))
        .mount(server)
        .await;
}

pub fn token_env(server: &MockServer) -> String {
    format!("GITFORGE_TOKEN_127_0_0_1_{}", server.address().port())
}

/// Runs `git` in `dir` with a fixed identity; returns its trimmed stdout.
pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Dev", "-c", "user.email=dev@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A checkout of acme/widgets on `main` with one commit; returns its sha.
/// Its remote is a placeholder until the forge exists.
pub fn one_commit(dir: &std::path::Path) -> String {
    super::git_checkout(dir, "http://127.0.0.1:1/acme/widgets.git");
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Turns Actions off for acme/widgets on `forge`, as a repository that uses
/// CI outside the forge has it.
pub async fn without_actions(forge: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "acme/widgets",
            "default_branch": "main",
            "private": true,
            "archived": false,
            "permissions": { "admin": false, "push": true, "pull": true },
            "has_actions": false
        })))
        .with_priority(1)
        .mount(forge)
        .await;
}

/// `sha`'s commit statuses on `forge`.
pub async fn statuses(forge: &MockServer, sha: &str, list: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v1/repos/acme/widgets/commits/{sha}/statuses"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(list))
        .mount(forge)
        .await;
}
