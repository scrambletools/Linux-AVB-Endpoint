#!/bin/sh
# Smoke test, no root needed: in a user and network namespace, a PipeWire
# of its own on a tmpfs and two daemons on the two ends of a veth pair.
# A tone played into one daemon's device for the other's Listen input,
# linked by hand as no session manager runs, has the first connect one of
# its default entity's pool talkers to that input: it binds the listener,
# the status says connected, and the file remembers the stream as pinned.
# No audio flows, as no gPTP runs: the test is of connecting.
#
#   scripts/smoke/connect.sh [path/to/avb-endpointd]
#
# Needs unshare, ip, pipewire, pw-play, pw-link and python3. Build the
# daemon first, with `cargo build -p avb-endpointd --features daemon`.
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

pipewire >"$work/pipewire.log" 2>&1 &
pipewire=$!
sleep 1
# The session manager gives the devices their ports, links the tone,
# and suspends a device no app uses.
wireplumber >"$work/wireplumber.log" 2>&1 &
wireplumber=$!
sleep 1
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

kill "$player" 2>/dev/null
kill "$first" "$second"
wait "$first"
wait "$second"
kill "$wireplumber" "$pipewire"
if [ "$failed" -ne 0 ]; then
    echo "== v0 status"; status v0
    echo "== v1 status"; status v1
    for log in v0 v1 play wireplumber; do
        echo "== $log"; cat "$work/$log.log"
    done
    echo "== ports"; pw-link -o 2>/dev/null; pw-link -i 2>/dev/null
    exit 1
fi
echo "== v0's log"; grep -E 'connect|released' "$work/v0.log"
echo "ok: an app's tone connected a pool talker to the other's input, and the file remembers it"
