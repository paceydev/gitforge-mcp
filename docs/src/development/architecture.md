# Architecture

How a request travels through the server, which module owns each step, and the rules that keep them apart.

## From start to a tool call

```text
 agent ──stdio──▶ server.rs ─────────────────────────────────────────────┐
                  GATES, ACTIONS: which tools and actions are offered     │
                       │ first tools/list, or forge_context refresh       │
                       ▼                                                  │
                  context.rs: the probe                                   │
                   1. config.rs     read config.toml                      │
                   2. remote.rs     bind to the checkout's git remote     │
                   3. auth.rs       find a credential (env, keyring,      │
                                    gh, git credential helper)            │
                   4. forge/        connect: detect kind and version,     │
                                    who you are, repository permissions   │
                   5. capability.rs the feature table, by kind, version   │
                                    and permission                        │
                   6. external/     CI systems the commit statuses link to│
                       │                                                  │
                       ├──▶ cache.rs: the last good probe, for when the   │
                       │    forge is down                                 │
                       ▼                                                  │
                  Session ──────────▶ tools.rs: what each tool does ◀─────┘
                                         │
                       ┌─────────────────┼──────────────────┐
                       ▼                 ▼                  ▼
                  forge/ drivers     external/          models: ci.rs,
                  gitea.rs, github.rs woodpecker.rs,    logs.rs, runners.rs,
                  (reads and write.rs) jenkins.rs       pulls.rs, estimate/
```

1. The agent starts the server in a repository. `server.rs` answers the MCP
   handshake at once. The first `tools/list` runs the probe and waits up to
   15 seconds for it; if the forge hasn't answered by then, it lists the
   always-on tools alone. The list carries a five-minute lifetime, and the
   server tells the client when it changes, so a forge that comes back up
   shows its tools without a restart.
2. The probe (`context.rs`) reads the config file, binds to the git
   remote, finds a credential, asks the forge what it is and what you may
   do, and builds the capability table. A forge that doesn't answer falls
   back to the last good probe in `cache.rs`, so its tools stay listed and
   fail with a clear "unreachable" instead of vanishing.
3. `server.rs` offers the tools the `GATES` table allows for those
   capabilities, with each tool's `action` narrowed to what the `ACTIONS`
   table allows, and refuses a call that names anything else.
4. `tools.rs` does the work against the `Session`: a connected forge driver,
   the repository, your permissions, and any CI providers outside the
   forge.
5. The drivers speak each forge's API and return forge-neutral models
   (`ci.rs`, `pulls.rs`, `runners.rs`, `logs.rs`). Verdicts are decided in
   those models, once, for every forge.

## Modules

| Module | Owns |
|---|---|
| `main.rs` | The command line: serve, `config`, `context`, `call`, `wait`, `auth`. |
| `server.rs` | The MCP server: routing, the `GATES` and `ACTIONS` tables, tool schemas and annotations, `ci_wait` as an MCP task. |
| `tools.rs` | What each tool does, in terms of a `Session`. |
| `context.rs` | The probe that ties binding, credentials, the forge and capabilities together. |
| `capability.rs` | The feature table by forge kind, version and permission, with the reason for each missing feature. |
| `remote.rs` | Binding to a checkout's git remote. |
| `auth.rs`, `secret.rs` | Finding credentials, and a type that never prints them. |
| `config.rs`, `config_report/` | The config file, and the report `gitforge-mcp config` prints. |
| `repo_settings.rs`, `settings/` | Settings requests, shared fields, plans and the operations that undo changes. Each forge driver translates them to its API. |
| `cache.rs`, `history.rs` | The last good probe, and run history for estimates, on disk. |
| `forge/` | The `Forge` trait and its drivers: `gitea.rs` (Gitea and Forgejo), `github.rs`, the shared HTTP client and response shapes. |
| `external/` | CI outside the forge through its own API: `woodpecker.rs`, `jenkins.rs`. |
| `ci.rs`, `logs.rs`, `runners.rs`, `pulls.rs` | The models every driver returns, and the verdict rules. |
| `estimate/` | Durations by job set, queue waits, `ci_estimate`'s outlook, and backtesting. |

## Rules

- **Gating fails closed.** A tool missing from `GATES` is never offered.
  A new capability is a feature in `capability.rs`, mapped in `GATES` or
  `ACTIONS`.
- **Capabilities come from versions, not errors.** What a forge supports is
  decided from its kind and version, checked against its published API
  description by the contract tests, never by calling an endpoint and
  reading the 404.
- **Differences are keyed, not guessed.** Where Gitea and Forgejo differ,
  the driver branches on kind and version.
- **Writes are sent once.** Never retried after the request has gone.
- **stdout is the MCP channel.** Logs go through `tracing`, to stderr.
- **Credentials stay in `Secret`.** It won't format itself, so a token
  can't reach a log, an error or a reply.
- **A small toolset.** Every tool definition costs every user context on
  every request, so a new capability is usually an action on an existing
  tool.

## Where to start reading

- A tool's behaviour: its function in `tools.rs`, then the model it returns.
- Why a tool isn't offered: `GATES` in `server.rs`, then the feature in
  `capability.rs`.
- A forge's quirk: the module comment at the top of each driver file says
  how that forge differs.
