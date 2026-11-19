//! Every setting the server runs with, and where each one came from: a
//! command-line option, an environment variable, the config file, the git
//! remote or a default. This is `gitforge-mcp config`.
//!
//! It reads no credential and asks no forge anything. It names the token
//! variables it would read and says whether each is set; `auth status` says
//! which credential source answers, and `context` what the forge says.

mod hosts;
mod render;

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use clap::parser::ValueSource;
use serde::Serialize;

pub use render::render;

use crate::cache::{CACHE_ENV, Cache};
use crate::config::{CONFIG_ENV, Config, ConfigError};
use crate::history;
use crate::remote::{self, Binding, RemoteError};

/// Sets the log filter (`tracing` syntax). Logs go to stderr.
pub const LOG_ENV: &str = "GITFORGE_MCP_LOG";
/// The log filter when `GITFORGE_MCP_LOG` isn't set.
pub const DEFAULT_LOG: &str = "warn";

const DIR_ENV: &str = "GITFORGE_MCP_DIR";
const REMOTE_ENV: &str = "GITFORGE_MCP_REMOTE";
const READ_ONLY_ENV: &str = "GITFORGE_MCP_READ_ONLY";
const FROM_DEFAULT: &str = "default";

/// How a process-wide option was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Given {
    CommandLine,
    Environment,
    Default,
}

impl Given {
    /// How clap saw option `id`. A global option can be given before or
    /// after the subcommand; the deepest match that saw it knows.
    pub fn of(matches: &ArgMatches, id: &str) -> Self {
        let mut found = matches.value_source(id);
        let mut level = matches;
        while let Some((_, sub)) = level.subcommand() {
            found = sub.value_source(id).or(found);
            level = sub;
        }
        match found {
            Some(ValueSource::CommandLine) => Given::CommandLine,
            Some(ValueSource::EnvVariable) => Given::Environment,
            _ => Given::Default,
        }
    }
}

/// The process-wide options as the command line and environment gave them.
#[derive(Debug, Clone)]
pub struct Process {
    pub dir: (PathBuf, Given),
    pub remote: (Option<String>, Given),
    pub config: (Option<PathBuf>, Given),
    pub read_only: (bool, Given),
}

/// A value and where it came from: an option (`--dir`), a variable
/// (`GITFORGE_MCP_DIR`), `config file`, `git remote`, or `default`.
#[derive(Debug, Clone, Serialize)]
pub struct Setting<T> {
    pub value: T,
    pub from: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Loaded,
    /// No file there, which is the same as an empty one.
    Missing,
    /// Unreadable or not valid. The server ignores it and says so.
    Invalid,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigFile {
    pub path: Option<PathBuf>,
    pub from: String,
    pub state: FileState,
}

/// The repository the server binds to.
#[derive(Debug, Clone, Serialize)]
pub struct Repository {
    pub dir: PathBuf,
    pub remote: String,
    pub remote_url: String,
    pub host: String,
    pub full_name: String,
}

/// A token variable, and whether it holds anything. Never its value.
#[derive(Debug, Clone, Serialize)]
pub struct TokenVar {
    pub name: String,
    pub set: bool,
}

/// What the server would use for a host: a forge, or a CI system outside
/// it.
#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub host: String,
    /// The bound repository's forge, rather than a host only the file names.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bound: bool,
    /// `None`: found by asking when the server starts (a forge) or from
    /// commit status links (a CI system).
    pub kind: Setting<Option<String>>,
    pub base_url: Setting<Option<String>>,
    pub token_vars: Setting<Vec<TokenVar>>,
    /// Jenkins only: the user the token belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub credentials: Setting<Vec<&'static str>>,
}

/// Everything `gitforge-mcp config` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub config_file: ConfigFile,
    pub dir: Setting<PathBuf>,
    pub remote: Setting<Option<String>>,
    pub read_only: Setting<bool>,
    pub log: Setting<String>,
    pub cache: Setting<Option<PathBuf>>,
    pub history: Setting<Option<PathBuf>>,
    pub repository: Option<Repository>,
    pub hosts: Vec<Host>,
    pub ci: Vec<Host>,
    /// What would stop the server working, in words a person can act on.
    pub problems: Vec<String>,
}

/// Gathers the report. Reads the config file and the git remote, nothing
/// else.
pub async fn gather(process: &Process) -> Report {
    let config_path = process.config.0.clone().or_else(Config::default_path);
    let loaded = config_path
        .as_deref()
        .filter(|p| p.exists())
        .map(Config::load);
    let (config, state, config_problem) = settle_config(loaded);
    let bound = remote::bind(&process.dir.0, process.remote.0.as_deref()).await;
    let (binding, bind_problem) = settle_binding(bound);
    Report {
        config_file: ConfigFile {
            path: config_path,
            from: source(process.config.1, "--config", CONFIG_ENV),
            state,
        },
        remote: Setting {
            value: process
                .remote
                .0
                .clone()
                .or_else(|| binding.as_ref().map(|b| b.remote.clone())),
            from: source(process.remote.1, "--remote", REMOTE_ENV),
        },
        hosts: hosts::forge_hosts(&config, binding.as_ref()),
        ci: config
            .ci
            .iter()
            .map(|(n, c)| hosts::ci_host(n, c))
            .collect(),
        repository: binding.map(repository),
        problems: [config_problem, bind_problem]
            .into_iter()
            .flatten()
            .collect(),
        ..process_settings(process)
    }
}

/// The settings that don't depend on the repository. The rest are filled
/// in by `gather`.
fn process_settings(process: &Process) -> Report {
    let cache = Cache::default_location();
    let history = history::Store::default_location();
    Report {
        config_file: ConfigFile {
            path: None,
            from: String::new(),
            state: FileState::Missing,
        },
        dir: Setting {
            value: process.dir.0.clone(),
            from: source(process.dir.1, "--dir", DIR_ENV),
        },
        remote: Setting {
            value: None,
            from: String::new(),
        },
        read_only: Setting {
            value: process.read_only.0,
            from: source(process.read_only.1, "--read-only", READ_ONLY_ENV),
        },
        log: Setting {
            value: std::env::var(LOG_ENV).unwrap_or_else(|_| DEFAULT_LOG.to_owned()),
            from: env_source(LOG_ENV, FROM_DEFAULT),
        },
        cache: Setting {
            value: cache.path().map(Path::to_path_buf),
            from: env_source(CACHE_ENV, FROM_DEFAULT),
        },
        history: Setting {
            value: history.dir().map(Path::to_path_buf),
            // Beside the cache, so the same variable moves it.
            from: env_source(CACHE_ENV, FROM_DEFAULT),
        },
        repository: None,
        hosts: Vec::new(),
        ci: Vec::new(),
        problems: Vec::new(),
    }
}

fn source(given: Given, option: &str, var: &str) -> String {
    match given {
        Given::CommandLine => option,
        Given::Environment => var,
        Given::Default => FROM_DEFAULT,
    }
    .to_owned()
}

/// `var` if it is set, else `otherwise`.
fn env_source(var: &str, otherwise: &str) -> String {
    if std::env::var_os(var).is_some() {
        var.to_owned()
    } else {
        otherwise.to_owned()
    }
}

/// A missing file is an empty config; an invalid one is ignored, as the
/// server ignores it.
fn settle_config(
    loaded: Option<Result<Config, ConfigError>>,
) -> (Config, FileState, Option<String>) {
    let state = file_state(&loaded);
    let problem = loaded
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("the server ignores the config file: {e}"));
    (
        loaded.and_then(Result::ok).unwrap_or_default(),
        state,
        problem,
    )
}

fn file_state(loaded: &Option<Result<Config, ConfigError>>) -> FileState {
    match loaded {
        None => FileState::Missing,
        Some(Ok(_)) => FileState::Loaded,
        Some(Err(_)) => FileState::Invalid,
    }
}

fn settle_binding(bound: Result<Binding, RemoteError>) -> (Option<Binding>, Option<String>) {
    match bound {
        Ok(binding) => (Some(binding), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

fn repository(binding: Binding) -> Repository {
    Repository {
        host: binding.repo.host.clone(),
        full_name: binding.repo.full_name(),
        dir: binding.dir,
        remote: binding.remote,
        remote_url: binding.remote_url,
    }
}

#[cfg(test)]
mod tests;
