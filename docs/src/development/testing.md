# Testing

The test layers, from unit tests to real forges, and what each one guards.

```bash
cargo test                         # everything below except the real forges
scripts/contract/run-local.sh      # contract tests against real forges (needs Docker)
cargo test --test contract_github  # against github.com: opt-in, see the file for its variables
scripts/check-clean.sh             # nothing private or credential-shaped in the tree
scripts/docs/build.sh              # this site, with llms.txt (needs mdbook)
```

| Layer | Where | Guards |
|---|---|---|
| Unit tests | `src/` | Verdict rules, capability thresholds, parsing, estimates. |
| Stdio and handshake | `tests/stdio.rs`, `tests/handshake.rs` | The real binary speaks MCP, on the raw wire too. |
| Fake forges | `tests/fake_forge.rs`, `fake_github.rs`, `fake_woodpecker.rs`, `fake_jenkins.rs`, `fake_estimates.rs` | Each tool against mocked API responses, including refusals and errors. |
| Recorded API replay | `tests/replay_forgejo15.rs`, `tests/fixtures/forgejo15` | Real API states that once produced a confident wrong answer. Verdicts must stay right for every one. |
| Documentation | `tests/docs.rs` | Every tool, action, parameter and command is documented here, every page is in the table of contents, and every link resolves. |
| Contract tests | `tests/contract*.rs` | What every tool reports against real Gitea, Forgejo, Woodpecker and Jenkins builds. |

## Contract tests

The contract tests run each forge release in `scripts/contract/forges.txt`
from its published binary, with checksums pinned, each with an Actions
runner (`scripts/contract/runners.txt`) that runs a workflow with a passing,
a failing, a skipped and a dependent job. For every forge they check:

1. what `forge_context` reports through the real binary: kind, version,
   user, and permissions for an admin and a read-only collaborator;
2. that the capability table is true: each version-gated feature is
   available exactly when the forge's own API description has the endpoint
   for it;
3. what the CI and pull request tools report about the real workflow run
   and an open pull request.

`tests/contract_writes.rs` then merges, cancels and reruns, on every forge
version in the matrix, after the read-only checks. Woodpecker 3.20 and
Jenkins 2.580 LTS run beside Forgejo 17.

They're skipped unless `GITFORGE_CONTRACT_TARGETS` names a targets file;
`GITFORGE_CONTRACT_REQUIRED=1` turns the skip into a failure, as CI does.


## GitHub CI and releases

Pull requests and pushes to `main` and `develop` run formatting, Clippy, secret and dependency scans, the quality baseline, documentation builds, and the real forge contract matrix. Native jobs test and package both x64 and ARM64 executables on macOS, Linux and Windows. `gate` passes only when every required job succeeds.

A `v` tag matching Cargo.toml runs the same checks again. The release workflow accepts only a commit on `main`, requires all six archives, and publishes their checksums and GitHub build attestations with the release. Platform publisher signing is planned.
