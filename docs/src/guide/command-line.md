# The command line

Everything the server does also works as a command, for checking a setup, for scripts, and for waiting on CI in the background.

Every command takes the options in [Configuration](configuration.md#options):
`--dir`, `--remote`, `--config` and `--read-only`, before or after the
command's name.

## `gitforge-mcp`

With no command (or `serve`), it serves MCP over stdio. This is what agents
run. stdout carries the protocol, and logs go to stderr.

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
