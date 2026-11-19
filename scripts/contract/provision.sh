#!/usr/bin/env bash
# Starts every forge in forges.txt from its release binary and seeds each one
# for the contract tests: an admin, a read-only collaborator, a repository
# with Actions on and one with Actions off, a runner registered to the first
# repository, a workflow it runs (one job of each outcome), and an open pull
# request with a review that has an inline comment. Beside the forge
# woodpecker.txt pairs it with, a Woodpecker (see woodpecker.sh), and beside
# the one jenkins.txt pairs it with, a Jenkins (see jenkins.sh). Writes the
# targets (URLs, users, throwaway tokens, the commits to check) as JSON for
# tests/contract.rs.
#
# Runs as root on Linux with curl, xz and git: the CI job container, or the
# container run-local.sh starts. The forges themselves run as an unprivileged
# user, because Gitea and Forgejo refuse to run as root.
#
#   provision.sh [--bind ADDR] [--downloads DIR] [--work DIR] [--out FILE]
#                [--only NAME] [--bench FILE] [--port-offset N]
#
# --only keeps the forges whose name (e.g. forgejo-17.0.1) starts with NAME,
# or with any of a comma-separated list of names.
# --bench also seeds the benchmark's scenario repositories on each forge
# (bench/seed.sh) and writes what the benchmark needs to FILE.
# --port-offset adds N to every port, to run beside another set of forges.
#
# Nothing here needs Docker, so CI runs it in a job without the host's
# Docker socket.
set -euo pipefail
# Without this, a failure inside $(...) doesn't stop the script: a bad
# download once carried on with sha256sum's output as the binary's path.
shopt -s inherit_errexit

here="$(cd "$(dirname "$0")" && pwd)"
bind=127.0.0.1
only=""
bench_out=""
port_offset=0
downloads=/tmp/gitforge-contract/downloads
work=/tmp/gitforge-contract/work
out=/tmp/gitforge-contract/targets.json
while [ $# -gt 0 ]; do
  case "$1" in
    --bind) bind="$2"; shift 2 ;;
    --downloads) downloads="$2"; shift 2 ;;
    --work) work="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --only) only="$2"; shift 2 ;;
    --bench) bench_out="$2"; shift 2 ;;
    --port-offset) port_offset="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

user=gitforge-contract
id "$user" >/dev/null 2>&1 || useradd --create-home --shell /bin/bash "$user"
mkdir -p "$downloads" "$work" "$(dirname "$out")"

ua="gitforge-mcp-contract"
rand() { head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

url_for() { # name version -> release .xz URL for this architecture
  case "$1" in
    gitea) echo "https://dl.gitea.com/gitea/$2/gitea-$2-linux-$arch.xz" ;;
    forgejo) echo "https://codeberg.org/forgejo/forgejo/releases/download/v$2/forgejo-$2-linux-$arch.xz" ;;
    gitea-runner) echo "https://dl.gitea.com/gitea-runner/$2/gitea-runner-$2-linux-$arch.xz" ;;
    # code.forgejo.org challenges browser-like User-Agents; ours isn't one.
    forgejo-runner) echo "https://code.forgejo.org/forgejo/runner/releases/download/v$2/forgejo-runner-$2-linux-$arch.xz" ;;
  esac
}

fetch() { # name version sha -> path to an executable binary
  local kind="$1" version="$2" sha="$3" url bin
  url="$(url_for "$kind" "$version")"
  bin="$downloads/$kind-$version-$arch"
  if [ ! -x "$bin" ]; then
    # Codeberg's release downloads are slow and sometimes cut off partway
    # (curl exit 18), so resume, retry on any error, and only accept a file
    # whose checksum matches.
    local attempt
    for attempt in 1 2 3 4; do
      curl --fail --silent --show-error --location -A "$ua" \
        --retry 5 --retry-all-errors --retry-delay 5 \
        --continue-at - -o "$bin.xz" "$url" >&2 || true
      if echo "$sha  $bin.xz" | sha256sum --check --quiet >&2 2>/dev/null; then
        break
      fi
      echo "download $attempt of $url failed or did not match its checksum" >&2
      rm -f "$bin.xz"
      [ "$attempt" = 4 ] && return 1
      sleep 10
    done
    xz --decompress --force "$bin.xz"
    chmod 755 "$bin"
  fi
  echo "$bin"
}

as_forge() { # dir, command... (runs as the unprivileged user, in dir)
  local dir="$1"; shift
  runuser -u "$user" -- env HOME="/home/$user" GITEA_WORK_DIR="$dir" FORGEJO_WORK_DIR="$dir" \
    GITEA_CUSTOM="$dir/custom" FORGEJO_CUSTOM="$dir/custom" "$@"
}

api() { # method url token [json]
  local method="$1" url="$2" token="$3" body="${4:-}"
  if [ -n "$body" ]; then
    curl --fail-with-body --silent --show-error -X "$method" -A "$ua" \
      -H "Authorization: token $token" -H "Content-Type: application/json" -d "$body" "$url"
  else
    curl --fail-with-body --silent --show-error -X "$method" -A "$ua" \
      -H "Authorization: token $token" "$url"
  fi
}

# The runner for each forge kind, from runners.txt.
declare -A runner_version runner_sha
while read -r rname rversion rsha_amd64 rsha_arm64; do
  case "$rname" in ''|'#'*) continue ;; esac
  runner_version[$rname]="$rversion"
  if [ "$arch" = arm64 ]; then runner_sha[$rname]="$rsha_arm64"; else runner_sha[$rname]="$rsha_amd64"; fi
done < "$here/runners.txt"

json_field() { # field path, e.g. commit.sha, read from stdin
  node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{let v=JSON.parse(s);for(const k of process.argv[1].split("."))v=v[k];console.log(v)})' "$1"
}

# shellcheck source=woodpecker.sh
. "$here/woodpecker.sh"
# shellcheck source=jenkins.sh
. "$here/jenkins.sh"

targets=()
bench_targets=()
while read -r kind version port sha_amd64 sha_arm64; do
  case "$kind" in ''|'#'*) continue ;; esac
  sha="$sha_amd64"; [ "$arch" = arm64 ] && sha="$sha_arm64"
  name="$kind-$version"
  port=$((port + port_offset))
  if [ -n "$only" ]; then
    keep=""
    for prefix in ${only//,/ }; do
      case "$name" in "$prefix"*) keep=1 ;; esac
    done
    [ -n "$keep" ] || continue
  fi
  echo "== $name on port $port" >&2
  bin="$(fetch "$kind" "$version" "$sha")"
  dir="$work/$name"
  rm -rf "$dir"
  mkdir -p "$dir/custom/conf" "$dir/data" "$dir/log"
  cat > "$dir/custom/conf/app.ini" <<EOF
APP_NAME = $name
RUN_USER = $user
RUN_MODE = prod
WORK_PATH = $dir

[server]
PROTOCOL = http
HTTP_ADDR = $bind
HTTP_PORT = $port
ROOT_URL = http://127.0.0.1:$port/
DISABLE_SSH = true
START_SSH_SERVER = false
OFFLINE_MODE = true
LFS_START_SERVER = false

[database]
DB_TYPE = sqlite3
PATH = $dir/data/forge.db

[repository]
ROOT = $dir/repos

[security]
INSTALL_LOCK = true
SECRET_KEY = $(rand)
INTERNAL_TOKEN = $(rand)$(rand)

[oauth2]
JWT_SECRET = $(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')

[service]
DISABLE_REGISTRATION = true
REQUIRE_SIGNIN_VIEW = false

[actions]
ENABLED = true

[api]
ENABLE_SWAGGER = true

[log]
MODE = file
LEVEL = Warn
ROOT_PATH = $dir/log

[mailer]
ENABLED = false

[webhook]
; Woodpecker, beside the forge, takes its webhooks on this host.
ALLOWED_HOST_LIST = loopback
EOF
  chown -R "$user" "$dir"

  as_forge "$dir" "$bin" migrate >/dev/null
  admin_pw="$(rand)"
  reader_pw="$(rand)"
  as_forge "$dir" "$bin" admin user create --admin --username gfadmin \
    --password "$admin_pw" --email gfadmin@example.invalid --must-change-password=false >/dev/null
  as_forge "$dir" "$bin" admin user create --username gfreader \
    --password "$reader_pw" --email gfreader@example.invalid --must-change-password=false >/dev/null
  admin_token="$(as_forge "$dir" "$bin" admin user generate-access-token \
    --username gfadmin --token-name contract --scopes all --raw)"
  reader_token="$(as_forge "$dir" "$bin" admin user generate-access-token \
    --username gfreader --token-name contract --scopes read:repository,read:user --raw)"

  as_forge "$dir" nohup "$bin" web >"$dir/log/stdout.log" 2>&1 &
  base="http://127.0.0.1:$port"
  for _ in $(seq 1 60); do
    curl --silent --fail -A "$ua" "$base/api/v1/version" >/dev/null 2>&1 && break
    sleep 1
  done
  curl --silent --fail -A "$ua" "$base/api/v1/version" >/dev/null || {
    echo "$name did not start; its log:" >&2
    tail -50 "$dir/log/"*.log >&2 || true
    exit 1
  }

  api POST "$base/api/v1/user/repos" "$admin_token" \
    '{"name":"widgets","private":true,"auto_init":true,"default_branch":"main"}' >/dev/null
  api PUT "$base/api/v1/repos/gfadmin/widgets/collaborators/gfreader" "$admin_token" \
    '{"permission":"read"}' >/dev/null
  api POST "$base/api/v1/user/repos" "$admin_token" \
    '{"name":"no-actions","private":true,"auto_init":true,"default_branch":"main"}' >/dev/null
  api PATCH "$base/api/v1/repos/gfadmin/no-actions" "$admin_token" '{"has_actions":false}' >/dev/null
  # CI outside the forge, reported the way Woodpecker does: commit statuses
  # on no-actions' main, one step passed and one failed.
  ext_sha="$(api GET "$base/api/v1/repos/gfadmin/no-actions/branches/main" "$admin_token" | json_field commit.id)"
  for step in build:success lint:failure; do
    api POST "$base/api/v1/repos/gfadmin/no-actions/statuses/$ext_sha" "$admin_token" \
      "{\"state\":\"${step#*:}\",\"context\":\"ci/woodpecker/push/${step%%:*}\",\"target_url\":\"https://ci.example.com/repos/1/pipeline/1\",\"description\":\"contract\"}" >/dev/null
  done

  # A runner registered to the repository, running jobs on this host.
  rname="$kind-runner"
  rbin="$(fetch "$rname" "${runner_version[$rname]}" "${runner_sha[$rname]}")"
  rtoken="$(as_forge "$dir" "$bin" actions generate-runner-token --scope gfadmin/widgets | tail -1)"
  mkdir -p "$dir/runner"
  chown "$user" "$dir/runner"
  runuser -u "$user" -- bash -c "cd '$dir/runner' && HOME=/home/$user '$rbin' register --no-interactive \
    --instance '$base' --token '$rtoken' --name contract-runner --labels contract:host >register.log 2>&1"
  runuser -u "$user" -- bash -c "cd '$dir/runner' && HOME=/home/$user nohup '$rbin' daemon >daemon.log 2>&1 &"

  # The workflow, on main, then a branch and a pull request from it.
  workflows=".forgejo/workflows"; [ "$kind" = gitea ] && workflows=".gitea/workflows"
  ci_sha="$(api POST "$base/api/v1/repos/gfadmin/widgets/contents/$workflows/contract.yml" "$admin_token" \
    "{\"content\":\"$(base64 -w0 < "$here/workflow.yml")\",\"message\":\"Add the contract workflow\",\"branch\":\"main\"}" \
    | json_field commit.sha)"
  pr_sha="$(api POST "$base/api/v1/repos/gfadmin/widgets/contents/notes.txt" "$admin_token" \
    "{\"content\":\"$(printf 'a change for review\n' | base64 -w0)\",\"message\":\"A change for review\",\"branch\":\"main\",\"new_branch\":\"feature/contract\"}" \
    | json_field commit.sha)"
  pr_number="$(api POST "$base/api/v1/repos/gfadmin/widgets/pulls" "$admin_token" \
    '{"head":"feature/contract","base":"main","title":"Contract pull request","body":"Opened by the contract tests."}' \
    | json_field number)"
  # A review on it with an inline comment on its one changed line. The
  # admin opened it, so the review can only be a comment.
  api POST "$base/api/v1/repos/gfadmin/widgets/pulls/$pr_number/reviews" "$admin_token" \
    '{"event":"COMMENT","body":"contract-review-marker: one question","comments":[{"path":"notes.txt","new_position":1,"body":"contract-comment-marker: why this line?"}]}' >/dev/null

  # A queue: one runner for the whole instance, labelled `shared`, and two
  # repositories whose workflow (queue.yml) both want it. One's job runs on
  # it, the other's waits, and each has a job no runner can take.
  stoken="$(as_forge "$dir" "$bin" actions generate-runner-token | tail -1)"
  mkdir -p "$dir/shared-runner"
  chown "$user" "$dir/shared-runner"
  runuser -u "$user" -- bash -c "cd '$dir/shared-runner' && HOME=/home/$user '$rbin' register --no-interactive \
    --instance '$base' --token '$stoken' --name shared-runner --labels shared:host >register.log 2>&1"
  runuser -u "$user" -- bash -c "cd '$dir/shared-runner' && HOME=/home/$user nohup '$rbin' daemon >daemon.log 2>&1 &"
  for queue_repo in queue-a queue-b; do
    api POST "$base/api/v1/user/repos" "$admin_token" \
      "{\"name\":\"$queue_repo\",\"private\":true,\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null
    api POST "$base/api/v1/repos/gfadmin/$queue_repo/contents/$workflows/queue.yml" "$admin_token" \
      "{\"content\":\"$(base64 -w0 < "$here/queue.yml")\",\"message\":\"Add the queue workflow\",\"branch\":\"main\"}" >/dev/null
  done

  woodpecker_json=null
  if [ "$name" = "$woodpecker_pair" ]; then
    seed_woodpecker "$dir" "$base" "$admin_token"
  fi
  jenkins_json=null
  if [ "$name" = "$jenkins_pair" ]; then
    seed_jenkins "$dir" "$base" "$admin_token"
  fi

  targets+=("$(printf '{"name":"%s","kind":"%s","version":"%s","base_url":"%s","admin_token":"%s","reader_token":"%s","ci_sha":"%s","pr_number":%s,"pr_sha":"%s","runner_name":"contract-runner","woodpecker":%s,"jenkins":%s}' \
    "$name" "$kind" "$version" "$base" "$admin_token" "$reader_token" "$ci_sha" "$pr_number" "$pr_sha" "$woodpecker_json" "$jenkins_json")")
  if [ -n "$bench_out" ]; then
    # shellcheck source=/dev/null
    . "$here/../../bench/seed.sh"
    seed_bench
  fi
done < "$here/forges.txt"

{
  echo "["
  for i in "${!targets[@]}"; do
    sep=","; [ "$i" -eq $((${#targets[@]} - 1)) ] && sep=""
    echo "  ${targets[$i]}$sep"
  done
  echo "]"
} > "$out"
chmod 644 "$out"
echo "wrote $out (${#targets[@]} forges)" >&2
if [ -n "$bench_out" ]; then
  {
    echo "["
    for i in "${!bench_targets[@]}"; do
      sep=","; [ "$i" -eq $((${#bench_targets[@]} - 1)) ] && sep=""
      echo "  ${bench_targets[$i]}$sep"
    done
    echo "]"
  } > "$bench_out"
  chmod 644 "$bench_out"
  echo "wrote $bench_out" >&2
fi
