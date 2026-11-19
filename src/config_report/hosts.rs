//! What the server would use for each host: the repository's forge, the
//! other forges in the config file, and CI systems outside the forge.

use serde::Serialize;

use super::{FROM_DEFAULT, Host, Setting, TokenVar};
use crate::auth;
use crate::config::{CiHostConfig, Config, CredentialSource, HostConfig};
use crate::remote::Binding;

const FROM_FILE: &str = "config file";

/// The bound repository's host first, then every other host in the file.
fn host_names(config: &Config, binding: Option<&Binding>) -> Vec<(String, bool, Option<String>)> {
    let bound = binding.map(|b| (b.repo.host.clone(), true, Some(b.repo.base_url.to_string())));
    let bound_name = bound.as_ref().map(|(name, _, _)| name.clone());
    let others = config
        .hosts
        .keys()
        .filter(|name| Some(*name) != bound_name.as_ref())
        .map(|name| (name.clone(), false, None));
    bound.into_iter().chain(others).collect()
}

pub(super) fn forge_hosts(config: &Config, binding: Option<&Binding>) -> Vec<Host> {
    host_names(config, binding)
        .into_iter()
        .map(|(name, bound, remote_base)| {
            forge_host(&name, bound, &config.host(&name), remote_base)
        })
        .collect()
}

fn forge_host(name: &str, bound: bool, c: &HostConfig, remote_base: Option<String>) -> Host {
    Host {
        host: name.to_owned(),
        bound,
        kind: from_file(c.kind.map(|k| spelling(&k))),
        base_url: base_url(c.base_url.as_ref(), remote_base),
        token_vars: token_vars(auth::env_vars(name, c), c.token_env.is_some()),
        username: None,
        credentials: credentials(auth::credential_order(name, c), c.credentials.is_some()),
    }
}

pub(super) fn ci_host(name: &str, c: &CiHostConfig) -> Host {
    let lookup = c.credential_lookup();
    Host {
        host: name.to_owned(),
        bound: false,
        kind: from_file(c.kind.map(|k| spelling(&k))),
        base_url: base_url(c.base_url.as_ref(), None),
        token_vars: token_vars(auth::env_vars(name, &lookup), c.token_env.is_some()),
        username: c.username.clone(),
        credentials: credentials(
            auth::credential_order(name, &lookup),
            c.credentials.is_some(),
        ),
    }
}

fn from_file(value: Option<String>) -> Setting<Option<String>> {
    let from = if value.is_some() {
        FROM_FILE
    } else {
        FROM_DEFAULT
    };
    Setting {
        value,
        from: from.to_owned(),
    }
}

fn base_url(file: Option<&url::Url>, remote: Option<String>) -> Setting<Option<String>> {
    let (value, from) = match (file, remote) {
        (Some(url), _) => (Some(url.to_string()), FROM_FILE),
        (None, Some(url)) => (Some(url), "git remote"),
        (None, None) => (None, FROM_DEFAULT),
    };
    Setting {
        value,
        from: from.to_owned(),
    }
}

fn token_vars(names: Vec<String>, configured: bool) -> Setting<Vec<TokenVar>> {
    let value = names
        .into_iter()
        .map(|name| TokenVar {
            set: std::env::var(&name).is_ok_and(|v| !v.trim().is_empty()),
            name,
        })
        .collect();
    let from = if configured { FROM_FILE } else { FROM_DEFAULT };
    Setting {
        value,
        from: from.to_owned(),
    }
}

fn credentials(order: Vec<CredentialSource>, configured: bool) -> Setting<Vec<&'static str>> {
    let from = if configured { FROM_FILE } else { FROM_DEFAULT };
    Setting {
        value: order.into_iter().map(CredentialSource::as_str).collect(),
        from: from.to_owned(),
    }
}

/// A kind as the config file spells it (`forgejo`, not `Forgejo`).
fn spelling(kind: &impl Serialize) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
