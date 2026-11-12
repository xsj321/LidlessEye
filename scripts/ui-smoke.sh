#!/usr/bin/env bash
# Run the dashboard smoke test in a real browser.
#
#   ./scripts/ui-smoke.sh                 # headless, screenshots in .tmp/ui
#   SHOT_DIR= ./scripts/ui-smoke.sh       # no screenshots
#
# What it does:
#   1. builds the debug server binary (the UI is embedded at compile time, so
#      this is also what makes the test cover the current web/dist),
#   2. starts it on 127.0.0.1:4040 inside the container,
#   3. seeds two tenants over HTTP,
#   4. drives the dashboard with Chromium and fails on any console error.
#
# Requires Docker only: no local Rust, node or browser.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=lidlesseye-ui-test

# MSYS/Git Bash would rewrite container paths such as /work into C://...
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*' 

# Docker Desktop on Windows wants a native path for the bind mount.
if command -v cygpath >/dev/null 2>&1; then
  HOST_PATH="$(cygpath -w "$REPO_ROOT")"
else
  HOST_PATH="$REPO_ROOT"
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "building $IMAGE (one time)…"
  docker build -q -t "$IMAGE" -f "$REPO_ROOT/.devcontainer/Dockerfile.ui" "$REPO_ROOT/.devcontainer" >/dev/null
fi

echo "building the server binary…"
docker run --rm \
  --shm-size=1g \
  -v "${HOST_PATH}:/work" \
  -v lidlesseye-target:/build \
  -e CARGO_TARGET_DIR=/build \
  -w /work \
  lidlesseye-dev cargo build -p lidlesseye --quiet

SHOTS="${SHOT_DIR-/work/.tmp/ui}"
mkdir -p "$REPO_ROOT/.tmp/ui"

# MSYS/Git Bash would rewrite container paths such as /work into C:\...
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

docker run --rm \
  --shm-size=1g \
  -v "${HOST_PATH}:/work" \
  -v lidlesseye-target:/build:ro \
  -e LIDLESSEYE_BASE=http://127.0.0.1:4040 \
  -e SHOT_DIR="$SHOTS" \
  -w /work \
  "$IMAGE" \
  bash -c '
    set -u
    /build/debug/lidlesseye serve --data-dir /tmp/le-ui --listen 127.0.0.1:4040 >/tmp/server.log 2>&1 &
    server=$!
    trap "kill $server 2>/dev/null || true" EXIT

    node web/test/smoke.cjs
    status=$?

    echo "--- server log (tail) ---"
    tail -20 /tmp/server.log
    exit $status
  '
