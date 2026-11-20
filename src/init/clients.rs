//! Small edits to the clients' user configuration, never a token in the preview.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::Client;

pub struct Plan {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
    pub snippet: String,
}

impl Plan {
    pub fn changed(&self) -> bool {
        self.before.as_deref() != Some(self.after.as_slice())
    }
}

pub fn path(client: Client) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot find your home directory")?;
    Ok(match client {
        Client::Claude => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join(".claude.json"),
        Client::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        _ => {
            return Err(anyhow!(
                "Choose one client when resolving its configuration"
            ));
        }
    })
}

pub fn plan(client: Client, path: &Path, exe: &Path, args: &[String]) -> Result<Plan> {
    let path = if path.exists() {
        // Preserve a dotfile symlink instead of replacing the link itself.
        std::fs::canonicalize(path)?
    } else {
        ensure!(
            !path.is_symlink(),
            "Configuration is a dangling symlink: {}",
            path.display()
        );
        std::path::absolute(path)?
    };
    let before = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(anyhow!(
                "Cannot read client configuration: {}",
                path.display()
            ));
        }
    };
    let text = match &before {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| anyhow!("Client configuration is not UTF-8: {}", path.display()))?,
        None => "",
    };
    let exe = exe
        .to_str()
        .context("The executable path must be valid Unicode")?;
    let (after, snippet) = match client {
        Client::Claude => claude(text, exe, args)?,
        Client::Codex => codex(text, exe, args)?,
        _ => return Err(anyhow!("Choose one client when planning its configuration")),
    };
    // Avoid backups and formatting-only writes on repeated init.
    let unchanged = match client {
        Client::Claude => {
            !text.is_empty()
                && serde_json::from_str::<Value>(text).ok()
                    == serde_json::from_str::<Value>(&after).ok()
        }
        _ => text == after,
    };
    Ok(Plan {
        path,
        after: if unchanged {
            text.as_bytes().to_vec()
        } else {
            after.into_bytes()
        },
        before,
        snippet,
    })
}

fn claude(text: &str, exe: &str, args: &[String]) -> Result<(String, String)> {
    ensure!(
        !exe.contains("${") && args.iter().all(|arg| !arg.contains("${")),
        "A path or argument contains Claude Code variable-expansion syntax; use a plain path/argument"
    );
    // Don't include parser errors: they can echo secrets from the source file.
    let mut doc: Value = if text.is_empty() {
        json!({})
    } else {
        serde_json::from_str(text).map_err(|_| {
            anyhow!("Claude Code configuration is not valid JSON; fix it before running init")
        })?
    };
    let root = doc
        .as_object_mut()
        .context("Claude Code configuration must be a JSON object")?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude Code mcpServers must be an object")?;
    let server = servers
        .entry("gitforge")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Existing gitforge entry must be an object; edit it manually")?;
    ensure!(
        !server.contains_key("url")
            && server
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "stdio"),
        "Existing gitforge entry uses another transport; rename or remove it manually first"
    );
    server.insert("type".to_owned(), json!("stdio"));
    server.insert("command".to_owned(), json!(exe));
    server.insert("args".to_owned(), json!(args));
    let snippet = serde_json::to_string_pretty(
        &json!({"mcpServers": {"gitforge": {"type":"stdio", "command":exe,"args":args}}}),
    )?;
    Ok((serde_json::to_string_pretty(&doc)? + "\n", snippet))
}

fn codex(text: &str, exe: &str, args: &[String]) -> Result<(String, String)> {
    let mut doc: DocumentMut = text.parse().map_err(|_| {
        anyhow!("Codex configuration is not valid TOML; fix it before running init")
    })?;
    if doc.get("mcp_servers").is_none() {
        let mut table = Table::new();
        table.set_implicit(true);
        doc["mcp_servers"] = Item::Table(table);
    }
    let servers = doc["mcp_servers"]
        .as_table_like_mut()
        .context("Codex mcp_servers must be a table")?;
    if !servers.contains_key("gitforge") {
        servers.insert("gitforge", Item::Table(Table::new()));
    }
    let server = servers
        .get_mut("gitforge")
        .and_then(Item::as_table_like_mut)
        .context("Existing Codex gitforge entry must be a table; edit it manually")?;
    ensure!(
        !server.contains_key("url"),
        "Existing gitforge entry uses another transport; rename or remove it manually first"
    );
    // Leave an already-correct entry byte-for-byte intact, including comments.
    let same_command = server.get("command").and_then(Item::as_str) == Some(exe);
    let same_args = match server.get("args") {
        None => args.is_empty(),
        Some(item) => item.as_array().is_some_and(|array| {
            array.len() == args.len()
                && array
                    .iter()
                    .zip(args)
                    .all(|(value, arg)| value.as_str() == Some(arg.as_str()))
        }),
    };
    if !same_command || !same_args {
        server.insert("command", value(exe));
        let mut array = Array::new();
        for arg in args {
            array.push(arg);
        }
        server.insert("args", value(array));
    }
    let mut snippet = DocumentMut::new();
    snippet["mcp_servers"]["gitforge"]["command"] = value(exe);
    let mut array = Array::new();
    for arg in args {
        array.push(arg);
    }
    snippet["mcp_servers"]["gitforge"]["args"] = value(array);
    Ok((doc.to_string(), snippet.to_string()))
}

pub fn arguments(options: &crate::context::Options, include_config: bool) -> Result<Vec<String>> {
    let mut args = Vec::new();
    if let Some(path) = options.config_path.as_ref().filter(|_| include_config) {
        args.extend([
            "--config".to_owned(),
            std::path::absolute(path)?
                .to_str()
                .context("Config path is not valid Unicode")?
                .to_owned(),
        ]);
    }
    if let Some(remote) = &options.remote {
        args.extend(["--remote".to_owned(), remote.clone()]);
    }
    if options.read_only {
        args.push("--read-only".to_owned());
    }
    Ok(args)
}
