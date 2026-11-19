//! Forge kinds, CI system kinds and server versions.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Gitea,
    Forgejo,
    Github,
}

impl fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ForgeKind::Gitea => "Gitea",
            ForgeKind::Forgejo => "Forgejo",
            ForgeKind::Github => "GitHub",
        })
    }
}

/// A CI system outside the forge that gitforge-mcp reaches through its own
/// API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CiKind {
    Woodpecker,
    Jenkins,
}

impl fmt::Display for CiKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CiKind::Woodpecker => "Woodpecker",
            CiKind::Jenkins => "Jenkins",
        })
    }
}

// Releases the capability table and the drivers name, where an API
// appeared or changed. Each is checked against that release's swagger spec.
pub const GITEA_1_24: Version = Version::new(1, 24, 0);
pub const GITEA_1_25: Version = Version::new(1, 25, 0);
pub const GITEA_1_26: Version = Version::new(1, 26, 0);
pub const GITEA_28: Version = Version::new(28, 0, 0);
pub const FORGEJO_9: Version = Version::new(9, 0, 0);
pub const FORGEJO_11: Version = Version::new(11, 0, 0);
pub const FORGEJO_12: Version = Version::new(12, 0, 0);
pub const FORGEJO_15: Version = Version::new(15, 0, 0);
pub const FORGEJO_16: Version = Version::new(16, 0, 0);
pub const FORGEJO_17: Version = Version::new(17, 0, 0);
/// The oldest GitHub Enterprise Server whose API description has every
/// endpoint the GitHub driver uses (checked in github/rest-api-description).
pub const GHES_3_14: Version = Version::new(3, 14, 0);
/// Workflow dispatch can return the run it started (`return_run_details`).
pub const GHES_3_21: Version = Version::new(3, 21, 0);

/// A server version, compared numerically.
///
/// Gitea numbered its releases 1.x up to 1.27 and then switched to integer
/// majors (28.0.0 in September 2026), so 28.0.0 > 1.27.0 compares the right
/// way without special cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses what `/api/v1/version` and `/api/forgejo/v1/version` return.
    ///
    /// Build metadata after `+` is dropped. Forgejo always appends
    /// `+gitea-1.22.0` there: a fixed compatibility marker, never the real
    /// Gitea version, so it must not be read. A pre-release suffix such as
    /// `16.0.0-dev-123-abc` counts as the release it leads up to.
    pub fn parse(raw: &str) -> Option<Self> {
        let core = raw.trim().trim_start_matches('v');
        let core = core.split('+').next()?;
        let core = core.split('-').next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        Some(Self::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Serialized as `"15.0.11"`, the way people write versions.
impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Version::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("bad version {raw:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forge_version_strings() {
        assert_eq!(
            Version::parse("15.0.11+gitea-1.22.0"),
            Some(Version::new(15, 0, 11))
        );
        assert_eq!(
            Version::parse("16.0.0-dev-1234-abcdef+gitea-1.22.0"),
            Some(Version::new(16, 0, 0))
        );
        assert_eq!(Version::parse("1.25.3"), Some(Version::new(1, 25, 3)));
        assert_eq!(Version::parse("28.3.0"), Some(Version::new(28, 3, 0)));
        assert_eq!(Version::parse("29.0.0+dev"), Some(Version::new(29, 0, 0)));
        assert_eq!(Version::parse("v1.24"), Some(Version::new(1, 24, 0)));
        assert_eq!(Version::parse("development"), None);
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn serializes_as_a_string() {
        let v = Version::new(15, 0, 11);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"15.0.11\"");
        assert_eq!(serde_json::from_str::<Version>("\"15.0.11\"").unwrap(), v);
    }

    #[test]
    fn gitea_integer_majors_sort_after_1x() {
        assert!(Version::new(28, 0, 0) > Version::new(1, 27, 9));
    }
}
