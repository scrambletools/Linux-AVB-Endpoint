# Linux AVB Endpoint

Puts a Linux computer on an AVB or Milan network as an audio endpoint
that any app can use. `avb-endpointd` runs the computer's AVB entities
on one wired interface, as a systemd user unit,
`avb-endpointd@<interface>`. Each stream output is a PipeWire playback
device and each stream input a recording device, so a media player, a
DAW or a browser plays to the network as it would to speakers, and
records from it as from a microphone. Any controller, such as Hive,
triib or a Milan device's own, connects the streams.

Each interface the daemon runs on has a default entity, named after the
computer, with a stereo output, "Playback", and a stereo input,
"Listen". Apps such as triib can add entities of their own beside it.

## Crates

| Crate | What it is |
| --- | --- |
| [`avb-endpointd`](crates/avb-endpointd) | A library for Linux, macOS and Windows with what apps share with the daemon: its configuration, model, status and meters files, and its user unit. With the `daemon` feature, the daemon itself, for Linux |
| [`avb-pipewire`](crates/avb-pipewire) | The PipeWire playback and recording devices the daemon gives its streams |
| [`avb-netraw`](crates/avb-netraw) | A small helper, the only program granted `CAP_NET_RAW`, that opens raw Ethernet sockets for the daemon and for apps' controllers |

The protocol and stream crates they build on, `atdecc`, `avb-net`,
`avb-mrp` and `avb-stream`, come from
[scrambletools/avb](https://github.com/scrambletools/avb) on crates.io,
and gPTP from gptpd, in
[scrambletools/gptp](https://github.com/scrambletools/gptp).

## Using it

An app such as plaa or triib turns this computer on and off the
network. Without one, the daemon's own command does:

```sh
avb-endpointd on --interface enp2s0      # start it now and at each login
avb-endpointd list --interface enp2s0    # speakers and microphones found
avb-endpointd off --interface enp2s0
```

Each speaker or amp input found on the network gets a playback device
of its own, which any app plays into. `hide` leaves one out, `pin`
keeps one while it is off the network, and `mode destinations pinned`
shows only those pinned, for a large network. A microphone or other
talker gets a recording device once pinned, or with
`mode sources all`. `list` names each by name or by
`<entity id>:in<n>` and `<entity id>:out<n>`, and the others take
either.
On this branch a destination connects as an app plays into it and
lets go 30 s after; a source does not connect yet
([docs/endpointd.md](docs/endpointd.md)).

## The files

- `~/.config/avb-endpointd/<interface>.toml`: the entities to run,
  which apps change through the library's `config::edit` under the
  file's lock, and which you can also edit by hand.
- `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.toml`: what the daemon is
  doing, rewritten every two seconds.
- `/usr/share/avb-endpointd/models/<model>.toml`: each entity's vendor,
  model name, firmware version and logos, which an app's package
  installs.

[docs/endpointd.md](docs/endpointd.md) is the design, and where it is
going.

## Building and installing

```sh
cargo build --release -p avb-endpointd --features daemon -p avb-netraw
sudo scripts/dist-install.sh target/release / /usr
sudo setcap cap_net_raw+ep /usr/lib/avb-endpointd/avb-netraw
systemctl --user enable --now avb-endpointd@<interface>
```

The daemon needs PipeWire, RealtimeKit (rtkit) for its streams'
realtime threads, and gptpd for the network's time; the link units
need ethtool and iproute2. triib's packages carry avb-endpointd until it
has packages of its own.

## Developing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p avb-endpointd --features daemon --all-targets -- -D warnings
cargo test --workspace
cargo test -p avb-endpointd --features daemon
cargo build -p avb-endpointd --features daemon && scripts/smoke/held.sh
```

The scripts in `scripts/smoke` run the daemon in a user and network
namespace on dummy interfaces, and need no root: `held.sh` beside an
older program's entity at the default's instance, `instances.sh` on
two interfaces at once, with a second daemon refused, and `devices.sh`
with two daemons on a veth pair finding each other's streams.

To build against unpublished changes to the avb or gptp crates, a
git-ignored `.cargo/avb-local.toml` can patch them to checkouts beside
this one, used as `cargo build --config .cargo/avb-local.toml`:

```toml
[patch.crates-io]
atdecc = { path = "../avb/crates/atdecc" }
avb-mrp = { path = "../avb/crates/avb-mrp" }
avb-net = { path = "../avb/crates/avb-net" }
avb-stream = { path = "../avb/crates/avb-stream" }
gptp = { path = "../gptp/crates/gptp" }
```

It writes local paths into `Cargo.lock`, so build once without it
before committing.

## License

MIT OR Apache-2.0, at your option.
