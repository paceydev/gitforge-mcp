//! The MCP handshake on the wire: a built binary over stdio, spoken to in
//! raw JSON-RPC lines rather than through rmcp's client, so it fails on
//! anything a real client would choke on: a stray line on stdout, a reply
//! that isn't JSON-RPC 2.0, no `tools/list`, or a server that doesn't exit
//! when its client goes away.
//!
//! It runs on every OS CI builds on, against the binary `cargo test` builds.
//! Point it at another build, a release binary say, with
//! `GITFORGE_SMOKE_BIN=path/to/gitforge-mcp cargo test --test handshake`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Long enough for a cold start on a loaded CI runner.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Absolute, because the server runs in another directory: a relative
/// `GITFORGE_SMOKE_BIN` is taken from where the test runs, the package root.
fn binary() -> PathBuf {
    let path = std::env::var_os("GITFORGE_SMOKE_BIN")
        .filter(|v| !v.is_empty())
        .map_or_else(|| env!("CARGO_BIN_EXE_gitforge-mcp").into(), PathBuf::from);
    std::path::absolute(&path).unwrap_or(path)
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Server {
    /// Starts the server in an empty directory that git won't look above,
    /// so it is unbound wherever the temporary directory is, with its own
    /// cache and no config file.
    fn start(dir: &std::path::Path) -> Self {
        let parent = dir.parent().expect("a temporary directory has a parent");
        let mut child = Command::new(binary())
            .current_dir(dir)
            .env("GIT_CEILING_DIRECTORIES", parent)
            .env("GITFORGE_MCP_CACHE", dir.join("probes.json"))
            .env("GITFORGE_MCP_CONFIG", dir.join("no-config.toml"))
            .env_remove("GITFORGE_MCP_DIR")
            .env_remove("GITFORGE_MCP_REMOTE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("{} doesn't start: {e}", binary().display()));
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{message}").expect("write to the server");
        stdin.flush().expect("flush to the server");
    }

    /// The reply to request `id`. Every line before it must be a JSON-RPC
    /// 2.0 message too: stdout is the protocol channel and nothing else.
    fn reply(&self, id: u64) -> Value {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no reply to request {id}: {e}"));
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout line isn't JSON ({e}): {line}"));
            assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
            if message["id"] == id {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {line}"
                );
                return message["result"].clone();
            }
        }
    }

    /// Closes stdin, as a client does when it goes away, and waits for the
    /// server to exit.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for the server") {
                return status;
            }
            if Instant::now() >= deadline {
                self.child.kill().ok();
                panic!("the server didn't exit within {EXIT_TIMEOUT:?} of stdin closing");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn initialize_then_tools_list_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path());

    server.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "handshake-test", "version": "0" },
        },
    }));
    let init = server.reply(1);
    assert_eq!(init["serverInfo"]["name"], "gitforge-mcp", "{init}");
    assert_eq!(
        init["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "{init}"
    );
    assert_eq!(init["protocolVersion"], "2025-06-18", "{init}");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    assert!(
        init["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("forge_context")),
        "{init}"
    );

    server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    server.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = server.reply(2);
    let tools = list["tools"].as_array().expect("tools is an array");
    // Unbound, only the always-on tool is offered.
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["forge_context"], "{list}");
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    }

    let status = server.close();
    assert!(status.success(), "exited with {status}");
}
