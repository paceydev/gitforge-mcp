#!/usr/bin/env bash
# Starts the benchmark's forges in one Linux container, with the scenario
# repositories seeded, and writes target/bench-targets.json for bench/run.py.
#
#   bench/forges.sh [PREFIXES]   e.g. forgejo-17 or gitea-1.25,forgejo-15,forgejo-17
#                                (default: every forge)
#   bench/forges.sh --down
#
# Needs Docker. Uses the contract harness's forges and runners
# (scripts/contract/) and caches their downloads in a named volume.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
name=gitforge-bench
image=node:24-bookworm

if [ "${1:-}" = --down ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  echo "removed $name"
  exit 0
fi

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" \
  -p 127.0.0.1:3101-3105:3101-3105 \
  -v "$repo/scripts:/repo/scripts:ro" \
  -v "$repo/bench:/repo/bench:ro" \
  -v gitforge-contract-downloads:/downloads \
  "$image" sleep infinity >/dev/null

docker exec "$name" /repo/scripts/contract/provision.sh --bind 0.0.0.0 \
  --downloads /downloads --work /work --out /work/targets.json \
  --only "${1:-}" --bench /work/bench-targets.json

mkdir -p "$repo/target"
docker cp "$name:/work/bench-targets.json" "$repo/target/bench-targets.json" >/dev/null
echo "forges running in container $name; targets in target/bench-targets.json"
