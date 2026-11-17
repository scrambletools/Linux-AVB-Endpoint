#!/bin/sh
# Smoke test, no root needed: `avb-endpointd crash` in a user and network
# namespace, reading fake_board's crash dump across a veth pair, as it
# reads an ESP32 board's: a CRASH_DUMP memory object, through Address
# Access, erased with START_OPERATION.
#
# - read: by name, the whole dump, octet for octet.
# - erase: by entity ID, read again and erased, after which there is
#   none.
# - empty: a board with an empty crash dump has none to save.
# - none: a board with no CRASH_DUMP object is an error.
# - missing: a device nobody has is an error, after the look.
#
#   scripts/smoke/crash.sh [target folder]
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

"$board" --interface v0 --name crash-board --instance 52 --rate 1 \
    --crash-dump 200000 --crash-file "$work/dump.bin" >/dev/null 2>&1 &
boards="$boards $!"
"$board" --interface v0 --name empty-board --instance 53 --rate 1 \
    --crash-max 4096 >/dev/null 2>&1 &
boards="$boards $!"
"$board" --interface v0 --name plain-board --instance 54 --rate 1 >/dev/null 2>&1 &
boards="$boards $!"
sleep 1.5

# read
started=$(date +%s%N)
"$bin" crash crash-board --interface v1 -o "$work/got.core" >"$work/read.out" 2>"$work/read.err"
status=$?
took=$((($(date +%s%N) - started) / 1000000))
echo "== read: exit $status in $took ms"
cat "$work/read.out" "$work/read.err"
[ "$status" -eq 0 ] || fail "read exited $status"
cmp "$work/dump.bin" "$work/got.core" || fail "read: the dump differs"
grep -q "esp-coredump info_corefile --core $work/got.core" "$work/read.out" || fail "read: no decode line"

# erase
entity=$(awk -F: '{ printf "0x%s%s%sff34%s%s%s\n", $1, $2, $3, $4, $5, $6 }' /sys/class/net/v0/address)
"$bin" crash "$entity" --interface v1 -o "$work/again.core" --erase >"$work/erase.out" 2>"$work/erase.err"
status=$?
echo "== erase: exit $status"
cat "$work/erase.out" "$work/erase.err"
[ "$status" -eq 0 ] || fail "erase exited $status"
cmp "$work/dump.bin" "$work/again.core" || fail "erase: the dump read before differs"
grep -q "crash dump erased" "$work/erase.out" || fail "erase: not said erased"
"$bin" crash crash-board --interface v1 -o "$work/gone.core" >"$work/after.out" 2>&1
status=$?
echo "== after: exit $status: $(cat "$work/after.out")"
[ "$status" -eq 0 ] && grep -q "no crash dump" "$work/after.out" || fail "after: not empty"
[ -e "$work/gone.core" ] && fail "after: a file written for no dump"

# empty
"$bin" crash empty-board --interface v1 >"$work/empty.out" 2>&1
status=$?
echo "== empty: exit $status: $(cat "$work/empty.out")"
[ "$status" -eq 0 ] && grep -q "no crash dump" "$work/empty.out" || fail "empty"

# none
"$bin" crash plain-board --interface v1 >"$work/none.out" 2>&1
status=$?
echo "== none: exit $status: $(cat "$work/none.out")"
[ "$status" -ne 0 ] && grep -q "keeps no crash dump" "$work/none.out" || fail "none"

# missing
"$bin" crash nobody --interface v1 >"$work/missing.out" 2>&1
status=$?
echo "== missing: exit $status: $(cat "$work/missing.out")"
[ "$status" -ne 0 ] && grep -q "no device nobody found" "$work/missing.out" || fail "missing"

for pid in $boards; do
    kill -TERM "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
done
if [ "$failed" -ne 0 ]; then
    echo "crash smoke test FAILED"
    exit 1
fi
echo "crash smoke test passed"
