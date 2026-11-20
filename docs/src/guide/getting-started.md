# Getting started

Install gitforge-mcp, connect it to your agent, and check that it can see your forge.

## Install

Use Homebrew on macOS or Linux, or download the archive for your system and architecture from [GitHub Releases](https://github.com/paceydev/gitforge-mcp/releases/latest). Git is required to read the checkout and its remote.

| System | Architecture | Archive target |
|---|---|---|
| macOS | Apple Silicon | `aarch64-apple-darwin.zip` |
| macOS | Intel | `x86_64-apple-darwin.zip` |
| Linux, glibc 2.35+ | x64 | `x86_64-unknown-linux-gnu.tar.gz` |
| Linux, glibc 2.35+ | ARM64 | `aarch64-unknown-linux-gnu.tar.gz` |
| Windows | x64 | `x86_64-pc-windows-msvc.zip` |
| Windows | ARM64 | `aarch64-pc-windows-msvc.zip` |

Each filename starts with `gitforge-mcp-<version>-`. Native CI builds and tests each target. The first release has no Apple or Windows publisher signature. macOS Gatekeeper, Windows SmartScreen and managed-device policies may restrict execution. Signing and native installers are planned.

### Homebrew (macOS and Linux)

```sh
brew install paceydev/tap/gitforge-mcp
```

The [paceydev tap](https://github.com/paceydev/homebrew-tap) installs prebuilt ARM64 or x64 binaries without a Rust compiler. Homebrew verifies the archive's pinned SHA-256 checksum. Linux requires glibc 2.35 or later; Alpine/musl is unsupported. The macOS binaries remain unsigned by an Apple Developer ID.

Upgrade with:

```sh
brew update
brew upgrade paceydev/tap/gitforge-mcp
```

Find the executable's full path for your MCP client:

```sh
echo "$(brew --prefix)/bin/gitforge-mcp"
```

The usual paths are `/opt/homebrew/bin/gitforge-mcp` on Apple Silicon, `/usr/local/bin/gitforge-mcp` on Intel Macs, and `/home/linuxbrew/.linuxbrew/bin/gitforge-mcp` on Linux. Use the path printed above if your Homebrew prefix differs. Use the `bin` path under the Homebrew prefix rather than a versioned Cellar path so client configuration survives upgrades.

If you previously installed with Cargo or copied a binary onto your PATH, check for multiple copies with `type -a gitforge-mcp`. Update any absolute paths in your clients before removing the old installation. `cargo uninstall gitforge-mcp` removes a Cargo-managed installation. Restart connected clients after switching installations or upgrading.

### macOS ZIP

Extract the ZIP for your Mac, then install the executable into a directory on your PATH. For Apple Silicon and version 0.2.0:

```sh
unzip gitforge-mcp-0.2.0-aarch64-apple-darwin.zip
mkdir -p "$HOME/.local/bin"
install -m 755 gitforge-mcp-0.2.0-aarch64-apple-darwin/gitforge-mcp "$HOME/.local/bin/gitforge-mcp"
```

Use the `x86_64-apple-darwin` archive on Intel.

### Linux

The installer selects x64 or ARM64, verifies the archive's SHA-256 checksum and installs into `~/.local/bin`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/paceydev/gitforge-mcp/v0.2.0/install.sh | sh
```

You can download and inspect `install.sh` before running it. Set `GITFORGE_INSTALL_DIR` to choose another directory and `GITFORGE_VERSION` to choose a release. These variables configure the installer. They do not configure the MCP server. A manual install can extract the matching archive and copy its executable to a directory on your PATH.

### Windows

Extract the ZIP with Explorer or PowerShell. For x64 and version 0.2.0:

```powershell
Expand-Archive .\gitforge-mcp-0.2.0-x86_64-pc-windows-msvc.zip -DestinationPath .
.\gitforge-mcp-0.2.0-x86_64-pc-windows-msvc\gitforge-mcp.exe --version
```

Move the extracted directory to a permanent location, then add it to your user PATH or give your MCP client the executable's full path. Use the `aarch64-pc-windows-msvc` ZIP on ARM64.

### Verify a download

Download `SHA256SUMS` from the same release. Compare the archive hash with its line in that file:

```sh
# macOS
shasum -a 256 <archive>
# Linux
sha256sum <archive>
```

```powershell
Get-FileHash .\<archive> -Algorithm SHA256
```

Checksums detect changed bytes. To verify the GitHub repository and workflow that produced the archive, use the GitHub CLI:

```sh
gh attestation verify <archive> --repo paceydev/gitforge-mcp
```

Each release also includes the attestation bundle as `attestation.json`.

### From source

The crates.io package provides 0.2.1 with guided setup. The current GitHub archives and Homebrew tap provide 0.2.0 until their next release. Requires Rust 1.89 or later:

```sh
cargo install gitforge-mcp --locked --version 0.2.1
```

Or, from a clone:

```sh
cargo install --locked --path .
```

Cargo installs into `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows). Ensure the installation directory is on your PATH, then check:

```sh
gitforge-mcp --version
```

## Connect it to your agent

The server works on the repository in the directory it's started in. Agents
start MCP servers in the project you opened, so normally there's nothing to
configure. If yours doesn't, pass `--dir <path>` or set `GITFORGE_MCP_DIR`.

### Guided setup

`gitforge-mcp init` is available in 0.2.1 and later. If you installed the 0.2.0 binary, use the manual instructions below.

```sh
gitforge-mcp init
```

Run it from a checkout to check the forge connection and credentials, then select Claude Code, Codex or both. It shows the executable path and proposed user-level settings and asks before changing each file. Existing settings are kept, and changed files receive a backup. Tokens can be stored in the OS keyring; they are not copied into client settings. Use `gitforge-mcp init --dry-run` for a preview without changes. See the [command reference](command-line.md#gitforge-mcp-init) for options and exit codes.

### Claude Code

For every project, or for one project:

```bash
claude mcp add --scope user gitforge -- gitforge-mcp
```

```bash
claude mcp add --scope project gitforge -- gitforge-mcp
```

`--scope project` writes `.mcp.json` in the repository, so everyone who
clones it gets the server too.

An agent started from a desktop app rather than a terminal may not see
your shell's `PATH`, so it can't find `gitforge-mcp`. If `claude mcp list`
or `forge_context` shows the server failing to start, give the full path
instead. For Homebrew, register the path under its prefix:

```sh
claude mcp add --scope user gitforge -- "$(brew --prefix)/bin/gitforge-mcp"
```

For a Cargo install, use `"$HOME/.cargo/bin/gitforge-mcp"` (on Windows,
`%USERPROFILE%\.cargo\bin\gitforge-mcp.exe`).

### Codex

In `~/.codex/config.toml`:

```toml
[mcp_servers.gitforge]
command = "gitforge-mcp"
```

If Codex cannot find the executable on PATH, use its full path. For example, with Homebrew on an Apple Silicon Mac:

```toml
[mcp_servers.gitforge]
command = "/opt/homebrew/bin/gitforge-mcp"
```

Use the path from `echo "$(brew --prefix)/bin/gitforge-mcp"` for other Homebrew prefixes. Restart Codex after changing the server configuration.

### Other MCP clients

Any client that can start a stdio server:

```json
{
  "mcpServers": {
    "gitforge": { "command": "gitforge-mcp" }
  }
}
```


## Give it a token

If you can already `git push` to the forge over HTTPS, the server can often
use the same credential and there's nothing to do. Otherwise store a token
in the OS keyring:

```bash
gitforge-mcp auth set git.example.com
```

[Credentials](credentials.md) lists every source the server tries, and the
scopes a token needs.

## Check it works

From inside the repository, see what the server would use:

```bash
gitforge-mcp config
```

Then ask the forge:

```bash
gitforge-mcp context
```

It prints what `forge_context` reports: your repository, your forge and its
version, who you're signed in as, and each capability, with the reason when
one isn't available. It exits non-zero unless the forge answered (or a
cached answer stands in for it).

In your agent, ask it to *"call forge_context"*. It should say the same.
Then ask it something real: *"did my last push pass?"*

## Next

- [Configuration](configuration.md), for hosts the server can't detect on its own.
- [Safety and permissions](safety.md), for what the server will and won't change.
- [The tools](../tools/index.md), for what an agent can ask.
