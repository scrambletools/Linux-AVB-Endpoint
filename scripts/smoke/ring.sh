#!/bin/sh
# Smoke test, no root needed: the app rings end to end. In a user and
# network namespace, two daemons on the two ends of a veth pair, each with
# an entity of a 2-channel ring stream each way, bound to the other's at
# start, and a stand-in for gptpd giving both one gPTP time, so the
# streams carry presentation times. A client queues a counted pattern
# into the first's outbound ring and reads it from the second's inbound
# ring: every word as it went, every block presented when it went out to
# be, a flush silencing the stream from the next block, and the rings
# attached again with the streams going on. No PipeWire runs, so nothing
# can reach the computer's own sound.
#
#   scripts/smoke/ring.sh [target folder]
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
app=$(entity v0 1)
room=$(entity v1 1)
cat >"$work/config/avb-endpointd/v0.toml" <<EOF
[[endpoint]]
instance = 1
model = "avb-endpointd"
name = "App"

[[endpoint.stream]]
direction = "out"
name = "Say"
channels = 2
audio = "ring"

[[endpoint.stream]]
direction = "in"
name = "Hear"
channels = 2
audio = "ring"
bound = "$room:0"
EOF
cat >"$work/config/avb-endpointd/v1.toml" <<EOF
[[endpoint]]
instance = 1
model = "avb-endpointd"
name = "Room"

[[endpoint.stream]]
direction = "in"
name = "Speaker"
channels = 2
audio = "ring"
bound = "$app:0"

[[endpoint.stream]]
direction = "out"
name = "Mic"
channels = 2
audio = "ring"
EOF

"$target/examples/fake_gptpd" /run/gptpd v0 v1 &
gptpd=$!
"$bin" --interface v0 >"$work/v0.log" 2>&1 &
first=$!
"$bin" --interface v1 >"$work/v1.log" 2>&1 &
second=$!

"$target/examples/ring_check" "$work/run/avb-endpointd" >"$work/check.log" 2>&1
failed=$?

kill "$first" "$second"
wait "$first"
wait "$second"
kill "$gptpd"
cat "$work/check.log"
if [ "$failed" -ne 0 ]; then
    for log in v0 v1; do
        echo "== $log"; cat "$work/$log.log"
    done
    exit 1
fi
echo "== v0's log"; grep -E 'ring|connect|bound|format' "$work/v0.log"
echo "ok: the rings carried the pattern word for word, presented as sent, flushed and attached again"
