# Notes for coding agents

gitforge-mcp is a Rust MCP server (rmcp 3.x, stdio) for CI, pull requests and
repository settings on Gitea, Forgejo and GitHub. Read CONTRIBUTING.md first.

## Commands

- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` before every commit.
- `scripts/contract/run-local.sh` after changing a driver or `src/capability.rs`.
- `scripts/check-clean.sh` before committing anything copied from a real forge.
- `scripts/docs/build.sh` after changing `docs/` (needs `mdbook`); `cargo test --test docs` checks the pages against the code without it.

## Layout

- `src/remote.rs` binds to a checkout's git remote.
- `src/auth.rs` finds credentials: env, keyring, git credential helper.
- `src/forge/` has the drivers: `gitea.rs` (Gitea and Forgejo), `github.rs`, and the `Forge` trait in `mod.rs`.
- `src/forge/gitea/ci.rs` reads CI runs, jobs, logs, runners and pull requests from Gitea and Forgejo, whose Actions APIs differ.
- `src/ci.rs` holds the CI model and verdict rules; `src/logs.rs`, `src/runners.rs` and `src/pulls.rs` the rest of the model.
- `src/forge/gitea/write.rs` holds the writes: pull requests, cancel, rerun, dispatch.
- `src/external/` reaches CI outside the forge through its own API: `woodpecker.rs` and `jenkins.rs`. A provider is keyed by the CI host, beside the `Forge` trait, and adds capability rows with `provider` set.
- `src/settings/` is the repository settings model: each section's fields under names every forge shares (`fields.rs`), and how a change becomes operations and the operations that undo it (`plan.rs`). Each driver's `settings.rs` translates those to its forge's API; `src/repo_settings.rs` is the `repo_settings` tool. A webhook secret must never reach a reply.
- `src/tools.rs` is what each tool does; `src/server.rs` routes and gates, including the `ACTIONS` table that gates each tool's actions one by one.
- `src/capability.rs` is the feature table by forge kind, version and permission.
- `src/context.rs` runs the probe that ties these together, and `src/cache.rs` keeps the last successful one.
- `src/server.rs` is the MCP server, with the `GATES` table that decides which tools are offered.
- `src/config_report/` is `gitforge-mcp config`: every setting and where it came from (`render.rs` the text, `hosts.rs` each host).
- `docs/src/` is the documentation site (mdBook). A new tool, parameter, action, command or environment variable needs its page updated: `tests/docs.rs` fails until it is.

## Rules

- Never add a real host, organisation, user or repository name, or a token, to any file. Use example.com, acme, widgets.
- MSRV is `rust-version` in Cargo.toml. Don't use newer language or library features.
- stdout is the MCP channel. Log only through `tracing`, which writes to stderr.
- Version thresholds in `src/capability.rs` must be checked against the forge's swagger spec; the contract tests do this for the matrix in `scripts/contract/forges.txt`.
- CI verdicts must stay correct for every scenario in `tests/fixtures/forgejo15`. Each is a real API state that once produced a confident wrong answer.
- Every write is gated on a capability, refused in read-only mode, and covered by `tests/contract_writes.rs` on each forge version in the matrix. Never retry a write once its request was sent.
