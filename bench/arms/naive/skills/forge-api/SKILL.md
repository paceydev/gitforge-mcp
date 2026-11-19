---
name: forge-api
description: Gitea and Forgejo REST API for CI runs, logs, runners and pull requests. Use when asked about CI status, build failures, runners or pull requests on a Gitea or Forgejo repository.
---

# Gitea / Forgejo API

The API base is the forge's web URL plus `/api/v1`. Get the web URL, owner
and repository from `git remote get-url origin`. Authenticate with the token
in `$FORGE_TOKEN`:

```bash
curl -s -H "Authorization: token $FORGE_TOKEN" "$BASE/api/v1/repos/$OWNER/$REPO/..."
```

Useful endpoints under `/repos/{owner}/{repo}`:

| Endpoint | Returns |
|---|---|
| `GET /actions/runs?head_sha={sha}` | Workflow runs for a commit |
| `GET /actions/runs/{run_id}/jobs` | Jobs of a run |
| `GET /actions/tasks` | Recent runner tasks (jobs that have run) |
| `GET /actions/jobs/{job_id}/logs` | A job's log as text |
| `GET /commits/{sha}/status` | Combined commit status |
| `GET /actions/runners` | Runners registered to the repository |
| `GET /pulls?state=open` | Pull requests |
| `GET /pulls/{number}` | One pull request, including `mergeable` |
