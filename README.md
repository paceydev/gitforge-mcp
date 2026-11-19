# gitforge-mcp

An MCP server that lets coding agents check CI, investigate failures, work with pull requests and manage repository settings on **GitHub**, **Gitea** and **Forgejo**. It also connects to **Woodpecker** and **Jenkins** for CI running outside the forge.

It works with the Git checkout you open in your agent, detects the forge and its version, and offers tools according to the capabilities and permissions it finds. One executable works with Claude Code, Codex and other MCP clients.

## Installation

Download a binary for macOS, Linux or Windows from the [latest release](https://github.com/paceydev/gitforge-mcp/releases/latest). Builds are available for x64 and ARM64 on all three systems. Git must be installed to discover the checkout and its remote.

| System | Install |
|---|---|
| macOS | Download the ZIP for Apple Silicon (`aarch64`) or Intel (`x86_64`), extract it, and place `gitforge-mcp` on your PATH. |
| Windows | Download the ZIP for x64 (`x86_64`) or ARM64 (`aarch64`), extract it, and add the directory containing `gitforge-mcp.exe` to your PATH. |
| Linux | Use the installer below, or extract a release archive. The binaries require glibc 2.35 or later. |

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/paceydev/gitforge-mcp/v0.2.0/install.sh | sh
```

The Linux installer verifies the archive checksum and installs into `~/.local/bin`. Homebrew installation is planned. The first release binaries are unsigned; [installation details](docs/src/guide/getting-started.md) explain checksums, attestations and platform security prompts.

To build from source, install Rust 1.89 or later, then run:

```sh
cargo install --locked --git https://github.com/paceydev/gitforge-mcp --tag v0.2.0 gitforge-mcp
```

Check the installation:

```sh
gitforge-mcp --version
```

A source install puts the executable in Cargo's binary directory. Ensure the installation directory is on your PATH. Desktop MCP clients may need the executable's full path.

## Quick start

### Claude Code

Register the server for your projects:

```sh
claude mcp add --scope user gitforge -- gitforge-mcp
```

### Codex

Add this to your Codex configuration file, `~/.codex/config.toml`:

```toml
[mcp_servers.gitforge]
command = "gitforge-mcp"
```

### Credentials

For GitHub, the server can use your existing GitHub CLI login. It can also use credentials from Git's HTTPS credential helper. If you need to supply a forge token, store it in the OS keyring:

```sh
gitforge-mcp auth set git.example.com
```

Replace `git.example.com` with your forge's hostname. See the [credential guide](docs/src/guide/credentials.md) for token permissions and other credential sources.

From your project checkout, check the configuration and connection:

```sh
gitforge-mcp config
gitforge-mcp context
```

Open the same project in your agent and ask about its CI or pull requests. Write operations are available where your permissions and the forge allow them. Add `--read-only` to the server's arguments if you want to limit it to reading.

## Example prompts

- “Did my latest push pass? What is still running?”
- “Why did CI fail? Show me the relevant log lines.”
- “Which repositories are holding the runners my jobs need?”
- “How long does CI usually take on this branch?”
- “Wait for CI to finish and tell me the result.”
- “Is pull request 12 ready to merge? Show me its checks and reviews.”
- “Create a pull request from this branch into develop.”
- “Show me a plan to require one approval before merging into main.”

Available actions depend on the forge, its version and your permissions. The support reference below covers the differences.

## Documentation

| Topic | Guide |
|---|---|
| Installation and client setup | [Getting started](docs/src/guide/getting-started.md) |
| Tokens and permissions | [Credentials](docs/src/guide/credentials.md) |
| Configuration and read-only mode | [Configuration](docs/src/guide/configuration.md) |
| Write safeguards and limitations | [Safety and permissions](docs/src/guide/safety.md) |
| Commands and exit codes | [Command line](docs/src/guide/command-line.md) |
| Tools and parameters | [Tool reference](docs/src/tools/index.md) |
| Supported forge versions and features | [Compatibility](docs/src/reference/forges.md) |
| External CI | [Woodpecker](docs/src/ci/woodpecker.md), [Jenkins](docs/src/ci/jenkins.md), [commit statuses](docs/src/ci/statuses.md) |
| Duration predictions | [Estimates](docs/src/tools/estimates.md) |
| Design and benchmark | [Why a server](docs/src/guide/why-a-server.md), [benchmark methods and results](bench/README.md) |
| Future work | [Roadmap](docs/src/reference/roadmap.md) |
| Contributing | [Contributor guide](CONTRIBUTING.md), [architecture](docs/src/development/architecture.md), [testing](docs/src/development/testing.md) |
| Release history | [Changelog](CHANGELOG.md) |
| Vulnerability reporting | [Security](SECURITY.md) |
| Agent-readable documentation | [LLM documentation guide](docs/src/reference/llms.md) |
| License | [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option |

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
