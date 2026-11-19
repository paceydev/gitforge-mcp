# Woodpecker

Runs, logs, cancel, rerun, dispatch and agents for Woodpecker 3.0 and later, through Woodpecker's own API.

Woodpecker's statuses link to its pipelines, and gitforge follows them to
Woodpecker's own API (3.0 and later). There, a run is the pipeline, with its
number; each workflow is a job, with its steps and timings. Then:

- `ci_logs` reads a workflow's failed steps' logs (or every step's when
  none failed), each under a `--- step <name> ---` header.
- `ci_control` can `cancel` a pipeline, `rerun` it (Woodpecker restarts a
  whole pipeline as a new one, with the next number; it has no rerun of
  failed workflows), and `dispatch` a manual pipeline on a branch, with
  `inputs` as its variables and, from Woodpecker 3.20, only one `workflow`.
  Where the forge can dispatch Actions workflows too, say which with
  `ci="woodpecker"` or `ci="forge"`.
- `ci_runners` lists Woodpecker's agents as idle, busy or offline, and the
  tasks waiting for one, under `ci_systems`. Woodpecker shows these to its
  admins only.
- Where the forge runs no CI of its own, `run` numbers are Woodpecker's
  pipeline numbers.

When the server starts, it looks at the statuses on the default branch's
latest commit, and asks each Woodpecker they link to for its version
(anonymously), then for the repository with the token you gave it for that
host. A token goes to the Woodpecker host it was given for, never to the
forge or anywhere else. Reads of a public repository need no token; writes
need one with push access. Create one under **User settings → CLI & API**
in Woodpecker, and store it like a forge token, under the Woodpecker host's
name:

```bash
gitforge-mcp auth set ci.example.com
```

or set `GITFORGE_TOKEN_CI_EXAMPLE_COM`. A Woodpecker that doesn't answer
within 6 seconds is left out, and its statuses are read as before.
`forge_context` lists each CI system found under `ci_systems`, with its
version, who you are there, where the token came from and, if it can't be
used, why. Its capabilities appear under `capabilities` with `provider`
set to the system's name.
