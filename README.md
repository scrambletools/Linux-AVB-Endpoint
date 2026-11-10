# Linux AVB Endpoint

This computer on an AVB or Milan network, for any app. `avb-endpointd`
runs this computer's AVB entities on one wired interface, as the
person's `avb-endpointd@<interface>` user unit. Each entity's stream
outputs are PipeWire playback devices any app plays into, and its stream
inputs recording devices any app records from, so a media player, a DAW
or a browser reaches the network as it reaches speakers and a
microphone. Any controller (Hive, triib, a Milan device's own) connects
the streams.

Every interface the daemon runs on has a default entity, named for the
computer, with a stereo "Playback" output and "Listen" input. Apps such
as triib add entities of their own beside it.

## Crates

| Crate | What it is |
| --- | --- |
| `avb-endpointd` | The configuration, model, status and meters files apps share with the daemon, and the daemon's unit, as a library on Linux, macOS and Windows; with the `daemon` feature, the daemon itself, for Linux |
| `avb-pipewire` | The virtual PipeWire devices the daemon gives its streams |
| `avb-netraw` | The one program granted `CAP_NET_RAW`, which opens raw Ethernet sockets for the daemon and the apps' controllers |

The protocol and stream crates they build on, `atdecc`, `avb-net`,
`avb-mrp` and `avb-stream`, come from
[scrambletools/avb](https://github.com/scrambletools/avb) on crates.io,
and gPTP from gptpd, in
[scrambletools/gptp](https://github.com/scrambletools/gptp).

## The files

- `~/.config/avb-endpointd/<interface>.toml`: the entities to run, which
  apps change through the library's `config::edit`, under the file's
  lock, and a person may edit by hand.
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
```

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
