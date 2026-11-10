# avb-endpointd

Runs a Linux computer's AVB and Milan entities, so that any app can
play to and record from the network through PipeWire. Part of
[Linux-AVB-Endpoint](https://github.com/scrambletools/Linux-AVB-Endpoint).

The crate has two halves:

- **A library**, which builds on Linux, macOS and Windows, for apps that
  work with the daemon:
  - `config`: the entities to run on an interface, read from their
    file, and changed with `config::edit` under the file's lock;
  - `model`: the model files that give each entity its vendor, model
    name, firmware version and logos;
  - `status` and `meters`: what the daemon is doing, and how loud each
    stream is;
  - `store`: where those files live;
  - `unit`: starting, stopping and checking the daemon's systemd user
    unit;
  - `entity_id`: an entity's ID, from the interface's MAC address and
    the entity's instance.
- **The daemon**, with the `daemon` feature, for Linux:
  `avb-endpointd --interface <name>`, run as the user unit
  `avb-endpointd@<interface>`. It runs ADP, the AEM responder, ACMP,
  MAAP, MSRP and MVRP for every entity on the interface, takes gPTP
  from gptpd, and sends and receives each stream through a PipeWire
  device from [avb-pipewire](https://crates.io/crates/avb-pipewire).

An app needs only the library:

```toml
[dependencies]
avb-endpointd = "0.2"
```

The [repository's README](https://github.com/scrambletools/Linux-AVB-Endpoint#readme)
covers building and installing the daemon, its helper and its units,
and [docs/endpointd.md](https://github.com/scrambletools/Linux-AVB-Endpoint/blob/main/docs/endpointd.md)
the design.

## License

MIT OR Apache-2.0, at your option.
