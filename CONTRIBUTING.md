# Contributing

How to work on gitforge-mcp: what keeps it small and safe, the tests to run, and what a pull request needs. Thanks for helping.

## Before you start

- **Small toolset.** Every tool definition costs every user context on every
  request. A new capability should usually be a new `action` on an existing
  tool, not a new tool.
- **Gated tools.** A tool is offered only when the forge, its version and the
  caller's permissions support it. Add a feature to `src/capability.rs`, and
  map the tool to it in the `GATES` table in `src/server.rs`. A tool missing
  from that table is never offered.
- **No guessing from errors.** Decide what a forge supports from its kind and
  version, not by calling an endpoint and reading the 404.
- **Documented.** A new tool, parameter, action, command or environment
  variable goes on its page in `docs/src/`. `cargo test --test docs` fails
  until it does, and `scripts/docs/build.sh` builds the site to look at.
- **No secrets anywhere.** Credentials go through `Secret`, which won't print
  itself. Tests assert tokens never appear in tool output.

## Running the tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The contract tests need real forges. With Docker (or OrbStack, Colima or
Podman) available:

```bash
scripts/contract/run-local.sh          # start forges, run tests/contract.rs
scripts/contract/run-local.sh --down   # remove the container afterwards
```

To test against another forge release, add a line to
`scripts/contract/forges.txt` with the checksums from the release's
`.sha256` files.

The minimum supported Rust version is in `Cargo.toml` (`rust-version`), and
CI builds with exactly that version.

Documentation-only pushes and pull requests run the documentation build and credential scans. Native builds, Rust quality checks and forge contracts run when any other tracked file changes. Release tags and manual CI runs always run the full suite. The required `gate` check verifies the expected jobs for each route.

## Fixtures

Recorded API responses used as test fixtures must not contain real host
names, organisation or user names, repository names or tokens. Replace them
with `example.com`, `acme`, `widgets` and so on. `scripts/check-clean.sh`
catches credential-shaped strings.

## Commits and pull requests

Keep changes focused, and add a line under `[Unreleased]` in `CHANGELOG.md`
for anything a user would notice.


Feature pull requests target `develop`. Release pull requests promote `develop` to `main`. Keep feature changes focused so the maintainer can squash each accepted pull request into one commit. Explain the problem, the resulting behavior, and how you verified the change. Include contract results for adapter changes and update the corresponding documentation.

AI assistance is welcome. Disclose material assistance in your pull request and describe the review and verification you performed. The contributor submitting the change is responsible for understanding it and checking its behavior, security and licensing. This project was developed with substantial assistance from Claude Code and Codex.
