#!/bin/sh
# A simulated room for an app's tests, no root needed and no hardware:
# in a user and network namespace, two daemons on the two ends of a veth
# pair and a stand-in for gptpd giving both one gPTP time. The first runs
# the app's entities from its configuration; the second runs "Room", a
# 2-channel ring stream each way, its input bound to the app entity's
# first output and playing a WAV file into its output. The app's command
# runs in the namespace, where `App::connect("v0")` reaches the first
# daemon's rings. What the room hears is recorded.
#
#   scripts/smoke/room.sh <app.toml> <play.wav> <record.wav> -- <command>...
#
# In app.toml, `@ROOM@` stands for the Room entity's ID, as in
# `bound = "@ROOM@:0"` on the app entity's input. The app's entity is the
# file's first; its first output streams to the room. No PipeWire runs,
# and the namespace's runtime folder hides the computer's own, so nothing
# here reaches the computer's sound. Build first, with `cargo build -p
# avb-endpointd --features daemon --bin avb-endpointd --examples`; set
# TARGET to another build folder than target/debug.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    if [ $# -lt 5 ] || [ "$4" != "--" ]; then
        echo "usage: $0 <app.toml> <play.wav> <record.wav> -- <command>..." >&2
        exit 2
    fi
    target=$(cd "${TARGET:-$root/target/debug}" && pwd)
    absolute() { (cd "$(dirname "$1")" && echo "$(pwd)/$(basename "$1")"); }
    app=$(absolute "$1")
    play=$(absolute "$2")
    record=$(absolute "$3")
    shift 4
    unshare -rnm sh "$0" --inside "$target" "$app" "$play" "$record" "$@"
    exit $?
fi

target=$2
app_config=$3
play=$4
record=$5
shift 5
bin=$target/avb-endpointd
mount -t tmpfs tmpfs /mnt
work=/mnt
mkdir -p "$work/stub" "$work/data/avb-endpointd/models" "$work/config/avb-endpointd" "$work/run"
chmod 700 "$work/run"
printf '#!/bin/sh\nexit 0\n' >"$work/stub/systemctl"
# No real-time threads through RealtimeKit, which busctl would ask.
printf '#!/bin/sh\nexit 1\n' >"$work/stub/busctl"
chmod +x "$work/stub/systemctl" "$work/stub/busctl"
cp "$root"/packaging/linux/models/* "$work/data/avb-endpointd/models/"
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /run/gptpd
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
export XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data
export DBUS_SESSION_BUS_ADDRESS=disabled:
export PATH=$work/stub:$PATH

entity() {
    awk -F: -v instance="$2" '{ printf "0x%s%s%sff%02x%s%s%s\n", $1, $2, $3, instance, $4, $5, $6 }' \
        "/sys/class/net/$1/address"
}
instance=$(awk '/^instance *=/ { print $3; exit }' "$app_config")
app=$(entity v0 "${instance:-0}")
room=$(entity v1 1)
sed "s/@ROOM@/$room/g" "$app_config" >"$work/config/avb-endpointd/v0.toml"
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
"$target/examples/ring_room" "$work/run/avb-endpointd" "$play" "$record" "$work/stop" &
the_room=$!

"$@"
status=$?

touch "$work/stop"
wait "$the_room"
kill "$first" "$second"
wait "$first"
wait "$second"
kill "$gptpd"
if [ "$status" -ne 0 ]; then
    for log in v0 v1; do
        echo "== $log"; cat "$work/$log.log"
    done
fi
exit "$status"
