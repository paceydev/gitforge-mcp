//! Helpers for tests that run the real binary over stdio.

#![allow(dead_code)]

pub mod forge;

use std::path::Path;
use std::process::Command as StdCommand;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, JsonObject,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;

pub type Client = RunningService<RoleClient, ()>;

/// A git checkout in `dir` whose origin is `remote_url`.
pub fn git_checkout(dir: &Path, remote_url: &str) {
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", remote_url],
    ] {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }
}

/// The server binary, run in `dir` with an isolated cache and no config
/// file. `env` adds variables, typically a `GITFORGE_TOKEN_<HOST>`.
pub fn command(dir: &Path, env: &[(&str, &str)]) -> Command {
    let cache = dir.join(".probes.json");
    let config = dir.join(".no-config.toml");
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Command::new(env!("CARGO_BIN_EXE_gitforge-mcp")).configure(|cmd| {
        cmd.current_dir(dir)
            .env("GITFORGE_MCP_CACHE", &cache)
            .env("GITFORGE_MCP_CONFIG", &config)
            .env_remove("GITFORGE_MCP_DIR")
            .env_remove("GITFORGE_MCP_REMOTE");
        for (k, v) in &env {
            cmd.env(k, v);
        }
    })
}

/// Starts the server in `dir` as an MCP client with no capabilities.
pub async fn start(dir: &Path, env: &[(&str, &str)]) -> Client {
    ().serve(TokioChildProcess::new(command(dir, env)).expect("spawns"))
        .await
        .expect("MCP handshake")
}

/// Starts the server in `dir` as a client that supports MCP tasks.
pub async fn start_with_tasks(
    dir: &Path,
    env: &[(&str, &str)],
) -> RunningService<RoleClient, ClientConfig> {
    ClientConfig::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::new("tests", "0"),
    )
    .serve(TokioChildProcess::new(command(dir, env)).expect("spawns"))
    .await
    .expect("MCP handshake")
}

pub async fn tool_names(client: &Client) -> Vec<String> {
    client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect()
}

pub async fn forge_context(client: &Client, refresh: bool) -> Value {
    let mut args = JsonObject::new();
    args.insert("refresh".into(), Value::Bool(refresh));
    let result = client
        .call_tool(CallToolRequestParams::new("forge_context").with_arguments(args))
        .await
        .expect("forge_context");
    result
        .structured_content
        .expect("forge_context returns structured content")
}

/// Calls `tool` and returns its structured result, panicking on a tool
/// error with the error's text.
pub async fn call(client: &Client, tool: &'static str, args: JsonObject) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    if result.is_error == Some(true) {
        panic!("{tool} failed: {:?}", result.content);
    }
    result
        .structured_content
        .unwrap_or_else(|| panic!("{tool} returned no structured content"))
}

/// Calls `tool`; `Err` holds the text of a tool error.
pub async fn try_call(
    client: &Client,
    tool: &'static str,
    args: JsonObject,
) -> Result<Value, String> {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(format!("{:?}", result.content));
    }
    result
        .structured_content
        .ok_or_else(|| format!("{tool} returned no structured content"))
}

/// The tools a client was offered, with their schemas.
pub async fn tools(client: &Client) -> Vec<rmcp::model::Tool> {
    client.list_all_tools().await.expect("tools/list")
}

/// An API path with its parameter names erased, so Gitea's `{run}`,
/// Forgejo's `{run_id}` and Woodpecker's `{repo_id}` compare equal.
pub fn erase_params(path: &str) -> String {
    let mut out = String::new();
    let mut in_param = false;
    for c in path.chars() {
        match c {
            '{' => {
                in_param = true;
                out.push_str("{}");
            }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(c),
        }
    }
    out
}

/// Gives the server in `dir` a config file: `command` points
/// `GITFORGE_MCP_CONFIG` at this path, which is otherwise absent.
pub fn write_config(dir: &Path, toml: &str) {
    std::fs::write(dir.join(".no-config.toml"), toml).expect("config written");
}
