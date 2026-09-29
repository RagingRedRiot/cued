#!/usr/bin/env bash
# Run the cross-user access fixture (users.sh) against a freshly built cued.
#
#   cargo build --locked && scripts/security/run.sh target/debug/cued
#
# Docker is required: a missing engine or a failed build is a failure, never a
# skip. Nothing is installed on the host and no host users are created.
set -euo pipefail
[[ $# == 1 ]] || { echo "Usage: $0 PATH/TO/cued" >&2; exit 2; }
binary=$(realpath -- "$1")
[[ -x $binary ]] || { echo "Not an executable: $binary" >&2; exit 1; }
context=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
docker info >/dev/null

scratch=$(mktemp -d)
cleanup() {
    if [[ -s $scratch/cid ]]; then docker rm -f "$(cat "$scratch/cid")" >/dev/null 2>&1 || true; fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT
docker build --quiet --iidfile "$scratch/image" "$context" >/dev/null

# No privileged mode, network, Docker socket, secrets, or writable host mounts.
# The capabilities are for root's fixture setup only; setpriv drops every one
# of them before cued or a probe runs.
timeout --signal=TERM --kill-after=10s 180s docker run --rm --init \
    --cidfile "$scratch/cid" --network none --read-only \
    --cap-drop ALL --cap-add DAC_OVERRIDE --cap-add CHOWN --cap-add FOWNER \
    --cap-add SETUID --cap-add SETGID --cap-add SETPCAP --cap-add KILL \
    --security-opt no-new-privileges --pids-limit 256 --memory 512m --cpus 2 \
    --tmpfs /fixture:rw,nosuid,nodev,noexec,mode=0755,size=64m \
    --tmpfs /run:rw,nosuid,nodev,noexec,mode=0755,size=4m \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777,size=16m \
    --mount "type=bind,src=$binary,dst=/app/cued,readonly" \
    --env CUED_CONTAINER_FIXTURE=1 "$(cat "$scratch/image")"
