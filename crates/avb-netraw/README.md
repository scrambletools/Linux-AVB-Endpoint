# avb-netraw

A small helper for [avb-endpointd](https://crates.io/crates/avb-endpointd)
and AVB apps on Linux: the only program granted `CAP_NET_RAW`
(`setcap cap_net_raw+ep`). It opens raw Ethernet sockets and passes them
over a socket pair to the program that started it, so the daemon and
apps' controllers need no capability of their own and run as ordinary
programs.

It is not on crates.io. Packages build it from the release tag and
install it as `/usr/lib/avb-endpointd/avb-netraw`. The broker it runs
is `avb_net::broker`, from [avb-net](https://crates.io/crates/avb-net).

## License

MIT OR Apache-2.0, at your option.
