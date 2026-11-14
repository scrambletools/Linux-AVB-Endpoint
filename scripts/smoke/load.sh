#!/bin/sh
# Smoke test, no root needed: load_talker in a user and network
# namespace, on one end of a veth pair, with a stand-in for gptpd, and
# load-capture.py on the other end checking every frame: the streams'
# IDs, PCP 3 on VLAN 2, AAF of 8 channels with presentation times,
# every frame sent arriving, and the destinations.
#
# - MAAP: two streams on one claimed block, announced. The block the
#   claim tries first is announced from the far end only while
#   load_talker listens, which it must remember and give way to.
# - A fixed unicast destination: no MAAP.
# - A multicast destination: refused.
# Each run stops on SIGTERM, within a second. No real time: RealtimeKit
# would raise threads over the computer's own audio.
#
#   scripts/smoke/load.sh [target folder]
#
# Needs unshare, ip and python3. Build first, with
# `cargo build -p avb-endpointd --features daemon --examples`.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    target=$(cd "${1:-$root/target/debug}" && pwd)
    unshare -rnm sh "$0" --inside "$target"
    exit $?
fi

target=$2
load=$target/examples/load_talker
capture="python3 -I $here/load-capture.py"
mount -t tmpfs tmpfs /mnt
work=/mnt
mkdir -p "$work/stub"
printf '#!/bin/sh\nexit 1\n' >"$work/stub/busctl"
chmod +x "$work/stub/busctl"
export PATH=$work/stub:$PATH
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /run/gptpd
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
talker=$(cat /sys/class/net/v0/address)
far=$(cat /sys/class/net/v1/address)
"$target/examples/fake_gptpd" /run/gptpd v0 v1 &
gptpd=$!
failed=0

# Runs load_talker with the arguments given for $run seconds of sending
# after its MAAP listening, then stops it, saying in its output how long
# the stop took. The capture ran before and goes on after.
run_load() {
    name=$1
    shift
    $capture capture v1 "$work/$name.json" $held &
    catcher=$!
    sleep 0.5
    "$load" v0 "$@" >"$work/$name.out" 2>&1 &
    pid=$!
    sleep "$run"
    asked=$(date +%s%N)
    kill -TERM "$pid"
    wait "$pid"
    status=$?
    took=$((($(date +%s%N) - asked) / 1000000))
    sleep 0.3
    kill -TERM "$catcher"
    wait "$catcher"
    echo "== $name: exit $status, stopped in $took ms"
    cat "$work/$name.out"
    if [ "$status" -ne 0 ] || [ "$took" -ge 1000 ]; then
        echo "FAIL: $name did not stop cleanly within a second"
        failed=1
    fi
}

first=$($capture first "$talker" 2)
held="$first 2 1"
run=9
run_load maap --streams 2 --listen 2
$capture check "$work/maap.json" "$work/maap.out" 2 "$talker" maap "$first" 2 || failed=1
grep -q "is in .* block of 2 from" "$work/maap.out" ||
    { echo "FAIL: the block heard held was not given up"; failed=1; }

held=""
run=3
run_load unicast --dest "$far"
$capture check "$work/unicast.json" "$work/unicast.out" 1 "$talker" "$far" || failed=1

"$load" v0 --dest 91:e0:f0:00:77:40 >"$work/multicast.out" 2>&1
status=$?
echo "== multicast: exit $status"
cat "$work/multicast.out"
if [ "$status" -ne 2 ]; then
    echo "FAIL: a multicast destination was taken"
    failed=1
fi

kill "$gptpd"
if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "ok: load_talker's streams arrived whole, tagged, on a MAAP block clear of one held or a unicast address, and stopped within a second"
