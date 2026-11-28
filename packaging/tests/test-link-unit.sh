#!/bin/sh
# Runs avb-link@'s script as systemd hands it to sh, on an interface
# this computer doesn't have, with ethtool, tc and ip standing in. With
# no RX_USECS it must ask nothing about interrupt moderation; with one,
# it sets rx-usecs only where the interface says another value, logs a
# driver's refusal and a value that is no number, and goes on to EEE and
# PAUSE either way. test-package.sh runs it on the installed unit; on
# the repository's:
#
#   packaging/tests/test-link-unit.sh packaging/linux/units/avb-link@.service
set -eu

unit=${1:?usage: test-link-unit.sh UNIT}
interface=avbtest0
unset RX_USECS REFUSE

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ ! -e "/sys/class/net/$interface" ] || fail "this computer has an interface $interface"
# Lines ending in a backslash joined with a space, the command's script
# out of its quotes, and $$ made $.
script=$(sed -e ':a' -e '/\\$/{N' -e 's/\\\n/ /' -e 'ta' -e '}' "$unit" |
    sed -n "s/^ExecStart=\/bin\/sh -c '\(.*\)' sh %i\$/\1/p" |
    sed 's/\$\$/$/g')
[ -n "$script" ] || fail "no ExecStart script in $unit"

stubs=$(mktemp -d)
trap 'rm -rf "$stubs"' EXIT
# ethtool answers as igc does, one moderation for both directions, from
# the value in rx: "none" for a driver without any, and REFUSE refuses
# a change.
cat >"$stubs/ethtool" <<'EOF'
#!/bin/sh
echo "ethtool $*" >>"$STUBS/calls"
rx=$(cat "$STUBS/rx")
case $1 in
--show-coalesce)
    if [ "$rx" = none ]; then
        echo "netlink error: Operation not supported" >&2
        exit 76
    fi
    printf 'Coalesce parameters for %s:\nrx-usecs:\t%s\nrx-frames:\tn/a\n\ntx-usecs:\t%s\n' \
        "$2" "$rx" "$rx"
    ;;
--coalesce)
    if [ "$rx" = none ]; then
        echo "netlink error: Operation not supported" >&2
        exit 76
    fi
    if [ -n "${REFUSE:-}" ]; then
        echo "netlink error: Invalid argument" >&2
        exit 75
    fi
    echo "$4" >"$STUBS/rx"
    ;;
--show-eee) echo "EEE settings for $2: disabled" ;;
--show-pause) echo "Pause parameters for $2: off" ;;
esac
EOF
for tool in tc ip; do
    printf '#!/bin/sh\necho "%s $*" >>"$STUBS/calls"\n' "$tool" >"$stubs/$tool"
done
chmod +x "$stubs/ethtool" "$stubs/tc" "$stubs/ip"

# Runs the script with RX_USECS (none where empty) and the interface's
# rx-usecs, any further words going into its environment.
run() {
    rx_usecs=$1
    echo "$2" >"$stubs/rx"
    shift 2
    : >"$stubs/calls"
    if [ -n "$rx_usecs" ]; then
        set -- RX_USECS="$rx_usecs" "$@"
    fi
    out=$(env "$@" STUBS="$stubs" PATH="$stubs:$PATH" sh -c "$script" sh "$interface" 2>&1) ||
        fail "the script failed with RX_USECS=$rx_usecs: $out"
    grep -qx "ethtool --set-eee $interface eee off" "$stubs/calls" ||
        fail "RX_USECS=$rx_usecs: EEE was not put off"
}

said() {
    case $out in
    *"$1"*) ;;
    *) fail "the script did not say \"$1\": $out" ;;
    esac
}

asked() {
    grep -qx -- "$1" "$stubs/calls"
}

run "" 3
! grep -q coalesce "$stubs/calls" || fail "with no RX_USECS it asked about moderation"

run 0 3
asked "ethtool --coalesce $interface rx-usecs 0" || fail "RX_USECS=0 did not set rx-usecs"
said "$interface: rx-usecs 3 to 0, tx-usecs now 0, as /etc/avb-link/$interface.conf asks"

run 0 0
asked "ethtool --show-coalesce $interface" || fail "RX_USECS=0 did not read rx-usecs"
! asked "ethtool --coalesce $interface rx-usecs 0" || fail "rx-usecs 0 was set again"
said "$interface: rx-usecs 0, as /etc/avb-link/$interface.conf asks"

run 0 3 REFUSE=1
said "$interface refused rx-usecs 0, left as it is: netlink error: Invalid argument"

run 0 none
said "$interface refused rx-usecs 0, left as it is: netlink error: Operation not supported"

for value in fast 010 -1 "0 0"; do
    run "$value" 3
    said "$interface: RX_USECS=$value is no number of microseconds, left as it is"
    ! grep -q coalesce "$stubs/calls" || fail "RX_USECS=$value asked about moderation"
done

echo "PASS: $(basename "$unit")"
