#!/bin/sh
# Smoke test, no root needed: unusual channel counts. In a user and
# network namespace, a PipeWire of its own on a tmpfs and two daemons on
# the two ends of a veth pair. The second has an entity besides its
# default with a 6-channel input and output, a mono input and a
# 60-channel input. The first gives each a device as wide as the
# smallest of 2, 8, 16 and 32 that holds what it takes, and says a
# stream set to more than 32 has no format. Played into, the 6-channel
# input connects a pool talker that takes its 6 channels, and recorded
# from, the 6-channel output binds a pool listener that takes them, while
# the second daemon's formats are never changed. The 60-channel input
# never connects. No audio flows, as no gPTP runs: the test is of
# matching formats and connecting.
#
#   scripts/smoke/channels.sh [path/to/avb-endpointd]
#
# Needs unshare, ip, pipewire, wireplumber, pw-play, pw-record, pw-link
# and python3. Build the daemon first, with
# `cargo build -p avb-endpointd --features daemon`.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    bin=${1:-$root/target/debug/avb-endpointd}
    bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
    unshare -rnm sh "$0" --inside "$bin"
    exit $?
fi

bin=$2
# A short path: PipeWire's socket path must fit a Unix socket's.
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
ip link add v0 type veth peer name v1
ip link set v0 up && ip link set v1 up
export XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data
export PIPEWIRE_RUNTIME_DIR=$work/run DBUS_SESSION_BUS_ADDRESS=disabled:
export PATH=$work/stub:$PATH
for conf in pipewire client; do
    mkdir -p "$XDG_CONFIG_HOME/pipewire/$conf.conf.d"
    echo 'context.properties = { module.rt = false }' \
        >"$XDG_CONFIG_HOME/pipewire/$conf.conf.d/no-rt.conf"
done
# Nor any of the computer's sound cards, MIDI, Bluetooth or cameras: a
# stream whose device goes, as these tests make devices go, would move to
# a real one, and play out loud or record the room.
mkdir -p "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d"
cat >"$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/no-hardware.conf" <<'EOF'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.libcamera = disabled
    monitor.v4l2 = disabled
  }
}
EOF
# The second daemon's entity of unusual counts, beside its default.
cat >"$work/config/avb-endpointd/v1.toml" <<'EOF'
[[endpoint]]
instance = 1
model = "avb-endpointd"
name = "Odd"

[[endpoint.stream]]
direction = "in"
name = "Six in"
channels = 6

[[endpoint.stream]]
direction = "in"
name = "Mono in"
channels = 1

[[endpoint.stream]]
direction = "in"
name = "Wide in"
channels = 60

[[endpoint.stream]]
direction = "out"
name = "Six out"
channels = 6
EOF

pipewire >"$work/pipewire.log" 2>&1 &
pipewire=$!
sleep 1
wireplumber >"$work/wireplumber.log" 2>&1 &
wireplumber=$!
sleep 1
# Checked before anything plays: no sound card shows here.
if pw-link -o -i 2>/dev/null | grep -q '^alsa_'; then
    echo "FAIL: a sound card shows in the test's PipeWire; nothing played"
    kill "$wireplumber" "$pipewire"
    exit 1
fi
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
# Each [[device]] table of a status on one line.
devices() {
    status "$1" | awk '/^\[\[device\]\]/ { if (line) print line; line = ""; inside = 1; next }
        /^\[/ { if (line) print line; line = ""; inside = 0; next }
        inside { line = line " " $0 } END { if (line) print line }'
}
# Waits up to $1 seconds for the command after it to succeed.
wait_for() {
    limit=$1
    shift
    waited=0
    until "$@" || [ "$waited" -ge "$limit" ]; do
        sleep 1
        waited=$((waited + 1))
    done
}
# The entity ID of the second daemon's entity named Odd.
odd_entity() {
    status v1 | awk '/^\[\[endpoint\]\]/ { id = "" } /^entity_id = / { id = $3 }
        /^name = "Odd"/ { gsub(/"/, "", id); print id; exit }'
}
# v0's device for one of Odd's streams, on one line.
device() {
    devices v0 | grep "\"$odd\"" | grep "way = \"$1\"" | grep "index = $2 "
}
odd_found() {
    odd=$(odd_entity)
    [ -n "$odd" ] && device destination 2 | grep -q 'state = '
}
wait_for 25 odd_found
odd=$(odd_entity)
[ -n "$odd" ] || fail "no entity Odd on v1"
hex=${odd#0x}

# Widths and states, before anything plays.
six=$(device destination 0)
mono=$(device destination 1)
wide=$(device destination 2)
printf '%s\n' "$six" | grep -q 'channels = 8 .*live_channels = 6' || fail "the 6-channel input is not 6 of 8: $six"
printf '%s\n' "$six" | grep -q 'state = "idle"' || fail "the 6-channel input is not idle: $six"
printf '%s\n' "$mono" | grep -q 'live_channels = 1 ' || fail "the mono input is not 1 live: $mono"
printf '%s\n' "$mono" | grep -q 'state = "idle"' || fail "the mono input is not idle: $mono"
printf '%s\n' "$wide" | grep -q 'channels = 32 ' || fail "the 60-channel input's device is not 32: $wide"
printf '%s\n' "$wide" | grep -q 'state = "no-format"' || fail "the 60-channel input does not say no-format: $wide"

python3 -I -c '
import math, struct, sys, wave
with wave.open(sys.argv[1], "wb") as out:
    out.setnchannels(2); out.setsampwidth(2); out.setframerate(48000)
    frames = (int(8000 * math.sin(2 * math.pi * 440 * n / 48000)) for n in range(48000 * 30))
    out.writeframes(b"".join(struct.pack("<hh", v, v) for v in frames))
' "$work/tone.wav"

# The 6-channel input, played into: a pool talker at its 6 channels.
six_node="avb-dest-$hex-in-0"
has_node() {
    pw-link -i 2>/dev/null | grep -q "^$1:"
}
wait_for 10 has_node "$six_node"
has_node "$six_node" || fail "no device $six_node in PipeWire"
pw-play --target "$six_node" "$work/tone.wav" >"$work/play.log" 2>&1 &
player=$!
six_connected() {
    device destination 0 | grep -q 'state = "connected"'
}
wait_for 15 six_connected
six_connected || fail "the 6-channel input did not connect"
grep -qE 'Destination [0-9]+: format AAF 48 kHz, 6 ch' "$work/v0.log" \
    || fail "no pool talker took 6 channels"

# The 6-channel output, pinned and recorded from: a pool listener at 6.
"$bin" pin "$odd:out0" --interface v0 >"$work/pin.log" 2>&1 || fail "the tool could not pin"
source_node="avb-src-$hex-out-0"
has_source() {
    pw-link -o 2>/dev/null | grep -q "^$source_node:"
}
wait_for 10 has_source
has_source || fail "no device $source_node in PipeWire"
device source 0 | grep -q 'channels = 8 .*live_channels = 6' || fail "the 6-channel output is not 6 of 8"
pw-record --target "$source_node" "$work/record.wav" >"$work/record.log" 2>&1 &
recorder=$!
source_connected() {
    device source 0 | grep -q 'state = "connected"'
}
wait_for 15 source_connected
source_connected || fail "the 6-channel output did not connect"
grep -qE 'Source [0-9]+: format AAF 48 kHz, 6 ch' "$work/v0.log" \
    || fail "no pool listener took 6 channels"

# The 60-channel input, played into: it never connects.
wide_node="avb-dest-$hex-in-2"
wait_for 5 has_node "$wide_node"
pw-play --target "$wide_node" "$work/tone.wav" >"$work/wide.log" 2>&1 &
wide_player=$!
sleep 4
device destination 2 | grep -q 'state = "no-format"' || fail "the 60-channel input left no-format"
grep -q "Odd Wide in: connect" "$work/v0.log" && fail "the 60-channel input was connected"

# The second daemon's formats as it set them: none changed.
grep -E '^Odd .*: format ' "$work/v1.log" && fail "a format on v1 was changed"

kill "$player" "$recorder" "$wide_player" 2>/dev/null
kill "$first" "$second"
wait "$first"
wait "$second"
kill "$wireplumber" "$pipewire"
if [ "$failed" -ne 0 ]; then
    echo "== v0 devices"; devices v0
    echo "== v0 status"; status v0
    echo "== v1 status"; status v1
    for log in v0 v1 play record wide pin; do
        echo "== $log"; cat "$work/$log.log"
    done
    exit 1
fi
echo "== v0's log"; grep -E 'format|connect|released' "$work/v0.log"
echo "ok: 6 of 8, 1 live and no-format for 60 channels; the pool took 6 channels each way, and v1's formats stayed"
