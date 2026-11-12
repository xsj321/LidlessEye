#!/usr/bin/env bash
# Smoke-test the deployment artefacts, not the source tree.
#
#   ./scripts/deploy-smoke.sh          # build the image, run it, exercise the API
#   PORT=18041 ./scripts/deploy-smoke.sh
#
# This catches the class of breakage the unit tests cannot see: a Dockerfile
# that forgets an asset, a release build that strips the dashboard, an image
# that starts but cannot write its data directory, or a multi-tenant path that
# only fails outside `cargo test`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=lidlesseye:smoke
CONTAINER=lidlesseye-smoke
PORT="${PORT:-18040}"
BASE="http://127.0.0.1:${PORT}"

# MSYS/Git Bash would rewrite container paths such as /work into C:\...
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

# Docker Desktop wants a native path for the build context; `-m` keeps forward
# slashes, which is friendlier to the rest of the script.
HOST_PATH="$REPO_ROOT"
if command -v cygpath >/dev/null 2>&1; then
  HOST_PATH="$(cygpath -m "$REPO_ROOT")"
fi

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
  echo "FAIL $*" >&2
  echo "--- container log ---" >&2
  docker logs "$CONTAINER" 2>&1 | tail -20 >&2 || true
  exit 1
}

pass() { echo "ok   $*"; }

echo "building $IMAGE…"
docker build -q -f "$HOST_PATH/deploy/docker/Dockerfile" -t "$IMAGE" "$HOST_PATH" >/dev/null

cleanup
docker run -d --name "$CONTAINER" -p "127.0.0.1:${PORT}:4040" "$IMAGE" >/dev/null

# The health endpoint is the readiness signal the Helm chart probes.
# No `-o /dev/null` anywhere in this script: with MSYS_NO_PATHCONV exported the
# native curl on Windows sees the literal path and exits 23.
health=""
for _ in $(seq 1 100); do
  if health=$(curl -sf "${BASE}/api/v1/health" 2>&1); then break; fi
  sleep 0.2
done
[[ "$health" == *'"status":"ok"'* ]] || fail "the container is not healthy: ${health}"
pass "health: ${health}"

# The dashboard must be inside the binary: a release build that dropped
# `web/dist` serves a 503 placeholder instead.
index=$(curl -sf "${BASE}/" 2>&1) || fail "GET / failed: ${index}"
[[ "$index" == *'id="main"'* ]] || fail "GET / did not return the dashboard"
script=$(curl -sf "${BASE}/js/app.js" 2>&1) || fail "GET /js/app.js failed: ${script}"
[[ "$script" == *"Application shell"* ]] || fail "the dashboard script is not embedded"
pass "dashboard HTML and JS are embedded"

# Ingest a batch as one tenant.
now=$(date +%s%N)
body=$(cat <<JSON
{"reports":[
  {"service_name":"checkout","labels":{"pod":"c-1"},"profile_type":"cpu",
   "start_ns":$((now - 60000000000)),"end_ns":$now,
   "strings":["","charge","http","main"],
   "stacks":[{"frames":[[0,1,0,0],[0,2,0,0],[0,3,0,0]],"value":400000000},
             {"frames":[[0,2,0,0],[0,3,0,0]],"value":100000000}]},
  {"service_name":"search","labels":{"pod":"s-1"},"profile_type":"cpu",
   "start_ns":$((now - 60000000000)),"end_ns":$now,
   "strings":["","query","http","main"],
   "stacks":[{"frames":[[0,1,0,0],[0,2,0,0],[0,3,0,0]],"value":250000000}]}
]}
JSON
)
accepted=$(printf '%s' "$body" | curl -sf -X POST \
  -H 'content-type: application/json' -H 'X-Scope-OrgID: acme' \
  --data-binary @- "${BASE}/ingest") || fail "ingest failed"
[[ "$accepted" == *'"accepted":2'* ]] || fail "unexpected ingest response: ${accepted}"
pass "ingest: ${accepted}"

# Query it back, and confirm the totals add up.
flame=$(curl -sf -H 'X-Scope-OrgID: acme' "${BASE}/api/v1/flamegraph?from=now-1h")
[[ "$flame" == *'"total":750000000'* ]] || fail "flamegraph total is wrong: ${flame}"
pass "flamegraph total is 750ms"

# Tenant isolation, on a real process, with real files on disk.
other=$(curl -sf -H 'X-Scope-OrgID: other' "${BASE}/api/v1/flamegraph?from=now-1h")
[[ "$other" == *'"total":0'* ]] || fail "another tenant sees acme's data: ${other}"
pass "a second tenant starts empty"

tenants=$(curl -sf -H 'X-Scope-OrgID: acme' "${BASE}/api/v1/tenants")
[[ "$tenants" == *'"id":"acme"'* ]] || fail "the tenant is missing from /api/v1/tenants: ${tenants}"
[[ "$tenants" != *'"id":"other"'* ]] || fail "an unwritten tenant was materialised: ${tenants}"
pass "only tenants with data are listed"

# The comparison view, which is the only place that returns two windows.
diff=$(curl -sf -H 'X-Scope-OrgID: acme' \
  "${BASE}/api/v1/diff?from=now-1h&baseline_from=now-2h&baseline_until=now-1h")
[[ "$diff" == *'"diff":[750000000'* ]] || fail "the diff is wrong: ${diff}"
pass "diff: empty baseline against the current window"

# Metrics are for the operator and must not carry tenant labels.
metrics=$(curl -sf "${BASE}/metrics")
[[ "$metrics" == *"lidlesseye_tenants 1"* ]] || fail "the tenant gauge is wrong: ${metrics}"
[[ "$metrics" != *"acme"* ]] || fail "a tenant id leaked into /metrics"
pass "metrics expose the tenant count, not the tenant names"

echo "all deployment checks passed"
