# Why a server and not a skill

What an MCP server buys over a skill of markdown and scripts, and what the benchmark found.

The usual way to give an agent forge access is a *skill*: a markdown file of
instructions plus a few helper scripts that call the forge's REST API. That
works, and it's where this project started. It has costs that a server
avoids:

| | Skill (SKILL.md + scripts) | gitforge-mcp |
|---|---|---|
| **Which agents** | Client skill formats and helper runtimes vary; sharing may require adaptation. | Any MCP client: Claude Code, Codex, Cursor, and others. One configuration line each. |
| **Runtime** | Needs Python and/or bash on the machine. Awkward on Windows. | One self-contained binary for macOS, Linux and Windows. |
| **Correctness** | The rules for reading the API correctly live in prose, or in a helper someone has to write and maintain per agent. For example, a skipped job isn't a pass, and a run's id isn't its number. | The same rules live in code, with regression tests against real API responses. (On Sonnet the benchmark found no correctness difference between this server and either skill; see below.) |
| **Different forge versions** | Typically assumes one version. Calling an endpoint that doesn't exist gives a 404, which the agent then has to interpret. | Probes the version and hides tools that won't work. `forge_context` says why ("cancel needs Forgejo 16 or later; this server runs 15.0.11"). |
| **Credentials** | Credential lookup and exposure depend on the helper and commands used. | Resolved inside the server from an environment variable, the OS keyring or git's credential helper. Never echoed, never on a command line. |
| **Output** | Text or JSON, depending on the helper. | Structured JSON results. |
| **Context cost** | A line per skill until one triggers, then the whole SKILL.md (often 9–11 KB) plus script output. | Tool definitions are kept small. Claude Code defers them until they're needed: 254 tokens at rest in the smoke benchmark, against 106 for a skill. Clients that don't defer them pay the full definitions on every request. |
| **Changes to the repository** | Whatever the script does. | `repo_settings`: read → plan, showing each change → apply, refused if anything moved since the plan → revert. |
| **Editing it** | Edit a markdown file. No build. | Rust. A change needs a release. |

Skills still have a place. Team policy, such as *which branch to merge into*
or *never push to main without asking*, is a good fit for a short skill on
top of this server, which handles the mechanics.

The benchmark (`bench/README.md` in the repository) measures the comparison: nine CI and pull
request questions, asked through this server, a hardened skill, a naive
skill and no tools at all, against real Gitea 1.25, Forgejo 15 and Forgejo 17
instances, five times each, graded mechanically. On Sonnet:

| | Correct | Wrong "all good" answers | Cost per task | Tokens per task | Turns |
|---|---|---|---|---|---|
| gitforge-mcp | 100% | 0 | $0.026 | 58k | 3 |
| hardened skill | 100% | 0 | $0.028 | 59k | 4 |
| naive skill | 100% | 0 | $0.042 | 83k | 5 |
| no tools | 78% | 0 | $0.037 | 97k | 5 |

So on a capable model the difference is efficiency, not correctness: this
server answers in fewer turns and for less than either skill (38% less than
the naive one), and a hardened skill comes close. Whether prose rules cost
correctness on a smaller model is what the Haiku run (planned for the end of
November 2026) will show.
