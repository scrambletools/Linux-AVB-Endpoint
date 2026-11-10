#!/bin/sh
# Smoke test, no root needed: in a user and network namespace, two
# daemons on the two ends of a veth pair, each another computer to the
# other. Each finds the other's default entity and lists its streams as
# network devices: its Listen input as a destination with a device here,
# its Playback output as a source without one until pinned. Neither lists
# its own. Then the tool pins the source, and hides the destination.
#
#   scripts/smoke/devices.sh [path/to/avb-endpointd]
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
# No real-time threads: RealtimeKit, which avb-stream asks through
# busctl, would raise a stream thread over the computer's own audio.
printf '#!/bin/sh\nexit 1\n' >"$work/stub/busctl"
chmod +x "$work/stub/systemctl" "$work/stub/busctl"
cp "$root"/packaging/linux/models/* "$work/data/avb-endpointd/models/"
mount -t sysfs sysfs /sys
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
export XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data
export PATH=$work/stub:$PATH

"$bin" --interface v0 >"$work/v0.log" 2>&1 &
first=$!
"$bin" --interface v1 >"$work/v1.log" 2>&1 &
second=$!

failed=0
fail() {
    echo "FAIL: $1"
    failed=1
}
status() {
    cat "$work/run/avb-endpointd/$1.toml" 2>/dev/null
}
# The other's default entity, as each one's own status names it.
own() {
    status "$1" | grep -m1 '^entity_id' | sed 's/.*"\(0x[0-9a-f]*\)".*/\1/'
}
# Each [[device]] table of a status on one line.
devices() {
    status "$1" | awk '/^\[\[device\]\]/ { if (line) print line; line = ""; inside = 1; next }
        /^\[/ { if (line) print line; line = ""; inside = 0; next }
        inside { line = line " " $0 } END { if (line) print line }'
}
waited=0
until devices v0 | grep -q 'way = "destination"' || [ "$waited" -ge 20 ]; do
    sleep 1
    waited=$((waited + 1))
done
other=$(own v1)
mine=$(own v0)
[ -n "$other" ] || fail "no default entity on v1"
destination=$(devices v0 | grep "\"$other\"" | grep 'way = "destination"')
source=$(devices v0 | grep "\"$other\"" | grep 'way = "source"')
[ -n "$destination" ] || fail "v0 lists no destination of v1's entity"
printf '%s\n' "$destination" | grep -q 'shown = true' || fail "v1's input has no device on v0"
printf '%s\n' "$destination" | grep -q "node = \"avb-dest-${other#0x}-in-0\"" \
    || fail "v1's input's device has another node name"
[ -n "$source" ] || fail "v0 lists no source of v1's entity"
printf '%s\n' "$source" | grep -q 'shown = true' && fail "v1's output has a device before it is pinned"
devices v0 | grep -q "\"$mine\"" && fail "v0 lists its own entity"

"$bin" pin "$other:out0" --interface v0 >"$work/pin.log" 2>&1 || fail "the tool could not pin"
"$bin" hide "$other:in0" --interface v0 >"$work/hide.log" 2>&1 || fail "the tool could not hide"
sleep 4
source=$(devices v0 | grep "\"$other\"" | grep 'way = "source"')
destination=$(devices v0 | grep "\"$other\"" | grep 'way = "destination"')
printf '%s\n' "$source" | grep -q 'pinned = true' || fail "the pinned output is not pinned"
printf '%s\n' "$source" | grep -q 'shown = true' || fail "the pinned output has no device"
printf '%s\n' "$destination" | grep -q 'hidden = true' || fail "the hidden input is not hidden"
printf '%s\n' "$destination" | grep -q 'shown = true' && fail "the hidden input still has a device"
grep -q 'changed, starting again' "$work/v0.log" && fail "pinning started the daemon again"
"$bin" list --interface v0 >"$work/list.log" 2>&1 || fail "the tool could not list"

kill "$first" "$second"
wait "$first"
wait "$second"
if [ "$failed" -ne 0 ]; then
    echo "== v0 status"; status v0
    for log in v0 v1 pin hide list; do
        echo "== $log"; cat "$work/$log.log"
    done
    exit 1
fi
echo "== list"; cat "$work/list.log"
echo "ok: each found the other's streams, not its own; the tool pinned a source and hid a destination"
