# forge_context

Which repository and forge the server is bound to, who you are there, and why any capability is missing.

It reports the forge's kind and version, who the credential belongs to and
where it came from, your permissions, and every capability with the reason
when one is unavailable. Agents call it first, and whenever a tool they
expected is missing. `refresh: true` probes
again, for example after the forge comes back up.

## Parameters

| Parameter | Meaning |
|---|---|
| `refresh` | Probe the forge again instead of returning what was found when the server started (or last refreshed). Default false. |

## Reply

```jsonc
{
  "status": "live",
  "forge": { "kind": "forgejo", "version": "15.0.11", "detected_by": "/api/forgejo/v1/version" },
  "credential": { "source": "git credential helper", "tried": [ /* … */ ] },
  "repository": { "full_name": "acme/widgets", "permissions": { "admin": true, "push": true, "pull": true } },
  "capabilities": [
    { "feature": "ci_runs", "available": true },
    { "feature": "ci_cancel", "available": false, "reason": "needs Forgejo 16 or later; this server runs 15.0.11" }
  ],
  "tools": ["ci_runners", "ci_status", "forge_context", "pull_request"]
}
```

| `status` | Meaning |
|---|---|
| `live` | Probed just now. |
| `cached` | The forge didn't answer, so the server reports the last successful probe. Tools stay listed and fail clearly until the forge is back. |
| `unreachable` | The forge didn't answer and has never been probed from this machine. |
| `error` | The forge answered with an error, such as no access or no such repository. `problems` says what. |
| `unbound` | The directory isn't a git checkout, or has no usable remote. |
