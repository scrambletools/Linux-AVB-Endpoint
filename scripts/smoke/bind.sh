#!/bin/sh
# Smoke test, no root needed: apps' binds end to end. In a user and
# network namespace, two daemons on the two ends of a veth pair, each with
# an entity of a 2-channel ring stream each way, none bound, both showing
# only pinned devices, so neither's controller runs; and a stand-in for
# gptpd giving both one gPTP time. A client of the first's app socket
# binds its ring input to the second's talker, in class B and then in
# class A, and the second's listener to its ring output, which starts the
# first's controller. It checks that media comes each way, that each
# binding reads back and the input's is kept in the configuration, and
# that a missing stream, a format not offered and
# a listener not on the network are refused; then unbinds both, and checks
# that they fall silent and no binding stays in either configuration. No
# PipeWire runs, so nothing can reach the computer's own sound.
#
#   scripts/smoke/bind.sh [target folder]
#
# Needs unshare and ip. Build first, with
# `cargo build -p avb-endpointd --features daemon --bin avb-endpointd
# --examples`.
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
mount -t tmpfs tmpfs /mnt
work=/mnt
mkdir -p "$work/stub" "$work/data/avb-endpointd/models" "$work/config/avb-endpointd" "$work/run"
chmod 700 "$work/run"
printf '#!/bin/sh\nexit 0\n' >"$work/stub/systemctl"
# No real-time threads: RealtimeKit, which avb-stream asks through
# busctl, would raise a stream thread over the computer's own audio.
printf '#!/bin/sh\nexit 1\n' >"$work/stub/busctl"
chmod +x "$work/stub/systemctl" "$work/stub/busctl"
cp "$root"/packaging/linux/models/* "$work/data/avb-endpointd/models/"
mount -t sysfs sysfs /sys
# The stand-in's records, over the host's gptpd folder, here only.
mount -t tmpfs tmpfs /run/gptpd
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
export XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data
export DBUS_SESSION_BUS_ADDRESS=disabled:
export PATH=$work/stub:$PATH

# Entity IDs as the daemon makes them: the address with ff and the
# instance between its halves.
entity() {
    awk -F: -v instance="$2" '{ printf "0x%s%s%sff%02x%s%s%s\n", $1, $2, $3, instance, $4, $5, $6 }' \
        "/sys/class/net/$1/address"
}
room=$(entity v1 1)

# An entity of a ring stream each way, the first `$3` and the second `$4`,
# named `$5` and `$6`, on interface `$1`, its entity named `$2`.
configure() {
    cat >"$work/config/avb-endpointd/$1.toml" <<EOF
destinations = "pinned"

[[endpoint]]
instance = 1
model = "avb-endpointd"
name = "$2"

[[endpoint.stream]]
direction = "$3"
name = "$5"
channels = 2
audio = "ring"

[[endpoint.stream]]
direction = "$4"
name = "$6"
channels = 2
audio = "ring"
EOF
}
configure v0 App out in Say Hear
configure v1 Room in out Speaker Mic

"$target/examples/fake_gptpd" /run/gptpd v0 v1 &
gptpd=$!
"$bin" --interface v0 >"$work/v0.log" 2>&1 &
first=$!
"$bin" --interface v1 >"$work/v1.log" 2>&1 &
second=$!

"$target/examples/ring_bind" "$work/run/avb-endpointd" "$room" "$work/config/avb-endpointd" \
    >"$work/check.log" 2>&1
failed=$?
# Unbound, no binding stays in either configuration.
sleep 1
for side in v0 v1; do
    if grep -q '^bound' "$work/config/avb-endpointd/$side.toml"; then
        echo "FAIL: $side's configuration still has a binding:"
        grep '^bound' "$work/config/avb-endpointd/$side.toml"
        failed=1
    fi
done
for line in "App input 1: bound to $room output 0, for an app" \
    "controller started, for an app's bind" "an app's bind done" "an app's unbind done"; do
    if ! grep -qF "$line" "$work/v0.log"; then
        echo "FAIL: v0's log has no \"$line\""
        failed=1
    fi
done

kill "$first" "$second"
wait "$first"
wait "$second"
kill "$gptpd"
cat "$work/check.log"
if [ "$failed" -ne 0 ]; then
    for log in v0 v1; do
        echo "== $log"
        cat "$work/$log.log"
    done
    exit 1
fi
echo "== v0's log"
grep -E 'app|controller' "$work/v0.log"
echo "ok: an app bound and unbound its streams each way through the daemon, and the bad binds were refused"
