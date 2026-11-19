//! Which repository this server is bound to: a directory's git remote, read
//! and parsed into a host, an owner and a repo name.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::process::Command;
use url::Url;

/// A repository on a forge, as its git remote names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoRef {
    /// `host` or `host:port`, as git credential helpers key it.
    pub host: String,
    /// The forge's web root, e.g. `https://git.example.com` or, for an
    /// instance served under a path, `https://example.com/git`.
    pub base_url: Url,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RemoteError {
    #[error("{0} is not inside a git repository")]
    NotARepo(String),
    #[error("the repository at {0} has no remotes")]
    NoRemotes(String),
    #[error(
        "the repository at {dir} has several remotes and none is called origin ({remotes}); set GITFORGE_MCP_REMOTE to choose one"
    )]
    AmbiguousRemote { dir: String, remotes: String },
    #[error("remote {0} does not exist")]
    NoSuchRemote(String),
    #[error("remote URL {0} does not point at a hosted forge")]
    NotHosted(String),
    #[error("remote URL {0} does not end in <owner>/<repo>")]
    NoOwnerRepo(String),
    #[error("could not run git: {0}")]
    Git(String),
}

/// Parses a git remote URL. Accepts `https://` and `http://` URLs, `ssh://`
/// and `git://` URLs, and scp-style `user@host:owner/repo.git`.
///
/// For SSH remotes the web root is assumed to be `https://<host>`: the SSH
/// port and user say nothing about where the web server is. Set `base_url`
/// for the host in the config file if that guess is wrong.
pub fn parse_remote_url(remote: &str) -> Result<RepoRef, RemoteError> {
    let trimmed = remote.trim();
    if let Ok(url) = Url::parse(trimmed)
        && url.has_host()
    {
        return match url.scheme() {
            "http" | "https" => from_http_url(trimmed, &url),
            "ssh" | "git" | "git+ssh" | "ssh+git" => {
                let host = url.host_str().unwrap_or_default();
                from_host_and_path(trimmed, host, url.path())
            }
            _ => Err(RemoteError::NotHosted(trimmed.to_owned())),
        };
    }
    // scp-like syntax: [user@]host:path, where the host has no slash before
    // the colon. A Windows path such as C:\repo has a one-letter "host".
    if let Some((left, path)) = trimmed.split_once(':')
        && !left.contains('/')
        && !path.starts_with("//")
    {
        let host = left.rsplit('@').next().unwrap_or(left);
        if host.len() > 1 {
            return from_host_and_path(trimmed, host, path);
        }
    }
    Err(RemoteError::NotHosted(trimmed.to_owned()))
}

fn from_http_url(remote: &str, url: &Url) -> Result<RepoRef, RemoteError> {
    let host = url.host_str().unwrap_or_default();
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let segments = path_segments(url.path());
    if segments.len() < 2 {
        return Err(RemoteError::NoOwnerRepo(remote.to_owned()));
    }
    let (prefix, tail) = segments.split_at(segments.len() - 2);
    let mut base = url.clone();
    base.set_username("").ok();
    base.set_password(None).ok();
    base.set_query(None);
    base.set_fragment(None);
    base.set_path(&prefix.join("/"));
    Ok(RepoRef {
        host,
        base_url: base,
        owner: tail[0].clone(),
        name: tail[1].clone(),
    })
}

fn from_host_and_path(remote: &str, host: &str, path: &str) -> Result<RepoRef, RemoteError> {
    let segments = path_segments(path);
    if host.is_empty() || segments.len() < 2 {
        return Err(RemoteError::NoOwnerRepo(remote.to_owned()));
    }
    let host = host.to_owned();
    let base_url = Url::parse(&format!("https://{host}"))
        .map_err(|_| RemoteError::NotHosted(remote.to_owned()))?;
    let n = segments.len();
    Ok(RepoRef {
        host,
        base_url,
        owner: segments[n - 2].clone(),
        name: segments[n - 1].clone(),
    })
}

fn path_segments(path: &str) -> Vec<String> {
    let mut segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".git")
    {
        *last = stripped.to_owned();
    }
    segments
}

/// A directory's repository and the remote URL it was bound through.
#[derive(Debug, Clone, Serialize)]
pub struct Binding {
    pub dir: PathBuf,
    pub remote: String,
    pub remote_url: String,
    pub repo: RepoRef,
}

/// Reads `dir`'s git remote: `remote` if given, else `origin`, else the only
/// remote there is.
pub async fn bind(dir: &Path, remote: Option<&str>) -> Result<Binding, RemoteError> {
    let shown = dir.display().to_string();
    let top = git(dir, &["rev-parse", "--show-toplevel"])
        .await?
        .ok_or_else(|| RemoteError::NotARepo(shown.clone()))?;
    let top = PathBuf::from(top);
    let remotes = git(&top, &["remote"]).await?.unwrap_or_default();
    let remotes: Vec<&str> = remotes.lines().filter(|l| !l.is_empty()).collect();
    let chosen = match remote {
        Some(name) if remotes.contains(&name) => name.to_owned(),
        Some(name) => return Err(RemoteError::NoSuchRemote(name.to_owned())),
        None if remotes.contains(&"origin") => "origin".to_owned(),
        None if remotes.len() == 1 => remotes[0].to_owned(),
        None if remotes.is_empty() => return Err(RemoteError::NoRemotes(shown)),
        None => {
            return Err(RemoteError::AmbiguousRemote {
                dir: shown,
                remotes: remotes.join(", "),
            });
        }
    };
    let url = git(&top, &["remote", "get-url", &chosen])
        .await?
        .ok_or_else(|| RemoteError::NoSuchRemote(chosen.clone()))?;
    let repo = parse_remote_url(&url)?;
    Ok(Binding {
        dir: top,
        remote: chosen,
        remote_url: redact_userinfo(&url),
        repo,
    })
}

/// Runs git in `dir`. `Ok(None)` means git ran and failed.
async fn git(dir: &Path, args: &[&str]) -> Result<Option<String>, RemoteError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| RemoteError::Git(e.to_string()))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// A remote URL can carry a token as its password; never repeat it.
fn redact_userinfo(remote: &str) -> String {
    match Url::parse(remote) {
        Ok(mut url) if url.password().is_some() => {
            url.set_password(Some("redacted")).ok();
            url.to_string()
        }
        _ => remote.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(remote: &str) -> (String, String, String, String) {
        let r = parse_remote_url(remote).unwrap();
        (r.host, r.base_url.to_string(), r.owner, r.name)
    }

    #[test]
    fn https_remotes() {
        assert_eq!(
            parsed("https://git.example.com/acme/widgets.git"),
            (
                "git.example.com".into(),
                "https://git.example.com/".into(),
                "acme".into(),
                "widgets".into()
            )
        );
        assert_eq!(
            parsed("https://user:tok@git.example.com:8443/acme/widgets/"),
            (
                "git.example.com:8443".into(),
                "https://git.example.com:8443/".into(),
                "acme".into(),
                "widgets".into()
            )
        );
    }

    #[test]
    fn instance_under_a_path_keeps_its_prefix() {
        assert_eq!(
            parsed("http://localhost:3000/forge/acme/widgets.git"),
            (
                "localhost:3000".into(),
                "http://localhost:3000/forge".into(),
                "acme".into(),
                "widgets".into()
            )
        );
    }

    #[test]
    fn ssh_remotes_assume_https_web_root() {
        assert_eq!(
            parsed("git@github.com:acme/widgets.git"),
            (
                "github.com".into(),
                "https://github.com/".into(),
                "acme".into(),
                "widgets".into()
            )
        );
        assert_eq!(
            parsed("ssh://git@git.example.com:2222/acme/widgets.git"),
            (
                "git.example.com".into(),
                "https://git.example.com/".into(),
                "acme".into(),
                "widgets".into()
            )
        );
    }

    #[test]
    fn rejects_local_and_incomplete_remotes() {
        assert!(matches!(
            parse_remote_url("/srv/git/widgets.git"),
            Err(RemoteError::NotHosted(_))
        ));
        assert!(matches!(
            parse_remote_url(r"C:\repos\widgets"),
            Err(RemoteError::NotHosted(_))
        ));
        assert!(matches!(
            parse_remote_url("https://git.example.com/widgets"),
            Err(RemoteError::NoOwnerRepo(_))
        ));
    }

    #[test]
    fn redacts_passwords_in_remote_urls() {
        assert_eq!(
            redact_userinfo("https://me:secret@git.example.com/a/b.git"),
            "https://me:redacted@git.example.com/a/b.git"
        );
        assert_eq!(
            redact_userinfo("git@host:a/b.git"),
            "git@host:a/b.git".to_owned()
        );
    }
}
