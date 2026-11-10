# avb-pipewire

Virtual PipeWire audio devices for AVB streams: a playback device
(`Audio/Sink`) for each stream output, which apps play into, and a
recording device (`Audio/Source`) for each stream input, which apps
record from. Each device fills or drains an
[avb-stream](https://crates.io/crates/avb-stream) buffer, which the
stream's talker or listener reads or writes as it would a sound card's.
Part of [Linux-AVB-Endpoint](https://github.com/scrambletools/Linux-AVB-Endpoint),
where avb-endpointd uses it.

- `VirtualAudio::start` connects to PipeWire on a thread of its own,
  which owns the devices. `add` makes a device from a `DeviceSpec`
  (node name, description, channels, rate, and playback or recording),
  and dropping the `VirtualDevice` it returns removes the device.
- Devices take 32-bit float samples and run at the graph's default
  rate, `VirtualAudio::rate`, normally 48 kHz, whatever rate the stream
  runs at; the stream's side converts between the two.
- Each is marked `node.virtual` and `node.network`, so the desktop
  treats it as a network device.
- It builds everywhere, but devices exist only on Linux; elsewhere
  `start` returns an error.

## License

MIT OR Apache-2.0, at your option.
