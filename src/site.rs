//! What a forge says about itself, the caller and a repository, in terms
//! every forge shares. The probe gathers these, the cache keeps them and the
//! capability table reads them, none of which needs a driver.

use serde::{Deserialize, Serialize};

use crate::version::{ForgeKind, Version};

/// What is running at a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub kind: ForgeKind,
    /// `None` for github.com, which has no version, and for a self-hosted
    /// build whose version string doesn't parse.
    pub version: Option<Version>,
    /// The version string as the server reported it.
    pub version_raw: Option<String>,
    /// How the kind was decided: the endpoint that answered, or `config`.
    pub detected_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub login: String,
    pub site_admin: bool,
}

/// The caller's access to a repository, in the three levels every forge has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    pub admin: bool,
    pub push: bool,
    pub pull: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    pub permissions: Permissions,
    /// Whether Actions is enabled for the repository. `None` when the forge
    /// doesn't say without extra permissions (GitHub).
    pub actions_enabled: Option<bool>,
    /// Whether pull requests may be set to merge once their checks pass, a
    /// repository setting on GitHub. `None` where it is no setting (Gitea
    /// and Forgejo) or the forge doesn't tell this caller.
    #[serde(default)]
    pub auto_merge_allowed: Option<bool>,
}
