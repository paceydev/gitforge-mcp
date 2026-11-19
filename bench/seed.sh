# Seeds one forge with the benchmark's scenario repositories. Sourced by
# scripts/contract/provision.sh --bench, after the forge, its contract runner
# and its tokens are set up; uses that script's helpers and variables
# (api, json_field, as_forge, $base, $admin_token, $dir, $bin, $rbin, $user,
# $workflows, $name, $kind, $version).
#
# Every repository is private, as work repositories are: an agent without the
# token can't read the forge at all, through the API or the web pages.

bench_dir="$(cd "$here/../../bench" && pwd)"

# A runner for every repository gfadmin owns, so the scenario repositories
# get their jobs run. Repository runners serve only their own repository.
bench_runner() {
  local token
  token="$(as_forge "$dir" "$bin" actions generate-runner-token --scope gfadmin | tail -1)"
  mkdir -p "$dir/bench-runner"
  chown "$user" "$dir/bench-runner"
  runuser -u "$user" -- bash -c "cd '$dir/bench-runner' && HOME=/home/$user '$rbin' register --no-interactive \
    --instance '$base' --token '$token' --name bench-runner --labels contract:host >register.log 2>&1"
  runuser -u "$user" -- bash -c "cd '$dir/bench-runner' && HOME=/home/$user nohup '$rbin' daemon >daemon.log 2>&1 &"
}

mkrepo() { # name
  api POST "$base/api/v1/user/repos" "$admin_token" \
    "{\"name\":\"$1\",\"private\":true,\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null
}

# Creates or replaces a file with one commit and prints the commit's sha.
put() { # repo path source-file message branch [new-branch]
  local repo="$1" path="$2" src="$3" message="$4" branch="$5" new_branch="${6:-}" sha="" body
  sha="$(api GET "$base/api/v1/repos/gfadmin/$repo/contents/$path?ref=$branch" "$admin_token" 2>/dev/null \
    | json_field sha 2>/dev/null || true)"
  body="{\"content\":\"$(base64 -w0 < "$src")\",\"message\":\"$message\",\"branch\":\"$branch\""
  [ -n "$new_branch" ] && body="$body,\"new_branch\":\"$new_branch\""
  if [ -n "$sha" ] && [ "$sha" != undefined ]; then
    api PUT "$base/api/v1/repos/gfadmin/$repo/contents/$path" "$admin_token" "$body,\"sha\":\"$sha\"}" \
      | json_field commit.sha
  else
    api POST "$base/api/v1/repos/gfadmin/$repo/contents/$path" "$admin_token" "$body}" | json_field commit.sha
  fi
}

text() { # content -> a temporary file holding it
  local f
  f="$(mktemp)"
  printf '%s\n' "$1" > "$f"
  echo "$f"
}

# Waits until every run of `repo` has finished: `count` runs, or, with
# `allow_waiting`, until the only unfinished run is one that can't start.
settle() { # repo count [allow_waiting]
  local repo="$1" count="$2" allow="${3:-}" state
  for _ in $(seq 1 180); do
    state="$(api GET "$base/api/v1/repos/gfadmin/$repo/actions/runs?limit=50" "$admin_token" | node -e '
      let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
        const runs=(JSON.parse(s).workflow_runs)||[];
        const final=r=>["success","failure","cancelled","skipped"].includes(r.status)||(r.status==="completed");
        console.log(runs.length+" "+runs.filter(r=>!final(r)).length);
      })')"
    set -- $state
    if [ "$1" -ge "$count" ] && { [ "$2" -eq 0 ] || [ -n "$allow" ]; }; then
      return 0
    fi
    sleep 2
  done
  echo "$name: runs in $repo did not settle" >&2
  return 1
}

# Waits until runner task `job` of `repo` has finished.
task_done() { # repo job
  for _ in $(seq 1 120); do
    if api GET "$base/api/v1/repos/gfadmin/$1/actions/tasks?limit=50" "$admin_token" | node -e '
      let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
        const t=((JSON.parse(s).workflow_runs)||[]).find(t=>t.name===process.argv[1]);
        process.exit(t&&["success","failure"].includes(t.status)?0:1);
      })' "$2"; then
      return 0
    fi
    sleep 2
  done
  echo "$name: job $2 in $1 never finished" >&2
  return 1
}

seed_bench() {
  local wf="$workflows/ci.yml" sc="$bench_dir/scenarios"
  echo "   seeding benchmark scenarios" >&2
  bench_runner

  # First, while few runs exist on the forge, so that this repository's run
  # ids overlap its run numbers (ids count runs across the whole forge).
  # Twelve runs, one at a time: a newer push can cancel an older run that
  # hasn't started, and every run here must finish.
  mkrepo bench-renumbered
  put bench-renumbered "$wf" "$sc/renumbered.yml" "Add CI" main >/dev/null
  settle bench-renumbered 1
  for i in $(seq 2 12); do
    put bench-renumbered counter.txt "$(text "$i")" "Change $i" main >/dev/null
    settle bench-renumbered "$i"
  done

  for scenario in green red skipped queued; do
    mkrepo "bench-$scenario"
    put "bench-$scenario" "$wf" "$sc/$scenario.yml" "Add CI" main >/dev/null
  done

  # A pull request that merges cleanly but whose CI fails.
  mkrepo bench-pr-red
  put bench-pr-red "$wf" "$sc/green.yml" "Add CI" main >/dev/null
  put bench-pr-red "$wf" "$sc/pr-red-head.yml" "Rework checkout totals" main feature/checkout-totals >/dev/null
  api POST "$base/api/v1/repos/gfadmin/bench-pr-red/pulls" "$admin_token" \
    '{"head":"feature/checkout-totals","base":"main","title":"Rework checkout totals"}' >/dev/null

  # A pull request whose CI passes but which conflicts with main.
  mkrepo bench-pr-conflict
  put bench-pr-conflict "$wf" "$sc/green.yml" "Add CI" main >/dev/null
  put bench-pr-conflict config.txt "$(text 'timeout = 30')" "Add config" main >/dev/null
  put bench-pr-conflict config.txt "$(text 'timeout = 45')" "Raise the timeout" main feature/raise-timeout >/dev/null
  put bench-pr-conflict config.txt "$(text 'timeout = 60')" "Raise the timeout further" main >/dev/null
  api POST "$base/api/v1/repos/gfadmin/bench-pr-conflict/pulls" "$admin_token" \
    '{"head":"feature/raise-timeout","base":"main","title":"Raise the timeout"}' >/dev/null

  settle bench-green 1
  settle bench-red 1
  settle bench-skipped 1
  task_done bench-queued build
  settle bench-pr-red 2
  settle bench-pr-conflict 4

  # A run number whose run failed while the run with that id passed.
  local renumbered
  renumbered="$(api GET "$base/api/v1/repos/gfadmin/bench-renumbered/actions/runs?limit=50" "$admin_token" | node -e '
    let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
      const runs=(JSON.parse(s).workflow_runs)||[];
      const num=r=>r.run_number||r.index_in_repo;
      const ok=r=>r.status==="success"||r.conclusion==="success";
      for (const n of [9,11,4]) {
        const byNumber=runs.find(r=>num(r)===n), byId=runs.find(r=>r.id===n);
        if (byNumber && byId && !ok(byNumber) && ok(byId)) { console.log(n); return; }
      }
      console.log("");
    })')"
  if [ -z "$renumbered" ]; then
    echo "$name: no run number in bench-renumbered differs from the run with that id" >&2
    exit 1
  fi

  bench_targets+=("$(printf '{"name":"%s","kind":"%s","version":"%s","base_url":"%s","token":"%s","owner":"gfadmin","renumbered_run":%s}' \
    "$name" "$kind" "$version" "$base" "$admin_token" "$renumbered")")
}
