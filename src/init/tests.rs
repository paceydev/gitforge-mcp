use std::path::Path;

use super::{Client, clients, files};

fn args() -> Vec<String> {
    vec!["--read-only".to_owned()]
}

#[test]
fn claude_keeps_other_servers_account_state_and_server_environment_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.json");
    let original = serde_json::json!({
        "account": {"sentinel": "example-private-value"},
        "mcpServers": {
            "other": {"command": "other-server"},
            "gitforge": {"command": "old-server", "env": {"SENTINEL": "example-private-value"}}
        }
    });
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let plan = clients::plan(
        Client::Claude,
        &path,
        Path::new("/example/bin/gitforge-mcp"),
        &args(),
    )
    .unwrap();
    assert!(!plan.snippet.contains("example-private-value"));
    let updated: serde_json::Value = serde_json::from_slice(&plan.after).unwrap();
    assert_eq!(updated["account"], original["account"]);
    assert_eq!(
        updated["mcpServers"]["other"],
        original["mcpServers"]["other"]
    );
    assert_eq!(
        updated["mcpServers"]["gitforge"]["env"],
        original["mcpServers"]["gitforge"]["env"]
    );
    assert_eq!(
        updated["mcpServers"]["gitforge"]["args"],
        serde_json::json!(["--read-only"])
    );
    assert_eq!(std::fs::read(&path).unwrap(), plan.before.clone().unwrap());
    files::apply(&plan).unwrap();
    assert!(
        !clients::plan(
            Client::Claude,
            &path,
            Path::new("/example/bin/gitforge-mcp"),
            &args()
        )
        .unwrap()
        .changed()
    );
}

#[test]
fn codex_preserves_comments_unrelated_configuration_and_disabled_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# my configuration\nmodel = 'example-model' # keep this\n\n[mcp_servers.other]\ncommand = 'other'\n\n[mcp_servers.gitforge]\ncommand = 'old'\nenabled = false # keep deliberate disable\n\n[mcp_servers.gitforge.env]\nSENTINEL = 'example-private-value'\n";
    std::fs::write(&path, original).unwrap();
    let plan = clients::plan(
        Client::Codex,
        &path,
        Path::new("C:\\Example Folder\\gitforge-mcp.exe"),
        &args(),
    )
    .unwrap();
    let text = std::str::from_utf8(&plan.after).unwrap();
    assert!(text.contains("model = 'example-model' # keep this"));
    assert!(text.contains("enabled = false # keep deliberate disable"));
    assert!(text.contains("SENTINEL = 'example-private-value'"));
    assert!(!plan.snippet.contains("example-private-value"));
    let parsed: toml::Value = toml::from_str(text).unwrap();
    assert_eq!(
        parsed["mcp_servers"]["gitforge"]["command"].as_str(),
        Some("C:\\Example Folder\\gitforge-mcp.exe")
    );
    files::apply(&plan).unwrap();
    assert!(
        !clients::plan(
            Client::Codex,
            &path,
            Path::new("C:\\Example Folder\\gitforge-mcp.exe"),
            &args()
        )
        .unwrap()
        .changed()
    );
}

#[test]
fn preview_does_not_create_files_or_directories_and_confirmed_write_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    for (client, name) in [
        (Client::Claude, "claude.json"),
        (Client::Codex, "config.toml"),
    ] {
        let path = dir.path().join(name).join("new").join(name);
        let plan =
            clients::plan(client, &path, Path::new("/example/bin/gitforge-mcp"), &[]).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert!(files::apply(&plan).unwrap().is_none());
        assert!(
            !clients::plan(client, &path, Path::new("/example/bin/gitforge-mcp"), &[])
                .unwrap()
                .changed()
        );
    }
}

#[test]
fn malformed_configuration_and_other_transports_are_not_overwritten_or_echoed() {
    let dir = tempfile::tempdir().unwrap();
    for (client, input) in [
        (Client::Claude, "{\"example-private-value\": invalid}"),
        (Client::Codex, "example-private-value = ["),
        (
            Client::Claude,
            "{\"mcpServers\":{\"gitforge\":{\"url\":\"https://example.com/mcp\"}}}",
        ),
        (
            Client::Codex,
            "[mcp_servers.gitforge]\nurl = 'https://example.com/mcp'\n",
        ),
        (Client::Codex, "mcp_servers = 5\n"),
    ] {
        let path = dir.path().join("client-config");
        std::fs::write(&path, input).unwrap();
        let error = clients::plan(client, &path, Path::new("/example/bin/gitforge-mcp"), &[])
            .err()
            .unwrap();
        assert!(!error.to_string().contains("example-private-value"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), input);
    }
}

#[test]
fn refused_stale_plan_never_overwrites_newer_client_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# original\n").unwrap();
    let plan = clients::plan(
        Client::Codex,
        &path,
        Path::new("/example/bin/gitforge-mcp"),
        &[],
    )
    .unwrap();
    std::fs::write(&path, "# edited while waiting for approval\n").unwrap();
    assert!(files::apply(&plan).is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "# edited while waiting for approval\n"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn backup_contains_original_bytes_and_has_private_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# keep this backup\nmodel = 'example-model'\n";
    std::fs::write(&path, original).unwrap();
    let plan = clients::plan(
        Client::Codex,
        &path,
        Path::new("/example/bin/gitforge-mcp"),
        &[],
    )
    .unwrap();
    let backup = files::apply(&plan).unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn invalid_existing_args_are_replaced_in_the_preview() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[mcp_servers.gitforge]\ncommand = '/example/bin/gitforge-mcp'\nargs = [1]\n",
    )
    .unwrap();
    let plan = clients::plan(
        Client::Codex,
        &path,
        Path::new("/example/bin/gitforge-mcp"),
        &[],
    )
    .unwrap();
    assert!(plan.changed());
    let text = std::str::from_utf8(&plan.after).unwrap();
    assert!(text.contains("args = []"));
}

#[cfg(unix)]
#[test]
fn symlinked_configuration_and_homebrew_command_paths_are_preserved() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("actual.toml");
    std::fs::write(&target, "# managed dotfile\n").unwrap();
    let link = dir.path().join("config.toml");
    symlink(&target, &link).unwrap();
    let plan = clients::plan(
        Client::Codex,
        &link,
        Path::new("/example/bin/gitforge-mcp"),
        &[],
    )
    .unwrap();
    files::apply(&plan).unwrap();
    assert!(link.is_symlink());
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("mcp_servers.gitforge")
    );
    let cellar = dir.path().join("versioned-binary");
    std::fs::write(&cellar, "example executable").unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let stable = bin.join("gitforge-mcp");
    symlink(&cellar, &stable).unwrap();
    assert_eq!(
        super::executable::resolve(Path::new("gitforge-mcp"), &cellar, &[bin]).unwrap(),
        stable
    );
    assert_eq!(
        super::executable::resolve(&stable, &cellar, &[]).unwrap(),
        stable
    );
}
