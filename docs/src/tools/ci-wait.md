# ci_wait

Waits for CI on a commit to finish, then answers as `ci_status` does.

## Parameters

| Parameter | Meaning |
|---|---|
| `sha`, `branch`, `run` | Which commit or run, as for [`ci_status`](ci-status.md). Default: the checkout's `HEAD`. |
| `timeout_seconds` | Give up after this many seconds and report where things stand. Default 300 (1800 as a task), at most 1800. |
| `fail_fast` | Return as soon as a job fails. Default true. |
| `jobs` | Which jobs to list: `attention` (default), `all` or `none`. |

Waits for CI on a commit to finish, then answers like `ci_status`. It checks
the forge every few seconds, sends progress as it goes, and returns as soon as
every job has finished, or as soon as one fails (`fail_fast`, on by default),
or after `timeout_seconds` (default 300, at most 1800) with where things
stand. If no run appears within 90 seconds, it says so instead of waiting out
the whole timeout. Use it instead of asking for `ci_status` in a loop.

An MCP tool call holds up the agent's conversation until it returns, so a
long wait blocks the agent. There are two ways round that:

- **A client that supports MCP tasks** (Claude Code's terminal CLI, from
  v2.1.304) gets `ci_wait` as a task: the call returns at once, the client
  polls it, and the agent carries on until CI finishes. The task's status
  message is the `progress` line. As a task, `timeout_seconds` defaults to
  1800.
- **An agent that can run shell commands in the background** (Claude Code's
  desktop app, Codex) can run [`gitforge-mcp wait`](../guide/command-line.md#gitforge-mcp-wait)
  instead. The server's instructions tell the agent so, with the binary's
  full path.

The generated background command uses PowerShell on Windows and POSIX shell quoting on macOS and Linux.
