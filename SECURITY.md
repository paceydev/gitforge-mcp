# Security

## Reporting a vulnerability

Report a vulnerability through [GitHub private vulnerability reporting](https://github.com/paceydev/gitforge-mcp/security/advisories/new). Include what you found, how to reproduce it, and which version you used. We aim to acknowledge reports within a week.

## How gitforge-mcp handles credentials

- Tokens come from an environment variable, the OS keyring, the GitHub CLI or git's
  credential helper, at the moment they're needed. The server never writes
  them to disk. The probe cache (`probes.json` in the user cache directory)
  holds forge kind, version, login and permissions only.
- Credentials are held in a type that won't format itself, so they can't
  reach logs, errors or tool output. The tests check this.
- git's credential helper is asked non-interactively. No prompts, and no
  askpass or credential-manager windows.
- Use a token with the narrowest scopes that cover what you need (see
  [Credentials](docs/src/guide/credentials.md)): `read:repository` covers basic repository reads on Gitea and Forgejo. Give it
  `write:repository` if agents should open or merge pull requests, control CI, or change repository settings. Runner scopes and GitHub permissions are covered in the credential guide, or run the server with `--read-only` to rule writes
  out whatever the token allows.

[Safety and permissions](docs/src/guide/safety.md) covers the rest: what
the server will change, and what it refuses.
