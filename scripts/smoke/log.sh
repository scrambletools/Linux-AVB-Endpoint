#!/bin/sh
# Smoke test, no root needed: `avb-endpointd log` in a user and network
# namespace, reading fake_board's log across a veth pair, as it reads an
# ESP32 board's: a LOG_OBJECT memory object, through Address Access.
# log-check.py compares what was read with what the board wrote.
#
# - follow: a boot burst, then lines, followed with --stats and
#   --stats-json; every line read, in order, none lost, no retries; the
#   boot said first, with why it began.
# - once: without --follow it reads what the log holds and stops.
# - lost: a small ring read slowly loses octets, says so, and every
#   line it prints is still one the board wrote.
# - restart: a new boot is marked and its lines follow.
# - dropped: lines the board dropped are counted.
# - two: two boards followed at once, each line under its board's name.
# - missing: a device nobody has is an error, after the look.
#
#   scripts/smoke/log.sh [target folder]
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
bin=$target/avb-endpointd
board=$target/examples/fake_board
check="python3 -I $here/log-check.py"
mount -t tmpfs tmpfs /mnt
work=/mnt
mount -t sysfs sysfs /sys
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
failed=0

# Runs a board on v0 with the arguments given, its lines in $work/$1.board.
start_board() {
    name=$1
    shift
    "$board" --interface v0 "$@" >"$work/$name.board" 2>"$work/$name.board-err" &
    boards="${boards:-} $!"
    sleep 1.5
}

stop_boards() {
    for pid in $boards; do
        kill -TERM "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    done
    boards=""
}

# Follows for $2 seconds with the arguments after, then stops it as ^C
# would; its output in $work/$1.read, its errors in $work/$1.err.
follow_for() {
    name=$1
    seconds=$2
    shift 2
    "$bin" log "$@" --interface v1 >"$work/$name.read" 2>"$work/$name.err" &
    pid=$!
    sleep "$seconds"
    kill -INT "$pid"
    wait "$pid"
    echo "== $name: exit $?"
}

verdict() {
    if "$@"; then
        :
    else
        failed=1
    fi
}

# follow
start_board follow --name fake-board --burst 6000 --rate 20 --line 60 --reset-reason 4 --resets 3
follow_for follow 8 fake-board --follow --stats --stats-json "$work/follow.json"
stop_boards
cat "$work/follow.err"
cat "$work/follow.json"
verdict $check follow "$work/follow.board" "$work/follow.read" "$work/follow.json" 200
grep -q "^\[fake-board: boot [0-9a-f]\{8\}, up [0-9]* s, reset PANIC (4), 3 unexpected resets\]$" "$work/follow.read" \
    || { echo "FAIL follow: no boot line with its reset"; failed=1; }
grep -q '"reset_reason":4,"resets":3' "$work/follow.json" || { echo "FAIL follow: reset not in the stats"; failed=1; }

# once
start_board once --name fake-board --instance 49 --burst 3000 --rate 2
entity=$(awk -F: '{ printf "0x%s%s%sff31%s%s%s\n", $1, $2, $3, $4, $5, $6 }' /sys/class/net/v0/address)
started=$(date +%s)
timeout 15 "$bin" log "$entity" --interface v1 >"$work/once.read" 2>"$work/once.err"
status=$?
took=$(($(date +%s) - started))
stop_boards
echo "== once: exit $status after ${took} s"
cat "$work/once.err"
if [ "$status" -ne 0 ] || [ "$took" -gt 5 ]; then
    echo "FAIL: once did not read the log and stop by itself"
    failed=1
fi
verdict $check once "$work/once.board" "$work/once.read" - 50
grep -q "^\[fake-board: boot [0-9a-f]\{8\}, up [0-9]* s, reset reason not given\]$" "$work/once.read" \
    || { echo "FAIL once: no boot line"; failed=1; }

# lost
start_board lost --name fake-board --ring 512 --rate 200 --line 80
follow_for lost 5 fake-board --follow --poll 500 --stats
stop_boards
cat "$work/lost.err"
verdict $check lost "$work/lost.board" "$work/lost.read" - 5

# restart
start_board restart --name fake-board --rate 10 --restart-after 4 --reset-reason 1 --resets 0
follow_for restart 6 fake-board --follow
stop_boards
verdict $check restart "$work/restart.board" "$work/restart.read" - 20
grep -q "reset SW (3), 0 unexpected resets" "$work/restart.read" || { echo "FAIL restart: the new boot's reset"; failed=1; }

# dropped
start_board dropped --name fake-board --rate 20 --drop-every 5
follow_for dropped 4 fake-board --follow
stop_boards
verdict $check dropped "$work/dropped.board" "$work/dropped.read" - 20

# two
start_board two-a --name board-a --instance 50 --rate 10
start_board two-b --name board-b --instance 51 --rate 10
follow_for two 4 board-a board-b --follow
stop_boards
verdict $check two "$work/two-a.board" "$work/two.read" - 10 board-a
verdict $check two "$work/two-b.board" "$work/two.read" - 10 board-b

# missing
started=$(date +%s)
"$bin" log nobody --interface v1 >"$work/missing.read" 2>"$work/missing.err"
status=$?
took=$(($(date +%s) - started))
echo "== missing: exit $status after ${took} s: $(cat "$work/missing.err")"
if [ "$status" -eq 0 ] || ! grep -q "no device nobody found" "$work/missing.err"; then
    echo "FAIL: missing"
    failed=1
fi

if [ "$failed" -ne 0 ]; then
    echo "log smoke test FAILED"
    exit 1
fi
echo "log smoke test passed"
