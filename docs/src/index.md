# gitforge-mcp

An MCP server that lets coding agents work with CI, pull requests and repository settings on Gitea, Forgejo and GitHub, offering only the tools that will work there.

<div class="hero-actions">
<a class="hero-button primary" href="guide/getting-started.html">Get started</a>
<a class="hero-button" href="tools/index.html">The tools</a>
<a class="hero-button" href="guide/safety.html">Safety</a>
</div>

It's one executable with no runtime to install. It binds to the git
repository it is started in, works out which forge that repository's remote
points at and what that forge's version supports, and offers **only the
tools that will work there**. Claude Code, Codex and any other MCP client
can use it.

> **First public version: 0.2.0.** Native binaries are published for macOS, Linux and Windows on x64 and ARM64. See [Getting started](guide/getting-started.md) for installation and verification.

## What it does

- **Answers "did it pass?" correctly.** One verdict from every run and job
  behind a commit. A skipped job isn't a pass, a job no runner has taken
  isn't finished, and a run's id isn't its number. Every scenario that once
  fooled it is a regression test.
- **Knows its forge.** It probes the forge's kind and version and your
  permissions, and hides what won't work. [`forge_context`](tools/forge-context.md)
  says why: *"cancel needs Forgejo 16 or later; this server runs 15.0.11"*.
- **Waits without blocking.** [`ci_wait`](tools/ci-wait.md) runs as an MCP
  task, or as [`gitforge-mcp wait`](guide/command-line.md#gitforge-mcp-wait)
  in the background, and reports where CI has got to as it goes.
- **Says when CI will finish.** [Estimates](tools/estimates.md) from the
  workflow's own history, grouped by the jobs that ran, and backtested.
- **Merges safely.** Merges refuse a moved head, a conflict and unfinished
  CI, unless told otherwise. See [Safety and permissions](guide/safety.md).
- **Changes settings you can review and undo.** [`repo_settings`](tools/repo-settings.md)
  plans a change to merge options, branch protection or webhooks, applies
  exactly what was planned, and reverts it.
- **Reaches CI outside the forge.** [Woodpecker](ci/woodpecker.md) and
  [Jenkins](ci/jenkins.md) through their own APIs, and anything else
  through [commit statuses](ci/statuses.md).
- **Keeps credentials out of sight.** Tokens come from the OS keyring, an
  environment variable, the GitHub CLI or git's credential helper, and are
  never echoed. See [Credentials](guide/credentials.md).
- **Needs little configuration.** Usually none. When it does, one file,
  keyed by host, and [`gitforge-mcp config`](guide/command-line.md#gitforge-mcp-config)
  shows what it's using and why.

## Try it

```bash
claude mcp add --scope user gitforge -- gitforge-mcp
```

Then ask your agent *"did my push pass?"*. The [getting started
guide](guide/getting-started.md) covers installing, other agents, and
tokens.

## For agents

This site is also available as [`llms.txt`](llms.txt) and
[`llms-full.txt`](llms-full.txt). See [For LLMs](reference/llms.md).
