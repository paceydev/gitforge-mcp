//! CLI setup must never edit configuration when run without a terminal.
use std::process::{Command, Stdio};

fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gitforge-mcp"));
    command
        .current_dir(dir)
        .env("CODEX_HOME", dir.join("codex"))
        .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
        .env("GITFORGE_MCP_CONFIG", dir.join("no-config.toml"))
        .env_remove("GITFORGE_MCP_DIR")
        .env_remove("GITFORGE_MCP_REMOTE")
        .env_remove("GITFORGE_MCP_READ_ONLY")
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .stdin(Stdio::null());
    command
}

#[test]
fn noninteractive_init_refuses_to_write_without_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(dir.path())
        .args(["init", "--client", "both"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn dry_run_previews_both_clients_without_touching_disk_or_claiming_live_access() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(dir.path())
        .args(["init", "--dry-run"])
        .output()
        .unwrap();
    // No checkout here, so a fresh connection was not verified.
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No usable checkout/remote"), "{stderr}");
    assert!(stderr.contains("Claude Code:"), "{stderr}");
    assert!(stderr.contains("Codex:"), "{stderr}");
    assert!(stderr.contains("Preview only"), "{stderr}");
    assert!(!stderr.contains("access verified with a fresh request"));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

mod common;

#[tokio::test]
async fn live_dry_run_checks_credentials_without_forge_writes_or_cache_changes() {
    use common::forge::{TOKEN, fake_gitea, token_env};
    let dir = tempfile::tempdir().unwrap();
    let forge = fake_gitea("1.25.3").await;
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let cache = dir.path().join("probes.json");
    std::fs::write(&cache, "example cache sentinel").unwrap();
    let mut cmd = command(dir.path());
    cmd.env(token_env(&forge), TOKEN)
        .env("GITFORGE_MCP_CACHE", &cache)
        .args(["init", "--dry-run", "--client", "both", "--read-only"]);
    let output = tokio::process::Command::from(cmd).output().await.unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("Repository access verified with a fresh request"),
        "{text}"
    );
    assert!(text.contains("Signed in as: dev"), "{text}");
    assert!(text.contains("--read-only"), "{text}");
    assert!(!text.contains(TOKEN), "credential leaked in setup output");
    assert!(output.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(&cache).unwrap(),
        "example cache sentinel"
    );
    assert!(!dir.path().join("claude").exists());
    assert!(!dir.path().join("codex").exists());
    assert!(
        forge
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
}

#[tokio::test]
async fn invalid_credential_does_not_claim_a_successful_connection() {
    use common::forge::{fake_gitea, token_env};
    let dir = tempfile::tempdir().unwrap();
    let forge = fake_gitea("1.25.3").await;
    common::git_checkout(dir.path(), &format!("{}/acme/widgets.git", forge.uri()));
    let mut cmd = command(dir.path());
    cmd.env(token_env(&forge), "example-wrong-credential")
        .args(["init", "--dry-run", "--client", "none"]);
    let output = tokio::process::Command::from(cmd).output().await.unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        !text.contains("access verified with a fresh request"),
        "{text}"
    );
    assert!(
        !text.contains("example-wrong-credential"),
        "credential leaked in setup output"
    );
    assert!(!dir.path().join("claude").exists());
    assert!(!dir.path().join("codex").exists());
}
