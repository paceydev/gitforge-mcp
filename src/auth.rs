//! Finding a credential for a host without ever storing or printing one.
//!
//! Sources, in the default order:
//!
//! 1. an environment variable: `GITFORGE_TOKEN_<HOST>` (or the host's
//!    `token_env`), plus `GH_TOKEN` and `GITHUB_TOKEN` for github.com, and
//!    `GH_ENTERPRISE_TOKEN` and `GITHUB_ENTERPRISE_TOKEN` for GitHub
//!    Enterprise Server, as the GitHub CLI reads them;
//! 2. the OS keyring (macOS Keychain, Windows Credential Manager, Secret
//!    Service on Linux), under service `gitforge-mcp` and the host's name,
//!    as `gitforge-mcp auth set <host>` stores it;
//! 3. for GitHub hosts, `gh auth token`: the GitHub CLI's sign-in;
//! 4. `git credential fill`: whatever helper already lets you push to the
//!    host, so a machine that can clone needs no new token at all.

use std::process::Stdio;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{CredentialSource, HostConfig};
use crate::secret::Secret;
use crate::version::ForgeKind;

/// The keyring service name every entry is stored under.
pub const KEYRING_SERVICE: &str = "gitforge-mcp";

#[derive(Debug, Clone)]
pub enum Credential {
    /// An access token. Gitea and Forgejo take it as `token <value>`,
    /// GitHub as `Bearer <value>`.
    Token(Secret),
    /// A username and password (or token as password), from a git
    /// credential helper. Sent as HTTP Basic auth.
    Basic { username: String, password: Secret },
}

/// A credential and where it came from. Only `source` is ever reported.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub credential: Credential,
    pub source: String,
}

/// What each source said, for `forge_context` and `auth status`, so a missing
/// credential can be explained without guessing.
#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub source: String,
    pub outcome: String,
}

/// The environment variable checked for `host`'s token:
/// `git.example.com:8443` → `GITFORGE_TOKEN_GIT_EXAMPLE_COM_8443`.
pub fn default_env_var(host: &str) -> String {
    let suffix: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GITFORGE_TOKEN_{suffix}")
}

/// Whether `host` is GitHub: github.com, or a host the config says is
/// GitHub Enterprise Server.
fn is_github(host: &str, config: &HostConfig) -> bool {
    host.eq_ignore_ascii_case("github.com") || config.kind == Some(ForgeKind::Github)
}

/// The environment variables checked for `host`'s token, in order.
pub fn env_vars(host: &str, config: &HostConfig) -> Vec<String> {
    let mut vars = vec![
        config
            .token_env
            .clone()
            .unwrap_or_else(|| default_env_var(host)),
    ];
    if host.eq_ignore_ascii_case("github.com") {
        vars.push("GH_TOKEN".into());
        vars.push("GITHUB_TOKEN".into());
    } else if is_github(host, config) {
        vars.push("GH_ENTERPRISE_TOKEN".into());
        vars.push("GITHUB_ENTERPRISE_TOKEN".into());
    }
    vars
}

/// The credential sources tried for `host`, in order: the config's, else
/// the default, which adds the GitHub CLI for GitHub hosts.
pub fn credential_order(host: &str, config: &HostConfig) -> Vec<CredentialSource> {
    config.credentials.clone().unwrap_or_else(|| {
        if is_github(host, config) {
            CredentialSource::GITHUB_ORDER.to_vec()
        } else {
            CredentialSource::DEFAULT_ORDER.to_vec()
        }
    })
}

/// Tries each configured source in order and returns the first credential
/// found, with a note of what every source tried said.
pub async fn resolve(
    host: &str,
    scheme: &str,
    config: &HostConfig,
) -> (Option<Resolved>, Vec<Attempt>) {
    let mut attempts = Vec::new();
    for source in credential_order(host, config) {
        let found = match source {
            CredentialSource::Env => from_env(host, config, &mut attempts),
            CredentialSource::Keyring => from_keyring(host, &mut attempts).await,
            CredentialSource::Gh if is_github(host, config) => from_gh(host, &mut attempts).await,
            CredentialSource::Gh => {
                attempts.push(Attempt {
                    source: GH_SOURCE.into(),
                    outcome: "skipped: not a GitHub host".into(),
                });
                None
            }
            CredentialSource::GitCredential => {
                from_git_credential(host, scheme, &mut attempts).await
            }
        };
        if found.is_some() {
            return (found, attempts);
        }
    }
    (None, attempts)
}

fn from_env(host: &str, config: &HostConfig, attempts: &mut Vec<Attempt>) -> Option<Resolved> {
    for var in env_vars(host, config) {
        match std::env::var(&var) {
            Ok(value) if !value.trim().is_empty() => {
                let source = format!("env {var}");
                attempts.push(Attempt {
                    source: source.clone(),
                    outcome: "found".into(),
                });
                return Some(Resolved {
                    credential: Credential::Token(Secret::new(value.trim())),
                    source,
                });
            }
            _ => attempts.push(Attempt {
                source: format!("env {var}"),
                outcome: "not set".into(),
            }),
        }
    }
    None
}

async fn from_keyring(host: &str, attempts: &mut Vec<Attempt>) -> Option<Resolved> {
    let host_owned = host.to_owned();
    let result = tokio::task::spawn_blocking(move || keyring_get(&host_owned)).await;
    let outcome = match result {
        Ok(Ok(Some(token))) => {
            attempts.push(Attempt {
                source: "keyring".into(),
                outcome: "found".into(),
            });
            return Some(Resolved {
                credential: Credential::Token(token),
                source: "keyring".into(),
            });
        }
        Ok(Ok(None)) => "no entry".to_owned(),
        Ok(Err(e)) => unavailable(e),
        Err(e) => unavailable(e),
    };
    attempts.push(Attempt {
        source: "keyring".into(),
        outcome,
    });
    None
}

pub fn keyring_get(host: &str) -> Result<Option<Secret>, keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, host)?;
    match entry.get_password() {
        Ok(value) if !value.is_empty() => Ok(Some(Secret::new(value))),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn keyring_set(host: &str, token: &Secret) -> Result<(), keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, host)?.set_password(token.expose())
}

pub fn keyring_delete(host: &str) -> Result<bool, keyring::Error> {
    match keyring::Entry::new(KEYRING_SERVICE, host)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(e),
    }
}

async fn from_git_credential(
    host: &str,
    scheme: &str,
    attempts: &mut Vec<Attempt>,
) -> Option<Resolved> {
    let source = "git credential helper".to_owned();
    match git_credential_fill(host, scheme).await {
        Ok(Some((username, password))) => {
            attempts.push(Attempt {
                source: source.clone(),
                outcome: "found".into(),
            });
            Some(Resolved {
                credential: Credential::Basic { username, password },
                source,
            })
        }
        Ok(None) => {
            attempts.push(Attempt {
                source,
                outcome: "nothing stored for this host".into(),
            });
            None
        }
        Err(e) => {
            attempts.push(Attempt {
                source,
                outcome: unavailable(e),
            });
            None
        }
    }
}

const GH_SOURCE: &str = "gh auth token";

async fn from_gh(host: &str, attempts: &mut Vec<Attempt>) -> Option<Resolved> {
    let outcome = match gh_auth_token(host).await {
        Ok(Some(token)) => {
            attempts.push(Attempt {
                source: GH_SOURCE.into(),
                outcome: "found".into(),
            });
            return Some(Resolved {
                credential: Credential::Token(token),
                source: GH_SOURCE.into(),
            });
        }
        Ok(None) => "not signed in to this host".to_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "gh is not installed".to_owned(),
        Err(e) => unavailable(e),
    };
    attempts.push(Attempt {
        source: GH_SOURCE.into(),
        outcome,
    });
    None
}

/// The GitHub CLI's token for `host`, asked for without prompting.
async fn gh_auth_token(host: &str) -> std::io::Result<Option<Secret>> {
    let child = Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let out = tokio::time::timeout(GIT_CREDENTIAL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| std::io::Error::other("gh auth token timed out"))??;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((out.status.success() && !token.is_empty()).then(|| Secret::new(token)))
}

fn unavailable(e: impl std::fmt::Display) -> String {
    format!("unavailable: {e}")
}

/// How long git's credential helpers, and `gh`, get to answer.
const GIT_CREDENTIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Asks git's credential helpers for `host`, never interactively: no
/// terminal prompt, no askpass dialog, no Git Credential Manager window.
/// A helper that would have to ask returns nothing instead.
async fn git_credential_fill(
    host: &str,
    scheme: &str,
) -> std::io::Result<Option<(String, Secret)>> {
    let mut child = Command::new("git")
        .args(["-c", "credential.interactive=never", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("protocol={scheme}\nhost={host}\n\n").as_bytes())
            .await?;
    }
    let out = tokio::time::timeout(GIT_CREDENTIAL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| std::io::Error::other("git credential fill timed out"))??;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_credential_output(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

fn parse_credential_output(text: &str) -> Option<(String, Secret)> {
    let mut username = None;
    let mut password = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("username=") {
            username = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("password=") {
            password = Some(Secret::new(v));
        }
    }
    match (username, password) {
        (Some(u), Some(p)) if !p.is_empty() => Some((u, p)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names() {
        assert_eq!(
            default_env_var("git.example.com"),
            "GITFORGE_TOKEN_GIT_EXAMPLE_COM"
        );
        assert_eq!(
            default_env_var("localhost:3000"),
            "GITFORGE_TOKEN_LOCALHOST_3000"
        );
    }

    #[test]
    fn github_also_reads_gh_token() {
        let vars = env_vars("github.com", &HostConfig::default());
        assert_eq!(
            vars,
            vec!["GITFORGE_TOKEN_GITHUB_COM", "GH_TOKEN", "GITHUB_TOKEN"]
        );
    }

    #[test]
    fn github_enterprise_reads_the_enterprise_variables() {
        let ghes = HostConfig {
            kind: Some(ForgeKind::Github),
            ..HostConfig::default()
        };
        assert_eq!(
            env_vars("github.example.com", &ghes),
            vec![
                "GITFORGE_TOKEN_GITHUB_EXAMPLE_COM",
                "GH_ENTERPRISE_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN"
            ]
        );
        assert_eq!(
            env_vars("git.example.com", &HostConfig::default()),
            vec!["GITFORGE_TOKEN_GIT_EXAMPLE_COM"]
        );
        assert!(is_github("GitHub.com", &HostConfig::default()));
        assert!(!is_github("git.example.com", &HostConfig::default()));
    }

    #[test]
    fn parses_credential_helper_output() {
        let (user, pass) = parse_credential_output(
            "protocol=https\nhost=git.example.com\nusername=me\npassword=p#ss word\n",
        )
        .unwrap();
        assert_eq!(user, "me");
        assert_eq!(pass.expose(), "p#ss word");
        assert!(parse_credential_output("protocol=https\nhost=x\n").is_none());
        assert!(parse_credential_output("username=me\npassword=\n").is_none());
    }
}
