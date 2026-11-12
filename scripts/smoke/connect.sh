#!/bin/sh
# Smoke test, no root needed: in a user and network namespace, a PipeWire
# of its own on a tmpfs and two daemons on the two ends of a veth pair.
# A tone played into one daemon's device for the other's Listen input,
# linked by hand as no session manager runs, has the first connect one of
# its default entity's pool talkers to that input: it binds the listener,
# the status says connected, and the file remembers the stream as pinned.
# Then the other's Playback output, pinned as a source and recorded from,
# has the first bind one of its own pool listeners to it; unpinned while
# recording, the source is released at once and that listener unbound.
# Then the first is killed while playing, which leaves the other's input
# bound to its pool talker: started again, it finds that binding with no
# connection of its own and lets it go. Playing once more and stopped,
# it disconnects the input as it stops.
# No audio flows, as no gPTP runs: the test is of connecting.
#
#   scripts/smoke/connect.sh [path/to/avb-endpointd]
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
mkdir -p "$work/stub" "$work/data/avb-endpointd/models" "$work/config" "$work/run"
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
# No real-time threads for this PipeWire or its clients, the daemons
# among them: RealtimeKit could raise one above an audio thread the
# computer runs for real.
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

pipewire >"$work/pipewire.log" 2>&1 &
pipewire=$!
sleep 1
# The session manager gives the devices their ports, links the tone,
# and suspends a device no app uses.
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
devices() {
    status "$1" | awk '/^\[\[device\]\]/ { if (line) print line; line = ""; inside = 1; next }
        /^\[/ { if (line) print line; line = ""; inside = 0; next }
        inside { line = line " " $0 } END { if (line) print line }'
}
# The state line of the stream named $2 in $1's status.
stream_state() {
    status "$1" | awk -v name="name = \"$2\"" '/^\[/ { found = 0 }
        $0 == name { found = 1 } found && /^state = / { print; exit }'
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
waited=0
until devices v0 | grep 'way = "destination"' | grep -q 'node = ' || [ "$waited" -ge 20 ]; do
    sleep 1
    waited=$((waited + 1))
done
other=$(status v1 | grep -m1 '^entity_id' | sed 's/.*"\(0x[0-9a-f]*\)".*/\1/')
node="avb-dest-${other#0x}-in-0"
sleep 2
pw-link -i 2>/dev/null | grep -q "^$node:" || fail "no device $node in PipeWire"

# Ten seconds of a tone, played into the device by hand.
python3 -I -c '
import math, struct, sys, wave
with wave.open(sys.argv[1], "wb") as out:
    out.setnchannels(2); out.setsampwidth(2); out.setframerate(48000)
    frames = (int(8000 * math.sin(2 * math.pi * 440 * n / 48000)) for n in range(48000 * 10))
    out.writeframes(b"".join(struct.pack("<hh", v, v) for v in frames))
' "$work/tone.wav"
pw-play --target "$node" "$work/tone.wav" >"$work/play.log" 2>&1 &
player=$!
sleep 1
realtime=$(ps -L -o cls=,rtprio=,comm= -p "$pipewire,$wireplumber,$first,$second,$player" | grep -E '^ *(RR|FF)')
if [ -n "$realtime" ]; then
    echo "FAIL: a real-time thread: $realtime"
    kill "$player" "$first" "$second" "$wireplumber" "$pipewire"
    exit 1
fi
waited=0
until devices v0 | grep "\"$other\"" | grep 'way = "destination"' | grep -q 'state = "connected"' \
    || [ "$waited" -ge 15 ]; do
    sleep 1
    waited=$((waited + 1))
done
destination=$(devices v0 | grep "\"$other\"" | grep 'way = "destination"')
printf '%s\n' "$destination" | grep -q 'state = "connected"' || fail "the device did not connect"
printf '%s\n' "$destination" | grep -q 'remembered = true' || fail "the device was not remembered"
grep -A8 '^\[\[pin\]\]' "$work/config/avb-endpointd/v0.toml" | grep -q 'remembered = true' \
    || fail "the file does not remember it"
status v1 | grep -A12 'name = "Listen"' | grep -qE 'state = "(bound|listening|waiting)"' \
    || fail "v1's Listen input is not bound"

# A source: v1's Playback output, pinned as sources wait to be, and
# recorded from by hand.
"$bin" pin "$other:out0" --interface v0 >"$work/pin.log" 2>&1 || fail "the tool could not pin"
source_node="avb-src-${other#0x}-out-0"
has_source_node() {
    pw-link -o 2>/dev/null | grep -q "^$source_node:"
}
wait_for 10 has_source_node
has_source_node || fail "no device $source_node in PipeWire"
pw-record --target "$source_node" "$work/record.wav" >"$work/record.log" 2>&1 &
recorder=$!
source_connected() {
    devices v0 | grep "\"$other\"" | grep 'way = "source"' | grep -q 'state = "connected"'
}
wait_for 15 source_connected
source_connected || fail "the source did not connect"
stream_state v0 "Source 1" | grep -qE '"(bound|listening)"' \
    || fail "v0's first source listener is not bound"
# Unpinned while recording, it is no longer shown: released at once, the
# listener unbound, and nothing remembers it.
"$bin" unpin "$other:out0" --interface v0 >"$work/unpin.log" 2>&1 || fail "the tool could not unpin"
source_released() {
    grep -q 'released, no longer shown' "$work/v0.log"
}
wait_for 10 source_released
source_released || fail "the unpinned source was not released"
# The status file catches up at the daemon's next report.
listener_unbound() {
    stream_state v0 "Source 1" | grep -q '"unbound"'
}
wait_for 5 listener_unbound
listener_unbound || fail "v0's first source listener is still bound"
awk '/^\[\[pin\]\]/ { inside = 1; next } /^\[/ { inside = 0 } inside && /^output = /' \
    "$work/config/avb-endpointd/v0.toml" | grep -q . && fail "the file still pins the source"

# Killed while playing: the other's Listen stays bound to the pool talker,
# until the first, started again, lets it go.
kill "$recorder" 2>/dev/null
# v0's status as the daemon running now wrote it: a killed one leaves
# its last behind until the next writes its own, two seconds in.
v0_current() {
    status v0 | grep -q "^pid = $first\$"
}
destination_connected() {
    v0_current \
        && devices v0 | grep "\"$other\"" | grep 'way = "destination"' | grep -q 'state = "connected"'
}
listen_bound() {
    stream_state v1 Listen | grep -qE '"(bound|listening)"'
}
listen_unbound() {
    stream_state v1 Listen | grep -q '"unbound"'
}
kill "$player" 2>/dev/null
pw-play --target "$node" "$work/tone.wav" >"$work/play1.log" 2>&1 &
player=$!
wait_for 15 destination_connected
wait_for 5 listen_bound
listen_bound || fail "v1's Listen was not bound before v0 was killed"
kill "$player" 2>/dev/null
kill -KILL "$first"
wait "$first" 2>/dev/null
sleep 2
listen_bound || fail "v1's Listen did not stay bound after v0 was killed"
"$bin" --interface v0 >>"$work/v0.log" 2>&1 &
first=$!
wait_for 10 v0_current
v0_current || fail "v0 started again wrote no status of its own"
wait_for 20 listen_unbound
listen_unbound || fail "v0 started again did not let go of v1's Listen"
grep -q 'bound to a pool talker with no connection here, released' "$work/v0.log" \
    || fail "v0 did not say it let go of a stale binding"
# Playing once more, then stopped: v0 disconnects the Listen as it stops.
pw-play --target "$node" "$work/tone.wav" >"$work/play2.log" 2>&1 &
player=$!
wait_for 15 destination_connected
destination_connected || fail "the device did not connect again"
wait_for 5 listen_bound
# The tone stops first, the connection staying its 30 s: nothing plays
# as the daemon goes.
kill "$player" 2>/dev/null
kill "$first"
wait "$first"
wait_for 5 listen_unbound
listen_unbound || fail "v0's stop did not disconnect v1's Listen"
grep -q 'released, stopping' "$work/v0.log" || fail "v0 did not say it released at its stop"

kill "$player" "$recorder" 2>/dev/null
kill "$second"
wait "$first"
wait "$second"
kill "$wireplumber" "$pipewire"
if [ "$failed" -ne 0 ]; then
    echo "== v0 status"; status v0
    echo "== v1 status"; status v1
    for log in v0 v1 play play1 play2 record pin unpin wireplumber; do
        echo "== $log"; cat "$work/$log.log"
    done
    echo "== ports"; pw-link -o 2>/dev/null; pw-link -i 2>/dev/null
    exit 1
fi
echo "== v0's log"; grep -E 'connect|released|source listener' "$work/v0.log"
echo "ok: an app's tone connected a pool talker to the other's input, and the file remembers it;"
echo "ok: recording bound a pool listener to the other's output, and unpinning released it;"
echo "ok: a stale binding was let go of at start, and a connection at stop"
