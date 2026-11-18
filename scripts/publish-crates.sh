#!/usr/bin/env bash
# Publish the workspace crates to crates.io, in dependency order.
#
#   CARGO_REGISTRY_TOKEN=… ./scripts/publish-crates.sh
#   ./scripts/publish-crates.sh --dry-run
#   ./scripts/publish-crates.sh --build-ui
#
# Run it through the toolchain container when the host rustc is older than the
# workspace `rust-version` (that is the normal case here):
#
#   ./scripts/dev.sh bash scripts/publish-crates.sh
#
# The token comes from `CARGO_REGISTRY_TOKEN`, or from `.tmp/crates-token`
# (gitignored) when the variable is unset, so it never has to be typed into a
# command line or written to shell history:
#
#   printf %s "$TOKEN" > .tmp/crates-token
#
# Why this exists instead of four `cargo publish` calls:
#
#  * order matters — a crate cannot be published before its dependencies are in
#    the index, and the index is not instantaneous, so each step waits for its
#    own version to show up before the next one starts;
#  * re-running is safe — a version that is already published is skipped, which
#    is what makes a half-finished run resumable;
#  * the dashboard is embedded at package time, so a missing export would be
#    published as the "UI is not embedded in this build" placeholder. The
#    script refuses to run without one.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

dry_run=0
build_ui=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --build-ui) build_ui=1 ;;
    -h | --help)
      sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
if [[ -z "$version" ]]; then
  echo "could not read the workspace version from Cargo.toml" >&2
  exit 1
fi

if [[ -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  if [[ -f .tmp/crates-token ]]; then
    CARGO_REGISTRY_TOKEN="$(tr -d '\r\n' < .tmp/crates-token)"
    export CARGO_REGISTRY_TOKEN
  else
    echo "needs CARGO_REGISTRY_TOKEN, or the token in .tmp/crates-token" >&2
    exit 1
  fi
fi

# Cargo refuses to package a tree with uncommitted changes, and a crate on
# crates.io cannot be unpublished: check before the first upload, not after the
# third.
if [[ $dry_run -eq 0 ]] && ! git diff --quiet; then
  echo "the working tree has uncommitted changes; commit them first" >&2
  exit 1
fi

if [[ $build_ui -eq 1 || ! -f web/dist/index.html ]]; then
  if [[ $build_ui -eq 0 ]]; then
    echo "web/dist has no dashboard export; run \`make ui\` first or pass --build-ui" >&2
    exit 1
  fi
  echo "== building the dashboard"
  make ui
fi

# The BPF objects are checked in; do not make verification depend on clang, and
# do not let it rewrite the sources of the packaged copy.
export LIDLESSEYE_PREBUILT_BPF=1

# Dependency order: core ← ebpf, server ← lidlesseye.
crates=(lidlesseye-core lidlesseye-ebpf lidlesseye-server lidlesseye)

# Sparse index path: `li/dl/lidlesseye-core` for these names, and the same
# two-plus-two split crates.io uses for every crate with four or more characters.
index_url() {
  local name="$1"
  echo "https://index.crates.io/${name:0:2}/${name:2:2}/$name"
}

published() {
  curl -fsS --max-time 10 "$(index_url "$1")" 2>/dev/null | grep -q "\"vers\":\"$version\""
}

wait_for_index() {
  local name="$1" attempt
  for attempt in $(seq 1 60); do
    if published "$name"; then
      return 0
    fi
    sleep 5
  done
  return 1
}

for crate in "${crates[@]}"; do
  echo "== $crate $version"
  if [[ $dry_run -eq 0 ]] && published "$crate"; then
    echo "   already in the index, skipping"
    continue
  fi

  # `--allow-dirty` is not a shortcut around the check above: the dashboard in
  # `ui/` is generated and gitignored, so cargo sees an uncommitted package even
  # when every source file is committed. Changes to *tracked* files are what the
  # `git diff` check at the top is for.
  args=(publish -p "$crate" --locked --allow-dirty)
  if [[ $dry_run -eq 1 ]]; then
    args+=(--dry-run)
  fi
  cargo "${args[@]}"

  if [[ $dry_run -eq 0 ]]; then
    if ! wait_for_index "$crate"; then
      echo "   published, but $version is not in the index yet; re-run later" >&2
      exit 1
    fi
  fi
done

if [[ $dry_run -eq 1 ]]; then
  echo
  echo "dry run finished: every crate packaged and verified"
else
  echo
  echo "published $version:"
  for crate in "${crates[@]}"; do echo "   https://crates.io/crates/$crate"; done
fi
