# Sourced by provision.sh: starts the Woodpecker in woodpecker.txt beside
# the forge it is paired with, and seeds it for tests/contract_woodpecker.rs.
#
# Woodpecker signs users in only through the forge's OAuth login, which a
# script can't click through, so the user is written to Woodpecker's
# database directly: the forge's admin, with a forge access token where the
# OAuth token would be. Its personal API token is a JWT signed with the
# user's hash, made the way Woodpecker makes one (shared/token/token.go:
# HS256, claims type=user and user-id). Then, through Woodpecker's API, the
# repository is activated, which adds the forge webhook, and a commit with
# three workflows (one passes, one fails, one only runs when started by
# hand) gives it pipeline 1. An agent on the local backend runs the steps
# as shell commands on this host: no Docker needed.
#
# Uses from provision.sh: $here $downloads $arch $user $bind $port_offset
# $ua, and rand, api, json_field. Sets woodpecker_json for the targets file.

woodpecker_url_for() { # part (server|agent) version -> release URL
  echo "https://github.com/woodpecker-ci/woodpecker/releases/download/v$2/woodpecker-$1_linux_$arch.tar.gz"
}

fetch_woodpecker() { # part version sha -> path to the binary
  local part="$1" version="$2" sha="$3" archive bin
  bin="$downloads/woodpecker-$part-$version-$arch"
  if [ ! -x "$bin" ]; then
    archive="$bin.tar.gz"
    curl --fail --silent --show-error --location -A "$ua" --retry 5 --retry-all-errors \
      -o "$archive" "$(woodpecker_url_for "$part" "$version")" >&2
    echo "$sha  $archive" | sha256sum --check --quiet >&2 || {
      echo "woodpecker-$part $version did not match its checksum" >&2
      rm -f "$archive"
      return 1
    }
    tar -xzf "$archive" -C "$downloads" "woodpecker-$part" >&2
    mv "$downloads/woodpecker-$part" "$bin"
    rm -f "$archive"
    chmod 755 "$bin"
  fi
  echo "$bin"
}

# The build, read once. (`read` empties its variables at the end of the
# file, so each field is copied out.)
woodpecker_pair=""
while read -r kind_ version_ port_ grpc_ pair_ srv_amd64 srv_arm64 agent_amd64 agent_arm64; do
  case "$kind_" in ''|'#'*) continue ;; esac
  woodpecker_pair="$pair_" wversion="$version_" wport="$port_" wgrpc="$grpc_"
  if [ "$arch" = arm64 ]; then
    wsrv_sha="$srv_arm64" wagent_sha="$agent_arm64"
  else
    wsrv_sha="$srv_amd64" wagent_sha="$agent_amd64"
  fi
done < "$here/woodpecker.txt"

# Writes the forge admin into Woodpecker's database and prints its API token.
woodpecker_user() { # database forge-user-id forge-token
  node - "$1" "$2" "$3" "$(rand)" <<'EOF'
const crypto = require("node:crypto");
const { DatabaseSync } = require("node:sqlite");
const [db, remoteId, forgeToken, hash] = process.argv.slice(2);
const row = new DatabaseSync(db)
  .prepare(
    "INSERT INTO users (forge_id, forge_remote_id, login, access_token, refresh_token, expiry, email, avatar, admin, hash, org_id) " +
      "VALUES (1, ?, 'gfadmin', ?, '', 4102444800, 'gfadmin@example.invalid', '', 1, ?, 0)",
  )
  .run(remoteId, forgeToken, hash);
const part = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
const signed = part({ alg: "HS256", typ: "JWT" }) + "." + part({ type: "user", "user-id": String(row.lastInsertRowid) });
console.log(signed + "." + crypto.createHmac("sha256", hash).update(signed).digest("base64url"));
EOF
}

wp_api() { # method url token
  curl --fail-with-body --silent --show-error -X "$1" -A "$ua" -H "Authorization: Bearer $3" "$2"
}

woodpecker_workflows() { # -> the ChangeFiles body adding the three workflows
  local build test slow
  build="$(base64 -w0 <<'EOF'
when:
  - event: [push, manual]
skip_clone: true
steps:
  - name: compile
    image: bash
    commands:
      - echo contract-woodpecker-build
EOF
)"
  test="$(base64 -w0 <<'EOF'
when:
  - event: push
skip_clone: true
steps:
  - name: setup
    image: bash
    commands:
      - echo setting up
  - name: unit
    image: bash
    commands:
      - echo contract-woodpecker-failure
      - exit 1
EOF
)"
  slow="$(base64 -w0 <<'EOF'
when:
  - event: manual
skip_clone: true
steps:
  - name: wait
    image: bash
    commands:
      - sleep 600
EOF
)"
  printf '{"branch":"main","message":"Add the Woodpecker workflows","files":[%s,%s,%s]}' \
    "{\"operation\":\"create\",\"path\":\".woodpecker/build.yaml\",\"content\":\"$build\"}" \
    "{\"operation\":\"create\",\"path\":\".woodpecker/test.yaml\",\"content\":\"$test\"}" \
    "{\"operation\":\"create\",\"path\":\".woodpecker/slow.yaml\",\"content\":\"$slow\"}"
}

seed_woodpecker() { # forge-dir forge-base admin-token
  local fdir="$1" fbase="$2" ftoken="$3"
  local port=$((wport + port_offset)) grpc=$((wgrpc + port_offset))
  local wdir="$fdir/woodpecker" wbase="http://127.0.0.1:$port"
  echo "== woodpecker-$wversion on port $port, beside $woodpecker_pair" >&2
  local server agent
  server="$(fetch_woodpecker server "$wversion" "$wsrv_sha")"
  agent="$(fetch_woodpecker agent "$wversion" "$wagent_sha")"
  mkdir -p "$wdir"
  chown "$user" "$wdir"

  local oauth repo_remote_id forge_user_id agent_secret
  oauth="$(api POST "$fbase/api/v1/user/applications/oauth2" "$ftoken" \
    "{\"name\":\"woodpecker\",\"redirect_uris\":[\"$wbase/authorize\"],\"confidential_client\":true}")"
  repo_remote_id="$(api POST "$fbase/api/v1/user/repos" "$ftoken" \
    '{"name":"woodpecker-widgets","private":true,"auto_init":true,"default_branch":"main"}' | json_field id)"
  # Its CI is Woodpecker's alone, as on a forge without Actions.
  api PATCH "$fbase/api/v1/repos/gfadmin/woodpecker-widgets" "$ftoken" '{"has_actions":false}' >/dev/null
  forge_user_id="$(api GET "$fbase/api/v1/user" "$ftoken" | json_field id)"
  agent_secret="$(rand)"

  runuser -u "$user" -- env HOME="/home/$user" \
    WOODPECKER_HOST="$wbase" WOODPECKER_SERVER_ADDR="$bind:$port" WOODPECKER_GRPC_ADDR="127.0.0.1:$grpc" \
    WOODPECKER_AGENT_SECRET="$agent_secret" WOODPECKER_FORGEJO=true WOODPECKER_FORGEJO_URL="$fbase" \
    WOODPECKER_FORGEJO_CLIENT="$(echo "$oauth" | json_field client_id)" \
    WOODPECKER_FORGEJO_SECRET="$(echo "$oauth" | json_field client_secret)" \
    WOODPECKER_DATABASE_DRIVER=sqlite3 WOODPECKER_DATABASE_DATASOURCE="$wdir/woodpecker.sqlite" \
    WOODPECKER_ADMIN=gfadmin WOODPECKER_LOG_LEVEL=warn \
    nohup "$server" >"$wdir/server.log" 2>&1 &
  for _ in $(seq 1 60); do
    curl --silent --fail -A "$ua" "$wbase/version" >/dev/null 2>&1 && break
    sleep 1
  done
  curl --silent --fail -A "$ua" "$wbase/version" >/dev/null || {
    echo "woodpecker did not start; its log:" >&2
    tail -50 "$wdir/server.log" >&2 || true
    exit 1
  }

  local wtoken wrepo_id push_sha
  wtoken="$(woodpecker_user "$wdir/woodpecker.sqlite" "$forge_user_id" "$ftoken" 2>/dev/null)"
  wrepo_id="$(wp_api POST "$wbase/api/repos?forge_remote_id=$repo_remote_id" "$wtoken" | json_field id)"

  runuser -u "$user" -- bash -c "cd '$wdir' && HOME=/home/$user WOODPECKER_SERVER=127.0.0.1:$grpc \
    WOODPECKER_AGENT_SECRET='$agent_secret' WOODPECKER_BACKEND=local WOODPECKER_HOSTNAME=contract-agent \
    WOODPECKER_MAX_WORKFLOWS=4 WOODPECKER_HEALTHCHECK=false WOODPECKER_AGENT_CONFIG_FILE='$wdir/agent.conf' \
    WOODPECKER_LOG_LEVEL=warn nohup '$agent' >agent.log 2>&1 &"

  push_sha="$(api POST "$fbase/api/v1/repos/gfadmin/woodpecker-widgets/contents" "$ftoken" \
    "$(woodpecker_workflows)" | json_field commit.sha)"
  # Pipeline 1 is the push: wait for it, so the tests start from a result.
  local status=""
  for _ in $(seq 1 120); do
    status="$(wp_api GET "$wbase/api/repos/$wrepo_id/pipelines/1" "$wtoken" 2>/dev/null | json_field status 2>/dev/null || true)"
    case "$status" in success|failure|error|killed) break ;; esac
    sleep 2
  done
  [ "$status" = failure ] || {
    echo "woodpecker pipeline 1 ended '$status', expected failure; the logs:" >&2
    tail -30 "$wdir/server.log" "$wdir/agent.log" >&2 || true
    exit 1
  }
  woodpecker_json="$(printf '{"version":"%s","base_url":"%s","token":"%s","repo":"woodpecker-widgets","repo_id":%s,"push_sha":"%s"}' \
    "$wversion" "$wbase" "$wtoken" "$wrepo_id" "$push_sha")"
}
