# The command line

Everything the server does also works as a command, for checking a setup, for scripts, and for waiting on CI in the background.

Every command takes the options in [Configuration](configuration.md#options):
`--dir`, `--remote`, `--config` and `--read-only`, before or after the
command's name.

## `gitforge-mcp`

With no command (or `serve`), it serves MCP over stdio. This is what agents
run. stdout carries the protocol, and logs go to stderr.

## `gitforge-mcp init`

Available in 0.2.1 and later. Version 0.2.0 uses the manual client setup in [Getting started](getting-started.md).

Guides setup for Claude Code, Codex, both, or a connection check only:

```sh
gitforge-mcp init
```

It detects the checkout and forge, reports the credential source without printing the credential, and checks repository access with a fresh, read-only request. It does not use or update cached probes. If no credential is found, it explains API authentication and can offer to store a hidden-input token in the OS keyring before retesting. SSH Git access alone does not authenticate API requests.

The client menu writes user-level `gitforge` entries in Claude Code's `~/.claude.json` and Codex's `~/.codex/config.toml`. `CLAUDE_CONFIG_DIR` and `CODEX_HOME` override these locations. The executable path and arguments are shown before a separate confirmation for each file; the default answer is no. Tokens are never placed in client configuration. Existing server environment, enablement, other servers and unrelated settings are preserved. Existing command/arguments are replaced by the preview; an entry using another transport must be handled manually. Codex comments are retained; Claude JSON formatting may change.

Existing changed files get a private backup beside them before atomic replacement. The backup path is printed. A configuration file edited after the preview is refused; rerun setup to review the new state. Stop clients while changing configuration to avoid concurrent edits. Unchanged entries are not rewritten.

Setup uses the stable invoked executable path when possible, including Homebrew's prefix symlink. It does not pin `--dir` into user-level configuration: each opened checkout supplies the repository. Explicit `--config`, `--remote` and `--read-only` settings are included in the proposed arguments. Desktop clients may have different credentials or environment variables; restart the client and ask it to call `forge_context` to verify its own session.

| Option | Meaning |
|---|---|
| `--client claude` | Configure Claude Code without the client menu. |
| `--client codex` | Configure Codex without the client menu. |
| `--client both` | Configure both, confirming each file separately. |
| `--client none` | Check the connection without client configuration. |
| `--dry-run` | Preview without changing client files or keyring; defaults to both clients. |

```sh
gitforge-mcp init --dry-run --client both
```

Without `--dry-run`, setup requires an interactive terminal and refuses redirected input. Exit 0 means fresh repository access was verified, including anonymous access to public repositories; it does not claim writes are permitted or that every selected client was changed. Unbound, unreachable, timed-out or failed checks exit 1. A declined configuration is explicitly reported as skipped.

## `gitforge-mcp config`

Shows every setting and where it came from: an option, an environment
variable, the config file, the git remote or a default. Run it inside the
repository:

```text
Settings
  config       /home/dev/.config/gitforge-mcp/config.toml  (default)
  dir          .  (default)
  remote       origin  (default)
  read-only    off  (default)
  log          warn  (default)
  cache        /home/dev/.cache/gitforge-mcp/probes.json  (default)
  history      /home/dev/.cache/gitforge-mcp/history  (default)

Repository
  acme/widgets on git.example.com, through remote origin (https://git.example.com/acme/widgets.git)
  checkout     /home/dev/src/widgets

Forge git.example.com (the repository's)
  kind         forgejo  (config file)
  web address  https://git.example.com/  (git remote)
  token from   GITFORGE_TOKEN_GIT_EXAMPLE_COM (not set)  (default)
  credentials  env, keyring, git-credential  (default)

CI ci.example.com
  kind         woodpecker  (config file)
  web address  https://<host>/, or what its links show  (default)
  token from   GITFORGE_TOKEN_CI_EXAMPLE_COM (set)  (default)
  credentials  env, keyring  (default)
```

It reads the config file and the git remote and nothing else: no
credential, and no request to the forge. For a token variable it says
whether it is set, never what it holds. `--json` prints the same as JSON.

It exits 1 if the config file isn't valid (the server would ignore it) or
there's no repository to bind to, and lists the problem under `Problems`.

## `gitforge-mcp context`

Prints what [`forge_context`](../tools/forge-context.md) reports, as JSON:

```bash
gitforge-mcp context
```

It exits non-zero unless the status is `live` or `cached`.

## `gitforge-mcp call`

Runs any offered tool with JSON arguments and prints its result:

```bash
gitforge-mcp call ci_status '{"branch": "main"}'
```

```bash
gitforge-mcp call ci_logs '{"run": 42, "grep": "error"}'
```

The MCP tools work on the repository the agent was started in. To act on
another checkout from the same session, run the command with `--dir`:

```bash
gitforge-mcp --dir ../other-repo call pull_request '{"action": "get", "number": 9}'
```

## `gitforge-mcp wait`

Does what [`ci_wait`](../tools/ci-wait.md) does, for running in the
background:

```bash
gitforge-mcp wait --branch main
```

Choose the commit with one of `--sha`, `--branch` or `--run` (default: the
checkout's `HEAD`), and give up after `--timeout` seconds (default and
maximum 1800). Add `--no-fail-fast` to keep waiting after a job fails, and
`--jobs all` to list every job in the result. Each time CI moves on it
prints a progress line to stderr, such as:

```text
[  93s] 15 of 50 jobs finished (11 success, 4 skipped); running: unit tests (step 3 of 4: Run unit tests); waiting: deploy and 3 more
```

When it ends it prints the `ci_wait` result as JSON on stdout.

## `gitforge-mcp auth`

Manages the token kept in the OS keyring. See [Credentials](credentials.md).

| Command | What it does |
|---|---|
| `auth set <host>` | Stores a token for the host. Prompts without echoing, or reads it from stdin. |
| `auth remove <host>` | Removes the stored token. |
| `auth status` | Shows each credential source the repository's host would try, and which one answers. |

## Exit codes

| Code | `wait` | Every other command |
|---|---|---|
| 0 | CI passed | it worked |
| 1 | CI failed, or nothing ran | it didn't: the message says why |
| 2 | CI hadn't finished in time, or no run appeared | |
| 3 | it couldn't check | |
| 64 | the command line was wrong, such as `--sha` with `--branch` | the command line was wrong |

A bad command line exits 64 rather than clap's usual 2, so a background
reader of `wait` never mistakes it for "not finished" and runs it again.
`--help` and `--version` exit 0.
