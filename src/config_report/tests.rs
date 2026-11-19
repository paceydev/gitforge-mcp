use std::path::Path;

use super::*;

fn process(dir: &Path, config: &Path) -> Process {
    Process {
        dir: (dir.to_owned(), Given::CommandLine),
        remote: (None, Given::Default),
        config: (Some(config.to_owned()), Given::Environment),
        read_only: (true, Given::Environment),
    }
}

fn checkout(dir: &Path, url: &str) {
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", url]] {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
}

#[tokio::test]
async fn names_where_each_setting_came_from() {
    let dir = tempfile::tempdir().unwrap();
    checkout(dir.path(), "https://git.example.com/acme/widgets.git");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
        [hosts."git.example.com"]
        kind = "forgejo"
        credentials = ["keyring", "env"]

        [hosts."other.example.com"]
        token_env = "OTHER_TOKEN"

        [ci."ci.example.com"]
        kind = "jenkins"
        username = "dev"
        "#,
    )
    .unwrap();

    let report = gather(&process(dir.path(), &config)).await;
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.config_file.state, FileState::Loaded);
    assert_eq!(report.config_file.from, CONFIG_ENV);
    assert_eq!(report.dir.from, "--dir");
    assert_eq!(report.remote.value.as_deref(), Some("origin"));
    assert_eq!(report.remote.from, "default");
    assert_eq!(report.read_only.from, READ_ONLY_ENV);
    let repo = report.repository.as_ref().unwrap();
    assert_eq!(repo.full_name, "acme/widgets");

    let forge = &report.hosts[0];
    assert!(forge.bound);
    assert_eq!(forge.kind.value.as_deref(), Some("forgejo"));
    assert_eq!(forge.kind.from, "config file");
    assert_eq!(forge.base_url.from, "git remote");
    assert_eq!(forge.credentials.value, ["keyring", "env"]);
    assert_eq!(forge.credentials.from, "config file");
    assert_eq!(
        forge.token_vars.value[0].name,
        "GITFORGE_TOKEN_GIT_EXAMPLE_COM"
    );
    assert_eq!(forge.token_vars.from, "default");

    // A host only the file names comes after the repository's.
    let other = &report.hosts[1];
    assert_eq!(other.host, "other.example.com");
    assert!(!other.bound);
    assert_eq!(other.token_vars.value[0].name, "OTHER_TOKEN");
    assert_eq!(other.token_vars.from, "config file");
    assert_eq!(report.hosts.len(), 2);

    let ci = &report.ci[0];
    assert_eq!(ci.kind.value.as_deref(), Some("jenkins"));
    assert_eq!(ci.credentials.value, ["env", "keyring"]);
    assert_eq!(ci.credentials.from, "default");

    let text = render(&report);
    assert!(text.contains("acme/widgets on git.example.com"), "{text}");
    assert!(
        text.contains("Forge git.example.com (the repository's)"),
        "{text}"
    );
    assert!(
        text.contains("kind         forgejo  (config file)"),
        "{text}"
    );
    assert!(text.contains("username     dev  (config file)"), "{text}");
}

#[tokio::test]
async fn reports_an_invalid_file_and_a_missing_repository() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "[hosts.\"h\"]\ntoken = \"oops\"\n").unwrap();
    let report = gather(&process(dir.path(), &config)).await;
    assert_eq!(report.config_file.state, FileState::Invalid);
    assert!(report.repository.is_none());
    assert_eq!(report.problems.len(), 2, "{:?}", report.problems);
    assert!(report.problems[0].contains("unknown field"));
    assert!(render(&report).contains("Problems"));
}

#[tokio::test]
async fn a_missing_file_is_nothing_configured() {
    let dir = tempfile::tempdir().unwrap();
    checkout(dir.path(), "https://github.com/acme/widgets.git");
    let report = gather(&process(dir.path(), &dir.path().join("absent.toml"))).await;
    assert_eq!(report.config_file.state, FileState::Missing);
    assert!(report.problems.is_empty());
    let forge = &report.hosts[0];
    assert!(forge.kind.value.is_none());
    let vars: Vec<&str> = forge
        .token_vars
        .value
        .iter()
        .map(|v| v.name.as_str())
        .collect();
    assert_eq!(
        vars,
        ["GITFORGE_TOKEN_GITHUB_COM", "GH_TOKEN", "GITHUB_TOKEN"]
    );
    assert_eq!(
        forge.credentials.value,
        ["env", "keyring", "gh", "git-credential"]
    );
    assert!(render(&report).contains("not there: nothing configured"));
}
