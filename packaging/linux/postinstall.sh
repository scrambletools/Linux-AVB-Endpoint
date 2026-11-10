#!/bin/sh
# Grants avb-netraw, the one program that opens raw Ethernet sockets for
# the daemon and the apps' controllers, CAP_NET_RAW, and has systemd read
# the link units. It starts and enables nothing: an app starts the
# person's avb-endpointd@INTERFACE as they put this computer on the
# network.
setcap cap_net_raw+ep /usr/lib/avb-endpointd/avb-netraw || {
    echo "avb-endpointd: could not grant avb-netraw CAP_NET_RAW" >&2
}
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
