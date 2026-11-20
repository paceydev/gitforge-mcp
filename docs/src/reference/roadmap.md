# Roadmap

Current capabilities, release work and known limitations.

The first public version supports CI, pull requests and repository settings on Gitea, Forgejo and GitHub, with Woodpecker and Jenkins integrations. See the [compatibility table](forges.md) for action and version limits.

Homebrew installation is available through the [paceydev tap](https://github.com/paceydev/homebrew-tap) for macOS and Linux. See [getting started](../guide/getting-started.md).

Guided `gitforge-mcp init` setup for Claude Code and Codex is available in the 0.2.1 source package. GitHub binary downloads and Homebrew currently provide 0.2.0, which uses manual client setup; their 0.2.1 update is pending.

Upcoming work includes Apple signing and notarization, and Windows signing and installers.

Duration forecasts use forge-native history. Woodpecker and Jenkins forecasts remain future work. GitHub settings use classic branch protection; repository rulesets are outside the current settings API. Live GitHub contract tests are opt-in and require a disposable repository; CI uses fake GitHub responses.
