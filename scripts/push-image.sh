#!/usr/bin/env bash
# Push an image and prove it landed.
#
# `docker push ... | tail -1` looks like success even when every blob failed:
# the CLI exits 0 on some transport errors and the last line is a progress line,
# not a verdict. A cluster then reports ImagePullBackOff for a tag whose name is
# right there in the chart, which is an expensive way to learn that the push
# never happened.
#
# So: retry the push, then ask the registry for the manifest. Only the manifest
# lookup proves the tag is real, because that is exactly what kubelet does.
#
#   scripts/push-image.sh xsj321/lidlesseye:0.10.47-symdiag
#   scripts/push-image.sh docker.io/xsj321/lidlesseye:0.10.47-symdiag --save out.tar
#
# --save also writes `docker save` output, for the clusters that cannot reach a
# registry at all.
set -uo pipefail

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage
image=$1
shift
save_to=""
while [ $# -gt 0 ]; do
    case $1 in
        --save)
            save_to=${2:?--save needs a path}
            shift 2
            ;;
        *) usage ;;
    esac
done

# A bare `name:tag` is `docker.io/library/name:tag` to the daemon but
# `docker.io/name:tag` to a Helm chart, and the mismatch is a pull failure.
case $image in
    */*:*) ;;
    *) image="docker.io/$image" ;;
esac
case $image in
    *:*|*/*@*) ;;
    *) printf 'push-image: %s has no tag\n' "$image" >&2; exit 2 ;;
esac

say() { printf '%s\n' "$*"; }

say "==> pushing $image"
pushed=0
for attempt in 1 2 3 4 5; do
    if docker push "$image" 2>&1 | grep -qE 'digest: sha256:'; then
        pushed=1
        break
    fi
    [ "$attempt" -lt 5 ] || break
    say "    attempt $attempt failed, retrying in $((attempt * 10))s"
    sleep $((attempt * 10))
done
if [ "$pushed" -ne 1 ]; then
    say "    push never reported a digest; the registry did not accept the image"
    say "    a cluster cannot pull it — use --save and import it into the node"
    verify="${image#docker.io/}"
    say "    check: docker manifest inspect $verify"
fi

# The manifest is the only proof kubelet will find the tag.
if docker manifest inspect "${image#docker.io/}" >/dev/null 2>&1; then
    say "==> verified: ${image#docker.io/} resolves on the registry"
    docker manifest inspect "${image#docker.io/}" 2>/dev/null \
        | grep -oE '"digest": "sha256:[a-f0-9]+"' | head -1 || true
else
    say "==> NOT verified: the registry has no manifest for ${image#docker.io/}"
    say "    (offline, behind a proxy that is down, or the push was rejected)"
    exit 1
fi

if [ -n "$save_to" ]; then
    say "==> saving to $save_to"
    docker save "$image" -o "$save_to" || exit 1
    say "    $(du -h "$save_to" | cut -f1) — import on the node, see docs/deployment.md"
fi
