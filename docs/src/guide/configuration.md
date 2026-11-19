# Configuration

Most setups need none; a config file covers hosts the server can't work out on its own, and a few options cover the process.

The server learns almost everything from the repository it's started in: the
forge from the git remote, the forge's kind and version by asking it, and
your credential from the places in [Credentials](credentials.md). Settings
live in one file, keyed by host, rather than in environment variables, so one
file covers every forge and CI system you use.

To see what the server would use, and where each value came from, run
[`gitforge-mcp config`](command-line.md#gitforge-mcp-config) inside the
repository.

## The config file

`~/.config/gitforge-mcp/config.toml` (or under `$XDG_CONFIG_HOME`). On
Windows it's `%APPDATA%\gitforge-mcp\config.toml`. `--config <file>` or
`GITFORGE_MCP_CONFIG` points at another. A missing file is the same as an
empty one. A key the server doesn't know is an error, so a typo is never
silently ignored: the server then ignores the whole file and says so in
`forge_context`, and `gitforge-mcp config` exits 1.

### Forge hosts

```toml
[hosts."git.example.com"]
kind = "gitea"
base_url = "https://git.example.com"
token_env = "MY_FORGE_TOKEN"
credentials = ["env", "keyring", "git-credential"]
```

| Key | Meaning | Default |
|---|---|---|
| `kind` | `gitea`, `forgejo` or `github`. Skips detection. Required for GitHub Enterprise Server, which can't be told apart by asking. | asked when the server starts |
| `base_url` | The forge's web address, when the git remote doesn't give it: an SSH remote whose web server isn't `https://<host>`, or one on another port. | from the remote |
| `token_env` | Read the token from this variable instead. | `GITFORGE_TOKEN_<HOST>` |
| `credentials` | Which sources to try, in order: `env`, `keyring`, `gh` (GitHub hosts only), `git-credential`. | `env`, `keyring`, `git-credential`; GitHub hosts add `gh` before `git-credential` |

### CI hosts

A CI system outside the forge, such as [Woodpecker](../ci/woodpecker.md) or
[Jenkins](../ci/jenkins.md). Only needed when the repository's commit
statuses don't link to it, or to say where its token is.

```toml
[ci."ci.example.com"]
kind = "woodpecker"
base_url = "https://ci.example.com"
token_env = "MY_WOODPECKER_TOKEN"
credentials = ["env", "keyring"]

[ci."jenkins.example.com"]
kind = "jenkins"
username = "dev"
```

| Key | Meaning | Default |
|---|---|---|
| `kind` | `woodpecker` or `jenkins`. | asked, from the links in commit statuses |
| `base_url` | Its web address, when its status links don't give it, or it isn't `https://<host>/`. | from its links |
| `token_env` | Read the token from this variable instead. | `GITFORGE_TOKEN_<HOST>` |
| `username` | Jenkins only: the user the API token belongs to, when the token isn't stored as `user:token`. | none |
| `credentials` | Which sources to try, in order. | `env`, `keyring`: git's credential helper holds forge passwords, not CI tokens |

## Options

These apply to one run of the server, so they're command-line options. Each
has an environment variable too, for clients that can set a server's
environment more easily than its arguments.

| Option | Variable | Default |
|---|---|---|
| `--dir <path>` | `GITFORGE_MCP_DIR` | the current directory |
| `--remote <name>` | `GITFORGE_MCP_REMOTE` | `origin`, or the only remote |
| `--config <file>` | `GITFORGE_MCP_CONFIG` | see above |
| `--read-only` | `GITFORGE_MCP_READ_ONLY` | off: writes are offered where the token allows them |
| | `GITFORGE_MCP_LOG` | `warn`. Logs go to stderr; stdout carries MCP. |
| | `GITFORGE_MCP_CACHE` | the probe cache file, `probes.json` in the user cache directory. Run history is kept in `history/` beside it. |

A command-line option wins over its variable.

## Read-only mode

Start the server with `--read-only` (or `GITFORGE_MCP_READ_ONLY=true`) and it
offers nothing that changes the forge, whatever the token allows:
`ci_control` disappears, and `pull_request` and `repo_settings` offer only
their read actions.
See [Safety and permissions](safety.md).
