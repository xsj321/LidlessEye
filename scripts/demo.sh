#!/usr/bin/env bash
# Run LidlessEye locally with demo data and print the dashboard URL.
#
#   ./scripts/demo.sh              # build, start on 127.0.0.1:4040, seed
#   PORT=5050 ./scripts/demo.sh    # another port
#   ./scripts/demo.sh --stop       # remove the container
#
# The service runs in a detached container so the dashboard stays reachable
# after the shell returns. Its data directory lives inside the container: this
# is a throwaway instance, not a deployment (use deploy/ for that).
# Requires Docker only: no local Rust, node or browser.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=lidlesseye-dev
NAME=lidlesseye-demo
PORT="${PORT:-4040}"

# MSYS/Git Bash would rewrite container paths such as /work into C://...
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

if [ "${1:-}" = "--stop" ]; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  echo "stopped $NAME"
  exit 0
fi

# Docker Desktop on Windows wants a native path for the bind mount.
if command -v cygpath >/dev/null 2>&1; then
  HOST_PATH="$(cygpath -w "$REPO_ROOT")"
else
  HOST_PATH="$REPO_ROOT"
fi

echo "building the server binary…"
docker run --rm \
  -v "${HOST_PATH}:/work" \
  -v lidlesseye-target:/build \
  -e CARGO_TARGET_DIR=/build \
  -w /work \
  "$IMAGE" cargo build -p lidlesseye --locked --quiet

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" \
  -p "127.0.0.1:${PORT}:${PORT}" \
  -v "${HOST_PATH}:/work" \
  -v lidlesseye-target:/build \
  -w /work \
  "$IMAGE" bash -c "
    lib=\$(dirname \"\$(find /build/debug/build -name libduckdb.so -type f -print -quit)\")
    export LD_LIBRARY_PATH=\"\$lib\"
    exec /build/debug/lidlesseye serve --data-dir /tmp/lidlesseye-demo --listen 0.0.0.0:${PORT}
  " >/dev/null

echo -n "waiting for the server"
for _ in $(seq 1 60); do
  # Redirect in the shell, not with `curl -o /dev/null`: MSYS_NO_PATHCONV=1
  # above stops Git Bash from translating that path for the native curl.exe,
  # which then fails with "client returned ERROR on write".
  if curl -fsS "http://127.0.0.1:${PORT}/api/v1/health" >/dev/null 2>&1; then
    echo " — up"
    break
  fi
  echo -n "."
  sleep 1
done
curl -fsS "http://127.0.0.1:${PORT}/api/v1/health" >/dev/null || {
  echo "the server never became healthy; logs:" >&2
  docker logs --tail 20 "$NAME" >&2
  exit 1
}

echo "seeding demo data…"
docker exec -e BASE="http://127.0.0.1:${PORT}" -w /work "$NAME" node scripts/demo-seed.cjs

echo
echo "dashboard: http://127.0.0.1:${PORT}/"
echo "stop with: ./scripts/demo.sh --stop"
