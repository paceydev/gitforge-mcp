# Benchmark: gitforge-mcp against a skills approach

> **Status: full run on Sonnet done.** Results are in [Results](#results).
> The Haiku run is planned for the end of November 2026.

This benchmark compares an MCP server with skills (a SKILL.md plus helper scripts), using the same questions, forge state and model for every approach. Answers are graded mechanically, with correctness, cost and tool usage reported separately.

## What it measures

| Measure | How |
|---|---|
| **Correct answers** | Each task has a known answer, and the agent ends with a machine-readable `RESULT:` line, graded without a model. |
| **Confident wrong answers** | A `RESULT` that names a wrong verdict, counted apart from "no answer". This is the failure that matters most: an agent reporting a red build as green. |
| **Tokens and cost** | From Claude Code's `stream-json` result: input, cache read, cache write, output, and `total_cost_usd`. |
| **Turns and time** | `num_turns` and `duration_ms` from the same result, plus each tool call by name. |
| **Context cost at rest** | What each approach adds to every request before any task: the input tokens of a run that only says "Reply with OK", minus a run with neither approach loaded. |

## Arms

Every arm runs Claude Code headless (`claude -p`) with the same model, prompt
and `--max-budget-usd` cap, in a fresh checkout of the scenario repository.
Each run gets an empty `CLAUDE_CONFIG_DIR`, and `--strict-mcp-config`, so no
settings, memory, hooks, plugins, MCP servers or CLAUDE.md from the machine
running it apply. (Not `--bare`: it lists a `--plugin-dir` skill but never
tells the model about it, which made the first smoke run's skill arm the
`none` arm in disguise.) Bash is
allowed in every arm, as it would be in real use, and every tool call is
logged, so an arm that falls back to `curl` shows up.

| Arm | Loaded | Why |
|---|---|---|
| **mcp** | gitforge-mcp, via `--mcp-config` | The approach under test. |
| **skill** | A hardened skill, via `--plugin-dir`: SKILL.md plus a stdlib Python helper that reads CI by commit (runs as the authority, commit statuses and runner tasks for the job list, skipped is not a pass), reads logs, lists runners and pull requests. | The fair comparison: a skill written by someone who has already hit the traps. |
| **naive** | A skill of SKILL.md alone: the relevant endpoints and `curl` examples. | What a team typically writes in an afternoon. |
| **none** | Nothing but Bash and the remote URL. | The floor, and the at-rest baseline. |

Both skills are published in `bench/arms/` so anyone can read them, improve
them and re-run the comparison. A benchmark whose authors also wrote the
baseline is suspect; the defence is that the baseline is open to challenge.

## Environment

The contract harness (`scripts/contract/`) starts real forges from their
release binaries, each with a real Actions runner. The benchmark adds a
`bench` repository to each, with commits that leave CI in known states:

| Scenario | State | Trap it tests |
|---|---|---|
| `green` | Every job passed. | None: the control. |
| `red` | One job failed, its error line after about 400 lines of output. | Finding the cause without reading the whole log into context. |
| `skipped` | Every job skipped (`if: false`). | Commit statuses say `success` for a skipped job. |
| `queued` | One job passed; another needs a runner label no runner has, so it waits forever with no runner task; a third `needs:` it and is blocked. | The newest finished job looks like the result. |
| `renumbered` | Twelve runs, after the other repositories' runs, so this repository's run ids and run numbers differ; runs 4, 9 and 11 fail. The seeding picks a run number whose run failed while the run with that id passed. | Passing a run number where the API takes an id returns another run. |
| `pr-red` | An open pull request that merges cleanly, with failing CI on its head. | "Mergeable" is not "ready". |
| `pr-conflict` | An open pull request with a conflict and green CI. | The opposite. |

Each scenario is its own public repository (`bench-<scenario>`), so a run
starts from a plain clone whose `HEAD` is the commit in question.

The forges are the ones whose differences matter most:

- **Gitea 1.25**: GitHub-style job states, no cancel.
- **Forgejo 15**: no jobs or logs endpoints at all. The right answer to "why
  did it fail?" is that the API can't say.
- **Forgejo 17**: the full API.

## Tasks

Each task is a question as a user would ask it, plus an instruction to end
with one `RESULT:` line of JSON in a given shape. The shape is the same for
every arm.

| Task | Question | Scenario | Graded on |
|---|---|---|---|
| T1 | Did CI pass for my latest commit? | `green` | `verdict` = pass |
| T2 | Did CI pass for my latest commit? | `red` | `verdict` = fail |
| T3 | Did CI pass for my latest commit? | `skipped` | `verdict` = nothing ran (a pass is wrong) |
| T4 | Did CI pass for my latest commit? | `queued` | `verdict` = not finished (a pass is wrong) |
| T5 | Why did CI fail on my latest commit? | `red` | `error_line` contains the buried error (Forgejo 15: says logs are unavailable) |
| T6 | Did run #N pass? | `renumbered` | `verdict` is run N's, not run id N's |
| T7 | Why hasn't the `gpu` job started? | `queued` | names the missing runner label |
| T8 | Is pull request #1 ready to merge? | `pr-red` | not ready, because CI failed |
| T9 | Is pull request #1 ready to merge? | `pr-conflict` | not ready, because of the conflict |

`bench/tasks.json` holds the prompts, the `RESULT` shapes and the grading
rules. A **false green** (CI said to pass, or a pull request said to be
ready, when it isn't) is counted on its own.

Nine tasks on three forges make 27 task instances.

## Running it

Two tiers, so the cost is measured before it is spent:

1. **Smoke**: Forgejo 17, the `mcp` and `skill` arms, all nine tasks, once
   each, on one model. That is 18 runs. They check that the harness works
   (each arm's approach is loaded and used) and give the real cost per run.
2. **Full**: 27 instances × 4 arms × 5 repetitions × 2 models = 1,080 runs,
   each capped by `--max-budget-usd`, with a total cap in the harness.

Two models, because the claim has a sharper form: rules kept in prose should
suffer more on a smaller model than rules kept in code.

Results are written as JSON lines (one per run, with the model id, Claude Code
version, forge version and date) and summarised by `bench/report.py` into a
table for the README: per arm, the correct-answer rate, the confident-wrong
rate, and median tokens, cost and turns with 95% bootstrap intervals. The raw
results are committed beside the summary.

The benchmark spends model tokens, so it never runs in CI. It needs an
`ANTHROPIC_API_KEY`, the only credential an empty config directory has.

```bash
bench/forges.sh forgejo-17       # forges, runners and scenarios (needs Docker)
```

```bash
bench/run.py --forge forgejo-17 --arms mcp,skill --model sonnet --at-rest
```

The full run, one model at a time, six runs at once:

```bash
bench/forges.sh gitea-1.25,forgejo-15,forgejo-17
```

```bash
bench/run.py --forge gitea-1.25,forgejo-15,forgejo-17 --arms mcp,skill,naive,none --reps 5 --model sonnet --jobs 6 --budget-total 30 --at-rest --out bench/results/full-sonnet.jsonl
```

The tasks only read from the forges, so parallel runs don't disturb each
other. The total budget is checked before each run starts, so runs already
under way can take the spend past it by at most `--jobs` × `--budget-per-run`.

```bash
bench/report.py bench/results/*.jsonl
```

```bash
bench/forges.sh --down
```

## Results

### Full run on Sonnet, 2026-11-08

Sonnet (`claude-sonnet-5-5`), Claude Code 2.1.311; Gitea 1.25.5, Forgejo
15.0.11 and Forgejo 17.0.1; nine tasks, four arms, five repetitions: 540 task
runs, $19.25 in all. Raw results:
[`results/full-sonnet.jsonl`](results/full-sonnet.jsonl).

| Arm | Correct | False green | Median cost per task | Median tokens per task | Median turns |
|---|---|---|---|---|---|
| mcp | 135 of 135 | 0 | $0.026 | 58,271 | 3 |
| skill | 135 of 135 | 0 | $0.028 | 59,064 | 4 |
| naive | 135 of 135 | 0 | $0.042 | 83,450 | 5 |
| none | 105 of 135 | 0 | $0.037 | 96,913 | 5 |

| Task | mcp | skill | naive | none |
|---|---|---|---|---|
| T1 green | 15/15 | 15/15 | 15/15 | 9/15 |
| T2 red | 15/15 | 15/15 | 15/15 | 6/15 |
| T3 skipped | 15/15 | 15/15 | 15/15 | 14/15 |
| T4 queued | 15/15 | 15/15 | 15/15 | 6/15 |
| T5 cause | 15/15 | 15/15 | 15/15 | 15/15 |
| T6 run number | 15/15 | 15/15 | 15/15 | 10/15 |
| T7 stuck job | 15/15 | 15/15 | 15/15 | 15/15 |
| T8 PR, red CI | 15/15 | 15/15 | 15/15 | 15/15 |
| T9 PR, conflict | 15/15 | 15/15 | 15/15 | 15/15 |

| At rest ("Reply with OK.") | Over none |
|---|---|
| mcp | +245 tokens |
| skill | +94 tokens |
| naive | +78 tokens |

Every run used its arm's approach.

What it shows:

- **On Sonnet, any tooling gets these right.** The three tooled arms
  answered all 405 of their runs correctly, and no arm gave a single false
  green in 540 runs. The traps this benchmark sets (skipped jobs, queued jobs,
  run ids, mergeable-but-red) don't fool Sonnet even with a skill that only
  lists endpoints. Whether they fool a smaller model is the Haiku run's
  question.
- **gitforge-mcp is the cheapest and quickest.** Against the hardened skill:
  7% less cost and one turn fewer per task, at about the same tokens. Against
  the naive skill: 38% less cost, 30% fewer tokens and two turns fewer. The
  naive skill gets there by trial and error with `curl`; the hardened skill
  and the server don't have to.
- **Without tooling, the agent couldn't find the credential.** Every one of
  the `none` arm's 30 misses was an honest "couldn't determine: the API
  returns 404 without authentication". Its correct answers came from the
  repository itself (see below) or from tasks it could answer without the
  forge.

What it also showed about the benchmark itself, to fix in its next version:

- **Some answers can be read from the checkout.** Agents in every arm read
  the workflow file: `runs-on: gpu` answers T7, `if: ${{ false }}` answers
  T3, and T5's error code is computed from constants in the script, which
  every arm worked out on Forgejo 15 (saying so: "derived, not quoted").
  Scenarios should take their outcome from something the repository doesn't
  contain.
- **Two grading rules were too narrow** and were corrected after the run,
  then every stored answer was regraded (bench/report.py always regrades
  against the current tasks.json). On Forgejo 15, T5 now also accepts the
  true error, derived from the source, as well as "the log can't be read";
  T7 now also accepts "only runners with other labels". Before the fixes,
  mcp, skill and naive scored 130, 129 and 130 of 135, all on those two
  rules.
- **The `none` arm measures finding a credential** more than reading CI. A
  machine with a git credential helper configured would give it one.

### Smoke tier, 2026-11-08

Sonnet (`claude-sonnet-5-5`), Claude Code 2.1.308, Forgejo 17.0.1, one run
per task per arm. Raw results:
[`results/smoke-forgejo17-sonnet.jsonl`](results/smoke-forgejo17-sonnet.jsonl).
The tier cost $0.57 in all.

| Arm | Correct | False green | Median cost per task | Median tokens per task | Median turns |
|---|---|---|---|---|---|
| mcp | 9 of 9 | 0 | $0.025 | 57,480 | 3 |
| skill | 9 of 9 | 0 | $0.028 | 58,419 | 4 |

| At rest ("Reply with OK.") | Input tokens | Over none |
|---|---|---|
| none | 18,205 | – |
| mcp | 18,459 | +254 |
| skill | 18,311 | +106 |

Every mcp run called gitforge-mcp, and every skill run loaded the skill and
ran its helper.

What this tier shows, and what it doesn't:

- **The harness works**: each arm's approach was visible to the model and
  used on every task, and the grader read every answer.
- **No difference in correctness yet.** With one run per task, on the most
  capable model and the forge with the fullest API, the hardened skill got
  everything right too. That skill already encodes the rules the traps test.
  The comparison that can show a difference is the full run: the naive and
  `none` arms, Haiku, Forgejo 15 and Gitea 1.25, and five repetitions.
- **Small savings for mcp**: about 11% less cost and one turn fewer per
  task. The token totals are close because Claude Code's own system prompt
  (about 18,000 tokens) dominates every request.
- **Context at rest is small for both.** Claude Code defers MCP tool
  definitions until they are searched for, so gitforge-mcp adds 254 tokens at
  rest and each task spends one `ToolSearch` call loading them. A client
  that doesn't defer them would pay the full definitions on every request;
  that isn't measured here.

The first smoke attempt, under `--bare`, was discarded: the skill arm never
saw its skill, and the scenario repositories were public, so the forge could
be read without the token. Both are fixed, and its results are not
published.

## Threats to validity

- **We wrote every arm.** Both skills are published and open to improvement.
  A better skill that closes the gap is a useful result too.
- **The scenarios favour traps.** That is deliberate: a status check is worth
  having because of the cases where it would mislead. T1, T2 and T8 are plain
  questions any approach should get right, and are reported separately.
- **Bash is available everywhere.** An arm may bypass its approach; tool
  calls are logged and reported, not forbidden.
- **Models and Claude Code change.** Every result records both versions and
  the date. Old results are kept, not overwritten.
- **One agent.** The cross-agent claim (Codex and others) needs the same tasks
  run through those agents; the harness keeps the agent command in one place
  so another can be added.
