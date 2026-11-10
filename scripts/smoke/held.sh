#!/bin/sh
# Smoke test, no root needed: in a user and network namespace, an older
# program's entity at the default's instance (0x70) on a dummy interface,
# then the daemon beside it with an entity of its own at instance 3. The
# daemon must hold its default entity off the network without ever
# advertising its ID, and still run the other.
#
#   scripts/smoke/held.sh [path/to/avb-endpointd]
#
# Needs unshare, ip and python3. Build the daemon first, with
# `cargo build -p avb-endpointd --features daemon`.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    bin=${1:-$root/target/debug/avb-endpointd}
    bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    unshare -rnm sh "$0" --inside "$bin" "$work"
    exit $?
fi

bin=$2
work=$3
mkdir -p "$work/stub" "$work/data/avb-endpointd/models" "$work/config/avb-endpointd" "$work/run"
printf '#!/bin/sh\nexit 0\n' >"$work/stub/systemctl"
# No real-time threads: RealtimeKit, which avb-stream asks through
# busctl, would raise a stream thread over the computer's own audio.
printf '#!/bin/sh\nexit 1\n' >"$work/stub/busctl"
chmod +x "$work/stub/systemctl" "$work/stub/busctl"
cp "$root"/packaging/linux/models/* "$work/data/avb-endpointd/models/"
printf '[[endpoint]]\ninstance = 3\nname = "Mine"\n' >"$work/config/avb-endpointd/d0.toml"
mount -t sysfs sysfs /sys
ip link add d0 type dummy && ip link set d0 up

python3 -I "$here/older-entity.py" d0 "$work/older.log" &
older=$!
sleep 1
XDG_CONFIG_HOME=$work/config XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data \
    PATH=$work/stub:$PATH "$bin" --interface d0 >"$work/daemon.log" 2>&1 &
daemon=$!
sleep 5
status=$(cat "$work/run/avb-endpointd/d0.toml")
kill "$daemon"
wait "$daemon"
sleep 0.3
kill "$older"

failed=0
if ! printf '%s\n' "$status" | grep -q '^held = true'; then
    echo "FAIL: the default entity is not held"
    failed=1
fi
if grep -q 'AVAILABLE .*(the older entity' "$work/older.log"; then
    echo "FAIL: the daemon advertised the older entity's ID"
    failed=1
fi
if ! grep -q 'AVAILABLE ' "$work/older.log"; then
    echo "FAIL: the daemon's other entity never advertised"
    failed=1
fi
if [ "$failed" -ne 0 ]; then
    echo "== what the older entity heard"; cat "$work/older.log"
    echo "== daemon log"; cat "$work/daemon.log"
    exit 1
fi
echo "ok: the default entity was held, never advertised, and the other ran"
