//! The last successful probe of each repository, so a forge that is down (a
//! work server switched off out of hours, a laptop off the VPN) still gets
//! its usual tools, which then fail with a clear "unreachable" instead of
//! vanishing.
//!
//! Holds only what a probe learned: kind, version, login, permissions.
//! Never a credential.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::site::{RepoInfo, ServerInfo, User};

/// Overrides the cache file's location (tests use it).
pub const CACHE_ENV: &str = "GITFORGE_MCP_CACHE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedProbe {
    pub server: ServerInfo,
    pub user: Option<User>,
    pub repo: RepoInfo,
    pub probed_at_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    repos: BTreeMap<String, CachedProbe>,
}

pub struct Cache {
    path: Option<PathBuf>,
}

impl Cache {
    /// Probe without reading or writing cached state (for setup diagnostics).
    pub fn disabled() -> Self {
        Self { path: None }
    }

    pub fn default_location() -> Self {
        let path = std::env::var_os(CACHE_ENV)
            .map(PathBuf::from)
            .or_else(|| dirs::cache_dir().map(|d| d.join("gitforge-mcp").join("probes.json")));
        Self { path }
    }

    #[cfg(test)]
    pub fn at(path: &std::path::Path) -> Self {
        Self {
            path: Some(path.to_owned()),
        }
    }

    /// Where the cache file is, if there is anywhere to put it.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    pub fn key(host: &str, full_name: &str) -> String {
        format!("{host}/{full_name}")
    }

    pub fn get(&self, key: &str) -> Option<CachedProbe> {
        self.read().repos.remove(key)
    }

    /// Best effort: a cache that can't be written costs the fallback, never
    /// the request.
    pub fn put(&self, key: &str, probe: CachedProbe) {
        let Some(path) = &self.path else { return };
        let mut file = self.read();
        file.repos.insert(key.to_owned(), probe);
        let Ok(text) = serde_json::to_string_pretty(&file) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    fn read(&self) -> CacheFile {
        self.path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::Permissions;
    use crate::version::{ForgeKind, Version};

    #[test]
    fn round_trips_and_survives_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probes.json");
        let cache = Cache::at(&path);
        assert!(cache.get("h/a/b").is_none());

        std::fs::write(&path, "not json").unwrap();
        let probe = CachedProbe {
            server: ServerInfo {
                kind: ForgeKind::Forgejo,
                version: Some(Version::new(15, 0, 11)),
                version_raw: Some("15.0.11+gitea-1.22.0".into()),
                detected_by: "/api/forgejo/v1/version".into(),
            },
            user: Some(User {
                login: "me".into(),
                site_admin: false,
            }),
            repo: RepoInfo {
                full_name: "a/b".into(),
                default_branch: "main".into(),
                private: true,
                archived: false,
                permissions: Permissions {
                    admin: true,
                    push: true,
                    pull: true,
                },
                actions_enabled: Some(true),
                auto_merge_allowed: None,
            },
            probed_at_unix: 1,
        };
        cache.put("h/a/b", probe.clone());
        assert_eq!(cache.get("h/a/b"), Some(probe));
    }
}
