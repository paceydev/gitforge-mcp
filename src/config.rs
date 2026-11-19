//! The optional config file: per-host overrides for detection and credentials.
//!
//! ```toml
//! [hosts."git.example.com"]
//! kind = "gitea"                    # skip detection: gitea, forgejo or github
//! base_url = "https://git.example.com"
//! token_env = "EXAMPLE_FORGE_TOKEN" # read the token from this variable
//! credentials = ["env", "keyring", "git-credential"]  # sources, in order
//!                                   # ("gh" too, for GitHub hosts)
//!
//! [ci."ci.example.com"]             # CI outside the forge (Woodpecker, Jenkins)
//! kind = "woodpecker"               # optional: found from commit statuses
//! base_url = "https://ci.example.com"
//! token_env = "EXAMPLE_CI_TOKEN"    # default GITFORGE_TOKEN_CI_EXAMPLE_COM
//! username = "dev"                  # Jenkins: the API token's user
//! credentials = ["env", "keyring"]  # the default for a CI host
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::version::{CiKind, ForgeKind};

/// Points the server at a config file other than the default.
pub const CONFIG_ENV: &str = "GITFORGE_MCP_CONFIG";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostConfig>,
    /// CI systems outside the forge, by host.
    #[serde(default)]
    pub ci: BTreeMap<String, CiHostConfig>,
}

/// A CI system outside the forge. Statuses on the repository's commits
/// usually say where it is; this names one they don't, or how to sign in.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiHostConfig {
    /// Which CI system runs here. Without it, the host is asked.
    pub kind: Option<CiKind>,
    /// The CI system's web root, when it is served under a path or on
    /// another port than its links show.
    pub base_url: Option<Url>,
    /// Read the token from this environment variable instead of the default
    /// `GITFORGE_TOKEN_<HOST>`.
    pub token_env: Option<String>,
    /// Jenkins only: the user the API token belongs to. Without it, the
    /// token must be stored as `user:token`.
    pub username: Option<String>,
    /// Which credential sources to try, in order. Default: env, keyring.
    pub credentials: Option<Vec<CredentialSource>>,
}

impl CiHostConfig {
    /// The same lookup a forge host has, minus the git credential helper by
    /// default: it holds forge passwords, not CI tokens.
    pub fn credential_lookup(&self) -> HostConfig {
        HostConfig {
            kind: None,
            base_url: None,
            token_env: self.token_env.clone(),
            credentials: Some(
                self.credentials
                    .clone()
                    .unwrap_or_else(|| vec![CredentialSource::Env, CredentialSource::Keyring]),
            ),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Skip detection and treat the host as this kind of forge. Required for
    /// GitHub Enterprise Server, which cannot be told apart by probing.
    pub kind: Option<ForgeKind>,
    /// The forge's web root, when the git remote doesn't give it (SSH
    /// remotes, or a web server on another port).
    pub base_url: Option<Url>,
    /// Read the token from this environment variable instead of the default
    /// `GITFORGE_TOKEN_<HOST>`.
    pub token_env: Option<String>,
    /// Which credential sources to try, in order.
    pub credentials: Option<Vec<CredentialSource>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    Env,
    Keyring,
    /// `gh auth token`: the GitHub CLI's sign-in. GitHub hosts only.
    Gh,
    GitCredential,
}

impl CredentialSource {
    /// The name the config file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialSource::Env => "env",
            CredentialSource::Keyring => "keyring",
            CredentialSource::Gh => "gh",
            CredentialSource::GitCredential => "git-credential",
        }
    }

    pub const DEFAULT_ORDER: [CredentialSource; 3] = [
        CredentialSource::Env,
        CredentialSource::Keyring,
        CredentialSource::GitCredential,
    ];
    /// GitHub hosts also try the GitHub CLI, before git's helper: a token
    /// from `gh` has the API scopes it was signed in with, while the helper
    /// may hold one meant only for git.
    pub const GITHUB_ORDER: [CredentialSource; 4] = [
        CredentialSource::Env,
        CredentialSource::Keyring,
        CredentialSource::Gh,
        CredentialSource::GitCredential,
    ];
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not valid: {message}")]
    Parse { path: String, message: String },
}

impl Config {
    /// `$GITFORGE_MCP_CONFIG`, else `config.toml` in the platform config
    /// directory: `$XDG_CONFIG_HOME/gitforge-mcp` or `~/.config/gitforge-mcp`
    /// on macOS and Linux, `%APPDATA%\gitforge-mcp` on Windows.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(CONFIG_ENV) {
            return Some(PathBuf::from(path));
        }
        config_dir().map(|d| d.join("gitforge-mcp").join("config.toml"))
    }

    /// Loads `path`. A missing file is an empty config, not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Self::parse(&text).map_err(|message| ConfigError::Parse {
            path: path.display().to_string(),
            message,
        })
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn host(&self, host: &str) -> HostConfig {
        self.hosts.get(host).cloned().unwrap_or_default()
    }
}

fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return dirs::config_dir();
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_sections() {
        let config = Config::parse(
            r#"
            [hosts."git.example.com"]
            kind = "forgejo"
            token_env = "MY_TOKEN"
            credentials = ["env", "gh", "git-credential"]

            [ci."ci.example.com"]
            kind = "woodpecker"

            [ci."jenkins.example.com"]
            kind = "jenkins"
            username = "dev"
            "#,
        )
        .unwrap();
        let host = config.host("git.example.com");
        assert_eq!(host.kind, Some(ForgeKind::Forgejo));
        assert_eq!(host.token_env.as_deref(), Some("MY_TOKEN"));
        assert_eq!(
            host.credentials,
            Some(vec![
                CredentialSource::Env,
                CredentialSource::Gh,
                CredentialSource::GitCredential
            ])
        );
        assert!(config.host("other.example.com").kind.is_none());
        let ci = &config.ci["ci.example.com"];
        assert_eq!(ci.kind, Some(CiKind::Woodpecker));
        let jenkins = &config.ci["jenkins.example.com"];
        assert_eq!(jenkins.kind, Some(CiKind::Jenkins));
        assert_eq!(jenkins.username.as_deref(), Some("dev"));
        assert_eq!(
            ci.credential_lookup().credentials,
            Some(vec![CredentialSource::Env, CredentialSource::Keyring])
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Config::parse("[hosts.\"h\"]\ntoken = \"oops\"\n").unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("absent.toml")).unwrap();
        assert!(config.hosts.is_empty());
    }
}
