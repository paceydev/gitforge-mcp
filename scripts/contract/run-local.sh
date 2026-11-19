#!/usr/bin/env bash
# Runs the contract tests on a developer machine: starts the forges in one
# Linux container (Forgejo publishes no macOS or Windows binaries), then runs
# tests/contract.rs on the host against the published ports.
#
#   scripts/contract/run-local.sh          provision, test, leave running
#   scripts/contract/run-local.sh --down   remove the container
#
# GITFORGE_CONTRACT_NAME and GITFORGE_CONTRACT_PORT_OFFSET run a second set
# beside the first (another worktree's), on ports shifted by the offset.
#
# Needs Docker (or a compatible runtime: OrbStack, Colima, Podman's docker
# shim). Downloads are kept in a named volume, so later runs start quickly.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
name="${GITFORGE_CONTRACT_NAME:-gitforge-contract}"
offset="${GITFORGE_CONTRACT_PORT_OFFSET:-0}"
first=$((3101 + offset)) last=$((3112 + offset))
image=node:24-bookworm   # the image CI's default runner uses

if [ "${1:-}" = --down ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  echo "removed $name"
  exit 0
fi

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" \
  -p "127.0.0.1:$first-$last:$first-$last" \
  -v "$repo/scripts/contract:/contract:ro" \
  -v gitforge-contract-downloads:/downloads \
  "$image" sleep infinity >/dev/null

docker exec "$name" /contract/provision.sh --bind 0.0.0.0 \
  --downloads /downloads --work /work --out /work/targets.json --port-offset "$offset"

mkdir -p "$repo/target"
targets="$repo/target/$name-targets.json"
docker cp "$name:/work/targets.json" "$targets" >/dev/null

cd "$repo"
GITFORGE_CONTRACT_TARGETS="$targets" GITFORGE_CONTRACT_REQUIRED=1 \
  cargo test --test contract -- --nocapture
# The write tests change the forges (merge, cancel, rerun), so they run
# after the read-only checks, never alongside them.
GITFORGE_CONTRACT_TARGETS="$targets" GITFORGE_CONTRACT_REQUIRED=1 \
  cargo test --test contract_writes -- --nocapture
# Repository settings: apply then revert each section, on a repository of
# its own.
GITFORGE_CONTRACT_TARGETS="$targets" GITFORGE_CONTRACT_REQUIRED=1 \
  cargo test --test contract_settings -- --nocapture
# Woodpecker's reads, then its writes (cancel, restart, a manual pipeline).
GITFORGE_CONTRACT_TARGETS="$targets" GITFORGE_CONTRACT_REQUIRED=1 \
  cargo test --test contract_woodpecker -- --nocapture
# Jenkins's writes (stop, rebuild, a build with parameters).
GITFORGE_CONTRACT_TARGETS="$targets" GITFORGE_CONTRACT_REQUIRED=1 \
  cargo test --test contract_jenkins -- --nocapture
echo "forges still running in container $name; remove with: $0 --down"
