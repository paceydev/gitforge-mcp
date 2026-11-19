# Credentials

Where the server finds a token for your forge, and which scopes to give it.

The server needs to call your forge's API as you. It tries these sources in
order and uses the first that has something:

1. **An environment variable** named after the host:
   `GITFORGE_TOKEN_GIT_EXAMPLE_COM` for `git.example.com`, or for
   `localhost:3000` `GITFORGE_TOKEN_LOCALHOST_3000`. For github.com it
   also reads `GH_TOKEN` and `GITHUB_TOKEN`, and for GitHub Enterprise
   Server `GH_ENTERPRISE_TOKEN` and `GITHUB_ENTERPRISE_TOKEN`, as the GitHub
   CLI does.
2. **The OS keyring**: macOS Keychain, Windows Credential Manager, or the
   Secret Service on Linux. Store a token there with:

   ```bash
   gitforge-mcp auth set git.example.com
   ```

   It prompts without echoing, or reads the token from stdin. Remove it
   with `gitforge-mcp auth remove git.example.com`.
3. **The GitHub CLI**, for GitHub hosts only: `gh auth token`, if you've
   signed in with `gh auth login`.
4. **git's credential helper**: whatever already lets you `git push` to that
   host over HTTPS. If you can push, this often needs no setup at all. The
   server asks non-interactively, so it never pops up a login window. On
   GitHub what the helper holds is a token, and it's used as one.

To see which source a repository would use, run this inside it:

```bash
gitforge-mcp auth status
```

## Creating a token (Gitea and Forgejo)

In the forge's web UI, open **Settings → Applications → Generate New Token**.
Choose scopes from this table:

| Scope | Needed for |
|---|---|
| `read:repository` | everything read-only: repository, CI runs and logs, pull requests |
| `read:user` | showing who you're signed in as, and runners registered to your own account and your other repositories' jobs |
| `read:organization` | runners registered to an organisation that owns the repository, and its repositories' jobs |
| `read:admin` | runners registered to the whole instance, and every repository's jobs (site administrators only) |
| `write:repository` | opening and merging pull requests; cancelling, rerunning and dispatching CI; changing repository settings, branch protection and webhooks |

Only `read:repository` is required. The others let `ci_runners` see more of
the runners your jobs actually run on: most instances register runners for
the whole instance, which a repository's own runner list doesn't include.
They also let it see the other repositories' jobs on those runners, so it
can say whose jobs are holding them.

Repository settings, webhooks and runner listing also need you to be an
**admin of the repository**. That's a role on the forge, not a token scope.

## Creating a token (GitHub)

`gh auth login` is enough if you use the GitHub CLI. Otherwise create a
fine-grained personal access token (**Settings → Developer settings →
Personal access tokens**) for the repositories you want, with these
repository permissions:

| Permission | Needed for |
|---|---|
| *Metadata*: read | everything: GitHub grants it with any other |
| *Actions*: read | CI runs, jobs and logs |
| *Checks*: read | CI outside Actions that reports through check runs |
| *Commit statuses*: read | CI outside Actions that reports through commit statuses |
| *Pull requests*: read | listing and reading pull requests and their reviews |
| *Contents*: write | merging pull requests, and deleting the branch afterwards |
| *Pull requests*: write | opening pull requests, and merging once checks pass |
| *Actions*: write | cancelling, rerunning and dispatching CI |
| *Administration*: read | the repository's runners and settings, and its branch protection (with an admin role) |
| *Administration*: write | changing the repository's settings and branch protection |
| *Webhooks*: read, or write | listing webhooks and their deliveries, or changing and testing them |

An organisation's runners need an organisation admin, with the
organisation permission *Self-hosted runners*: read. A classic token with
`repo` (and `workflow` to dispatch, `admin:repo_hook` for webhooks) works
too.
