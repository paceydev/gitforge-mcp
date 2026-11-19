# Roadmap

Current capabilities, release work and known limitations.

The first public version supports CI, pull requests and repository settings on Gitea, Forgejo and GitHub, with Woodpecker and Jenkins integrations. See the [compatibility table](forges.md) for action and version limits.

Upcoming work includes an interactive `gitforge-mcp init`, Homebrew installation, Apple signing and notarization, and Windows signing and installers.

Duration forecasts use forge-native history. Woodpecker and Jenkins forecasts remain future work. GitHub settings use classic branch protection; repository rulesets are outside the current settings API. Live GitHub contract tests are opt-in and require a disposable repository; CI uses fake GitHub responses.
