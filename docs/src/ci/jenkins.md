# Jenkins

Builds, stages, console logs, nodes and writes for Jenkins, through its JSON API.

Jenkins posts one status per build (the Gitea plugin names them
`<owner>/<repo>/pipeline/head`, `…/pr-main`; GitHub Branch Source
`continuous-integration/jenkins/…`), linking to the build. gitforge follows
each link to Jenkins's JSON API and reads:

- each build as a job: its result, start, duration and, for a Pipeline
  build on a Jenkins with Pipeline: Stage View, its stages as steps. A
  stage Jenkins marks failed only because an earlier one failed reads as
  skipped. Unstable (tests failed) is a failure. A build waiting for a
  node, or for input, reads as waiting or blocked rather than running.
- `ci_logs`: the build's console.
- `ci_runners`: the nodes as idle, busy or offline, and this repository's
  builds in the queue with the labels they wait for.

`ci_control` changes them, sending each request once:

- `cancel` stops the run's unfinished builds. A build still in the queue
  with no number yet has to be cancelled in Jenkins.
- `rerun` builds each of the run's jobs again with the parameters its
  build had. Core Jenkins has no rebuild, so this is a new build, and a
  branch job builds its branch as it is now, which may be a newer commit.
  A build with a password or file parameter, whose value Jenkins doesn't
  show, is refused.
- `dispatch` starts a build. `workflow` names the job, as `folder/job` or
  its URL; left out, it is the one project this repository's statuses
  link to. For a multibranch project it builds the job for `ref` (default:
  the default branch); a lone job builds what it is set up to, so `ref` is
  refused there. `inputs` are the job's parameters, and one the job doesn't
  define is refused before anything is sent, because Jenkins would drop it
  without a word.

Jenkins doesn't say what a user may do, so these are offered to any
signed-in user, and a refusal names the permission it most likely lacked
(Job/Build, Job/Cancel). `rerun_failed` isn't offered: Jenkins has no API
for it.

Jenkins numbers builds per job, so `run` numbers never name a Jenkins
build. Links must be Jenkins's own (`/job/…/<n>/`), as display-url-api
makes them; Blue Ocean links aren't followed.

Jenkins takes an API token (**your name → Security → API Token**) with the
user it belongs to. Store it as `user:token`:

```bash
gitforge-mcp auth set jenkins.example.com
```

or set `GITFORGE_TOKEN_JENKINS_EXAMPLE_COM=dev:11a2…`, or store the token
alone and name the user in the config (`username = "dev"` under
`[ci."jenkins.example.com"]`). A Jenkins that lets anonymous users read
needs no token. The token is sent only to that Jenkins, as Basic auth,
which Jenkins exempts from its CSRF crumb. A password works too; then each
write carries the crumb and the session cookie it is tied to.
