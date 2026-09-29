#!/usr/bin/env bash
# Cross-user access fixture (DESIGN.md §7.1, §7.3, §7.4). Container-only: root
# arranges UIDs, directories and modes; every cued process and probe runs as a
# real unprivileged UID with no capabilities. Each UID takes a turn as owner.
set -euo pipefail

[[ ${CUED_CONTAINER_FIXTURE:-} == 1 && $(id -u) == 0 ]] || {
    echo 'Refusing: run via scripts/security/run.sh, never directly on the host.' >&2
    exit 1
}
[[ -x /app/cued && -d /fixture && ! -e /fixture/initialized ]] || exit 1
touch /fixture/initialized

# A common default: without it the private modes below would pass by accident.
umask 022
install -d -m 0755 /run/user

pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    wait || true
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# as_user UID HOME [VAR=VALUE...] -- CMD...
as_user() {
    local uid=$1 home=$2
    shift 2
    local extra=()
    while [[ $1 != -- ]]; do extra+=("$1"); shift; done
    shift
    setpriv --reuid="$uid" --regid="$uid" --clear-groups \
        --inh-caps=-all --ambient-caps=-all --bounding-set=-all --no-new-privs \
        env -i PATH=/usr/bin:/bin HOME="$home" TZ=UTC \
        DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent "${extra[@]}" "$@"
}

probe() { local uid=$1; shift; as_user "$uid" /nonexistent -- python3 /checks/probe.py "$@"; }

mode_of() { stat -c %a "$1"; }
private() { (( (8#$(mode_of "$1") & 8#077) == 0 )); }

# start_daemon UID HOME LOG [VAR=VALUE...] — foreground, so shutdown is ours.
start_daemon() {
    local uid=$1 home=$2 log=$3
    shift 3
    ( exec setpriv --reuid="$uid" --regid="$uid" --clear-groups \
        --inh-caps=-all --ambient-caps=-all --bounding-set=-all --no-new-privs \
        env -i PATH=/usr/bin:/bin HOME="$home" TZ=UTC \
        DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent "$@" /app/cued daemon --foreground
    ) >"$log" 2>&1 &
    pids+=("$!")
}

wait_for_socket() {
    local socket=$1 log=$2
    for _ in {1..200}; do
        [[ -S $socket ]] && return 0
        kill -0 "${pids[-1]}" 2>/dev/null || { cat "$log"; fail "daemon exited before binding $socket"; }
        sleep 0.05
    done
    cat "$log"
    fail "daemon did not bind $socket"
}

stop_daemon() {
    kill -TERM "${pids[-1]}"
    wait "${pids[-1]}" || true
    unset 'pids[-1]'
}

for owner in 10001 10002; do
    other=$(( owner == 10001 ? 10002 : 10001 ))
    home=/fixture/home-$owner
    other_home=/fixture/home-$other
    data=$home/.local/share/cued
    runtime=/run/user/$owner
    log=/fixture/daemon-$owner.log
    echo "--- owner uid $owner, foreign uid $other"

    # Homes are world-traversable, as they are on many distributions: cued's
    # own directories must be what keeps the other user out, not the home.
    rm -rf "$home" "$other_home" "$runtime"
    install -d -m 0755 -o "$owner" -g "$owner" "$home"
    install -d -m 0755 -o "$other" -g "$other" "$other_home"

    # 0. A store directory cued did not create: another user's is refused, and
    #    the owner's own, left open to everyone, is taken private before use.
    install -d -m 0755 -o "$owner" -g "$owner" "$home/.local" "$home/.local/share"
    install -d -m 0700 -o "$other" -g "$other" "$data"
    out=$(as_user "$owner" "$home" -- /app/cued list 2>&1) \
        && fail "uid $owner used a store directory owned by uid $other: $out"
    grep -q 'must be a directory owned by' <<<"$out" || fail "unexpected refusal: $out"
    [[ -z $(ls -A "$data") ]] || fail "uid $owner wrote into uid $other's store directory"
    rm -rf "$data"
    install -d -m 0755 -o "$owner" -g "$owner" "$data"
    pass "a store directory owned by uid $other is refused"

    # 1. A runtime directory that is not the owner's is never used for the
    #    socket: the daemon falls back to its private directory in home.
    install -d -m 0700 -o "$other" -g "$other" "$runtime"
    start_daemon "$owner" "$home" "$log"
    wait_for_socket "$data/run/cued.sock" "$log"
    [[ -z $(ls -A "$runtime") ]] || fail "daemon wrote into $runtime, owned by uid $other"
    stop_daemon
    pass "a /run/user/$owner owned by uid $other is ignored; the socket stays in home"

    # 2. The owner's real runtime directory is used; the rest runs against it.
    rm -rf "$runtime"
    install -d -m 0700 -o "$owner" -g "$owner" "$runtime"
    socket=$runtime/cued.sock
    start_daemon "$owner" "$home" "$log"
    wait_for_socket "$socket" "$log"

    # 3. The owner schedules work; the command runs as the owner, no one else.
    # shellcheck disable=SC2016 # $HOME expands in the job, as the owner
    as_user "$owner" "$home" -- /app/cued at "in 1s" -- /bin/sh -c 'id -u > "$HOME/ran-as"' >/dev/null
    as_user "$owner" "$home" -- /app/cued at "tomorrow 9am" -- /bin/true >/dev/null
    for _ in {1..200}; do [[ -s $home/ran-as ]] && break; sleep 0.05; done
    [[ $(cat "$home/ran-as" 2>/dev/null) == "$owner" ]] || fail "job did not run as uid $owner"
    pass "the owner's job ran as uid $owner"
    probe "$owner" allowed "$socket" j2

    # 4. Everything cued created is private, whatever the umask.
    for path in "$data" "$data/logs" "$data/run" "$runtime" "$data/cued.db" "$data"/cued.db-*; do
        [[ -e $path ]] || continue
        private "$path" || fail "$path is mode $(mode_of "$path"), readable beyond uid $owner"
    done
    # The store directory was created 0755 in step 0; it must have been
    # tightened, not trusted. Everything under logs/ is checked, directories too.
    while IFS= read -r -d '' path; do
        private "$path" || fail "$path is mode $(mode_of "$path"), readable beyond uid $owner"
    done < <(find "$data/logs" -mindepth 1 -print0)
    [[ -n $(find "$data/logs" -type f -name '*.log') ]] || fail "no step log was written to check"
    pass "store, logs and socket directories are private to uid $owner"

    # 5. The other user is refused by the filesystem at every entry point.
    probe "$other" fs-denied "$socket"
    probe "$other" unreadable "$data" "$data/cued.db" "$data/logs"
    out=$(as_user "$other" "$other_home" CUED_SOCKET_DIR="$runtime" -- /app/cued list 2>&1) \
        && fail "uid $other's CLI accepted uid $owner's socket directory: $out"
    grep -q 'must be an owned directory' <<<"$out" || fail "unexpected refusal: $out"
    pass "uid $other's own CLI refuses uid $owner's socket directory"

    # 6. Take the filesystem barrier away, in this container only: the daemon's
    #    kernel peer-credential check must refuse the other user on its own.
    chmod 0755 "$runtime"
    chmod 0777 "$socket"
    probe "$other" uid-denied "$socket" j2
    probe "$owner" allowed "$socket" j2
    chmod 0700 "$runtime"
    chmod 0755 "$socket"

    # 7. Nothing the other user sent took effect.
    status=$(as_user "$owner" "$home" -- /app/cued list --json |
        python3 -c 'import json,sys; print({e["id"]: e["status"] for e in json.load(sys.stdin)}.get(2))')
    [[ $status == active ]] || fail "j2 is '$status' after the foreign cancel; expected active"
    pass "j2 is still active after uid $other's cancel attempt"

    # 8. The owner's client will not talk to a daemon run by the other user,
    #    even one whose socket is left open to everyone.
    squat=/fixture/squat-$other
    install -d -m 0700 -o "$other" -g "$other" "$squat"
    start_daemon "$other" "$other_home" "/fixture/daemon-$other.log" CUED_SOCKET_DIR="$squat"
    wait_for_socket "$squat/cued.sock" "/fixture/daemon-$other.log"
    chmod 0755 "$squat"
    chmod 0777 "$squat/cued.sock"
    out=$(as_user "$owner" "$home" CUED_SOCKET_DIR="$squat" -- /app/cued list 2>&1) \
        && fail "uid $owner's CLI used uid $other's daemon: $out"
    grep -q 'must be an owned directory' <<<"$out" || fail "unexpected refusal: $out"
    probe "$owner" uid-denied "$squat/cued.sock" j1
    stop_daemon
    rm -rf "$squat"
    pass "uid $owner's CLI refuses a daemon run by uid $other"

    as_user "$owner" "$home" -- /app/cued list >/dev/null
    stop_daemon
    rm -rf "$runtime"
done

echo 'PASS: both UIDs served as owner and were refused as the foreign user; no case skipped'
