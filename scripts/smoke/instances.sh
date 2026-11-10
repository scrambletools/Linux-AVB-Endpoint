#!/bin/sh
# Smoke test, no root needed: in a user and network namespace with two
# dummy interfaces, a daemon on each runs at once, each with its own
# file, status and default entity; a second daemon on an interface that
# has one is refused with its own exit status, 69, at once, which the
# user unit does not restart; and one on an interface that is not there
# ends with 68.
#
#   scripts/smoke/instances.sh [path/to/avb-endpointd]
#
# Needs unshare and ip. Build the daemon first, with
# `cargo build -p avb-endpointd --features daemon`.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    bin=${1:-$root/target/debug/avb-endpointd}
    bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    unshare -rnm sh "$0" --inside "$bin" "$work"
    exit $?
fi

bin=$2
work=$3
mkdir -p "$work/stub" "$work/data/avb-endpointd/models" "$work/config" "$work/run"
printf '#!/bin/sh\nexit 0\n' >"$work/stub/systemctl"
chmod +x "$work/stub/systemctl"
cp "$root"/packaging/linux/models/* "$work/data/avb-endpointd/models/"
mount -t sysfs sysfs /sys
for interface in d0 d1; do
    ip link add "$interface" type dummy && ip link set "$interface" up
done
export XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data
export PATH=$work/stub:$PATH

"$bin" --interface d0 >"$work/d0.log" 2>&1 &
first=$!
"$bin" --interface d1 >"$work/d1.log" 2>&1 &
second=$!
sleep 4

failed=0
fail() {
    echo "FAIL: $1"
    failed=1
}
for interface in d0 d1; do
    [ -f "$work/config/avb-endpointd/$interface.toml" ] || fail "no file for $interface"
    grep -q '^default = true' "$work/run/avb-endpointd/$interface.toml" 2>/dev/null \
        || fail "no default entity in $interface's status"
done
d0=$(grep -m1 '^entity_id' "$work/run/avb-endpointd/d0.toml" 2>/dev/null)
d1=$(grep -m1 '^entity_id' "$work/run/avb-endpointd/d1.toml" 2>/dev/null)
[ -n "$d0" ] && [ "$d0" != "$d1" ] || fail "the two default entities share an ID"

started=$(date +%s)
"$bin" --interface d0 >"$work/again.log" 2>&1
again=$?
took=$(( $(date +%s) - started ))
[ "$again" -eq 69 ] || fail "a second daemon on d0 ended with $again, not 69"
[ "$took" -le 2 ] || fail "a second daemon on d0 took ${took}s to give up"

"$bin" --interface d9 >"$work/missing.log" 2>&1
missing=$?
[ "$missing" -eq 68 ] || fail "a daemon on a missing interface ended with $missing, not 68"

kill "$first" "$second"
wait "$first"
wait "$second"
if [ "$failed" -ne 0 ]; then
    for log in d0 d1 again missing; do
        echo "== $log"; cat "$work/$log.log"
    done
    exit 1
fi
echo "ok: two interfaces ran at once; a second daemon was refused (69) and a missing interface ended it (68)"
