//! gitforge-mcp: an MCP server for CI, pull requests and repository
//! settings on Gitea, Forgejo and GitHub, bound to one local repository.

pub mod auth;
pub mod cache;
pub mod capability;
pub mod ci;
pub mod config;
pub mod config_report;
pub mod context;
pub mod estimate;
pub mod external;
pub mod forge;
pub mod history;
pub mod logs;
pub mod pulls;
pub mod remote;
pub mod repo_settings;
pub mod runners;
pub mod secret;
pub mod server;
pub mod settings;
pub mod site;
pub mod tools;
pub mod version;
