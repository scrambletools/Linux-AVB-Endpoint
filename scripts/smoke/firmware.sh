#!/bin/sh
# Smoke test, no root needed: `avb-endpointd firmware` in a user and
# network namespace, uploading images to fake_board boards across a veth
# pair as IEEE 1722.1-2021 Annex D has it, as to an ESP32 board.
#
# - ok: a signed image is kept and the board restarts in it.
# - streams: a board whose streams run refuses, and takes the image
#   locked with --stop-streams.
# - refused: a board that refuses the image says so, as does one given
#   another project's image.
# - rollback: a board back in its old image is said to have rolled back.
# - big: an image larger than the board takes is not sent.
# - unsigned: an image without a signature is sent with a warning.
# - staged: --no-reboot stores the image and leaves the board running.
# - none: a board that takes no firmware is an error.
#
#   scripts/smoke/firmware.sh [target folder]
#
# Needs unshare and ip. Build first, with
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
mount -t tmpfs tmpfs /mnt
work=/mnt
mount -t sysfs sysfs /sys
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
cd "$work"
failed=0
boards=""

fail() {
    echo "FAIL: $*"
    failed=1
}

start_board() {
    "$board" --interface v0 --rate 1 --version 1.0.0 "$@" >/dev/null 2>&1 &
    boards="$boards $!"
}

# Runs `firmware` with the arguments given; its output in $work/$1.out.
upload() {
    name=$1
    shift
    "$bin" firmware "$@" --interface v1 >"$work/$name.out" 2>&1
    status=$?
    echo "== $name: exit $status"
    sed 's/^/   /' "$work/$name.out" | grep -v "bytes sent$"
}

"$board" --make-image "$work/new.bin" --version 1.1.0 --size 100000 --signed 1
"$board" --make-image "$work/unsigned.bin" --version 1.2.0 --size 30000
"$board" --make-image "$work/staged.bin" --version 1.3.0 --size 30000 --signed 1
"$board" --make-image "$work/other.bin" --version 9.9.9 --project other --size 30000 --signed 1

start_board --name fw-board --instance 60 --firmware-max 262144
start_board --name streaming-board --instance 61 --firmware-max 262144 --streams-running
start_board --name strict-board --instance 62 --firmware-max 262144 --reject-images
start_board --name flaky-board --instance 63 --firmware-max 262144 --rollback-after 2
start_board --name small-board --instance 64 --firmware-max 4096
start_board --name plain-board --instance 65
sleep 1.5

upload ok fw-board "$work/new.bin" --settle 3
[ "$status" -eq 0 ] && grep -q "now runs 1.1.0 (fake - " "$work/ok.out" || fail "ok"
grep -q "signed with one key" "$work/ok.out" || fail "ok: signature not seen"

upload streams streaming-board "$work/new.bin" --settle 1
[ "$status" -ne 0 ] && grep -q -- "--stop-streams" "$work/streams.out" || fail "streams: not refused"
upload stop streaming-board "$work/new.bin" --stop-streams --settle 1
[ "$status" -eq 0 ] && grep -q "now runs 1.1.0" "$work/stop.out" || fail "stop"

upload refused strict-board "$work/new.bin"
[ "$status" -ne 0 ] && grep -q "refused the image" "$work/refused.out" || fail "refused"
upload project fw-board "$work/other.bin"
[ "$status" -ne 0 ] && grep -q "refused the image" "$work/project.out" || fail "project"

upload rollback flaky-board "$work/new.bin" --settle 6
[ "$status" -ne 0 ] && grep -q "rolled back to 1.0.0" "$work/rollback.out" || fail "rollback"

upload big small-board "$work/new.bin"
[ "$status" -ne 0 ] && grep -q "takes 4096 at most" "$work/big.out" || fail "big"

upload unsigned fw-board "$work/unsigned.bin" --settle 0
[ "$status" -eq 0 ] && grep -q "is not signed" "$work/unsigned.out" \
    && grep -q "now runs 1.2.0" "$work/unsigned.out" || fail "unsigned"

upload staged fw-board "$work/staged.bin" --no-reboot
[ "$status" -eq 0 ] && grep -q "image stored; it runs from" "$work/staged.out" || fail "staged"
upload after fw-board "$work/unsigned.bin" --settle 0
grep -q "fw-board runs 1.2.0" "$work/after.out" || fail "staged: the board restarted"

upload none plain-board "$work/new.bin"
[ "$status" -ne 0 ] && grep -q "takes no firmware" "$work/none.out" || fail "none"

for pid in $boards; do
    kill -TERM "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
done
if [ "$failed" -ne 0 ]; then
    echo "firmware smoke test FAILED"
    exit 1
fi
echo "firmware smoke test passed"
