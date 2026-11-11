#!/usr/bin/env bash
# Run a command inside the LidlessEye development container.
#
#   ./scripts/dev.sh cargo test --workspace
#   DEV_PRIVILEGED=1 ./scripts/dev.sh cargo run -- serve
#   DEV_PORTS=4040:4040 ./scripts/dev.sh cargo run -- serve --listen 0.0.0.0:4040
#
# Notes:
#  * the cargo registry and the target directory live in docker volumes so
#    incremental builds are fast despite the bind mount,
#  * DEV_PRIVILEGED=1 is required for anything that loads eBPF programs or
#    reads /proc of other processes.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Docker Desktop on Windows wants a native path for the bind mount.
if command -v cygpath >/dev/null 2>&1; then
  HOST_PATH="$(cygpath -w "$REPO_ROOT")"
else
  HOST_PATH="$REPO_ROOT"
fi

ARGS=(run --rm
  -v "${HOST_PATH}:/work"
  -v lidlesseye-cargo:/usr/local/cargo
  -v lidlesseye-target:/build
  -e CARGO_TARGET_DIR=/build
  -e CARGO_TERM_COLOR=always
  -w /work)

if [[ "${DEV_PRIVILEGED:-0}" == "1" ]]; then
  ARGS+=(--privileged --pid=host -v /sys/kernel/debug:/sys/kernel/debug:ro)
fi

if [[ -n "${DEV_ENV:-}" ]]; then
  for kv in $DEV_ENV; do ARGS+=(-e "$kv"); done
fi

# Publish ports, e.g. DEV_PORTS="4040:4040 4317:4317".
if [[ -n "${DEV_PORTS:-}" ]]; then
  for mapping in $DEV_PORTS; do ARGS+=(-p "$mapping"); done
fi

# MSYS/Git Bash would rewrite container paths such as /work into C:\...
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'
exec docker "${ARGS[@]}" lidlesseye-dev "$@"
