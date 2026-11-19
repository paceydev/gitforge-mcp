# Compared with other forge MCP servers

The scope and tradeoffs of gitforge-mcp when choosing a forge integration.

This server focuses on CI verdicts and diagnostics, runner capacity, pull requests and repository settings across Gitea, Forgejo and GitHub. It binds to the current checkout and filters tool actions using forge capabilities and repository permissions. Woodpecker and Jenkins extend CI support outside the forge.

For a broader GitHub integration, consult the [official GitHub MCP server](https://github.com/github/github-mcp-server). Check each project's current documentation against the actions and deployment model you need.

The [skills benchmark](../guide/why-a-server.md) compares this server with two skills on the same tasks. It does not compare the implementations of other MCP servers.
