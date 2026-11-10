//! The daemon's controller, `FF-FB` in its entity ID's instance octets
//! beside triib's controller (`FF-FE`), triib-cli's (`FF-FD`) and plaa's
//! (`FF-FC`): it finds the other devices on the network and reads their
//! stream inputs and outputs, for the network devices. It advertises no
//! entity of its own, so nothing new shows in controllers' lists, and it
//! reads no entity advertised from this computer's address, as this
//! computer's own entities get no network device.

use atdecc::controller::{Config, Controller, Event};
use atdecc::descriptor::DescriptorType;
use atdecc::{EntityId, Instant};
use avb_net::MacAddress;

use crate::devices::{FoundStream, RemoteStream};

/// The instance octet of the daemon's controller.
pub const CONTROLLER_INSTANCE: u8 = 0xfb;

/// The controller, and whether what it knows of the network changed since
/// the daemon last asked.
pub struct Discovery {
    controller: Controller,
    mac: MacAddress,
    changed: bool,
    buffer: Vec<u8>,
}

impl Discovery {
    pub fn new(mac: MacAddress) -> Self {
        let mut config = Config::new(crate::entity_id(mac, CONTROLLER_INSTANCE));
        // Only what the network devices need: each entity's descriptors and
        // its notifications, which keep its streams' formats and bindings
        // current.
        config.network_info = false;
        config.read_mappings = false;
        config.media_clock_info = false;
        config.read_counters = false;
        config.read_transit_times = false;
        config.read_configurations = false;
        config.read_talker_state = false;
        config.lite_status = false;
        config.wireless_status = false;
        config.read_logos = false;
        config.own_mac = Some(mac);
        config.advertise = None;
        let mut controller = Controller::new(config);
        // Everything already on the network answers at once, rather than
        // at its next advertisement.
        controller.discover(None);
        Discovery {
            controller,
            mac,
            changed: true,
            buffer: vec![0; 1500],
        }
    }

    pub fn handle_frame(&mut self, now: Instant, source: MacAddress, bytes: &[u8]) {
        let _ = self.controller.handle_frame(now, source, bytes);
        self.take_events();
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.controller.handle_timeout(now);
        self.take_events();
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.controller.poll_timeout()
    }

    /// The next frame to send: its destination, and the frame from the
    /// AVTP subtype on.
    pub fn poll_transmit(&mut self) -> Option<(MacAddress, Vec<u8>)> {
        let transmit = self.controller.poll_transmit(&mut self.buffer).ok()??;
        Some((
            transmit.destination,
            self.buffer[..transmit.length].to_vec(),
        ))
    }

    /// Whether an entity came, went or changed since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn take_events(&mut self) {
        while let Some(event) = self.controller.poll_event() {
            match event {
                Event::EntityOnline(_)
                | Event::EntityOffline(..)
                | Event::EntityRestarted(_)
                | Event::EntityEnumerated(_)
                | Event::EntityModelChanged(_)
                | Event::EnumerationFailed(..) => self.changed = true,
                _ => {}
            }
        }
    }

    /// The stream inputs and outputs of the entities read so far, other
    /// than those advertised from this computer.
    pub fn found(&self) -> Vec<FoundStream> {
        let mut found = Vec::new();
        for entity in self.controller.entities() {
            if entity.mac == self.mac {
                continue;
            }
            let id = entity.entity_id();
            let Some(model) = self.controller.model(id) else {
                continue;
            };
            let device = model
                .entity_name()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("0x{:016x}", id.0));
            for input in [true, false] {
                for descriptor in model.streams(input) {
                    if descriptor.is_media_clock() {
                        continue;
                    }
                    let index = descriptor.index;
                    let stream = if input {
                        RemoteStream::destination(id, index)
                    } else {
                        RemoteStream::source(id, index)
                    };
                    let descriptor_type = if input {
                        DescriptorType::STREAM_INPUT
                    } else {
                        DescriptorType::STREAM_OUTPUT
                    };
                    let current = model
                        .stream_info(descriptor_type, index)
                        .map(|info| info.stream_format)
                        .unwrap_or(descriptor.current_format)
                        .channels();
                    let mut accepted: Vec<u16> = descriptor
                        .formats()
                        .filter_map(|format| format.channels())
                        .collect();
                    accepted.sort_unstable();
                    accepted.dedup();
                    let busy = input
                        && model
                            .binding(index)
                            .and_then(|binding| binding.talker_stream())
                            .is_some_and(|(talker, _)| !self.is_ours(talker));
                    let name = if descriptor.object_name.is_empty() {
                        device.clone()
                    } else {
                        format!("{device} {}", descriptor.object_name)
                    };
                    found.push(FoundStream {
                        stream,
                        name,
                        accepted,
                        current,
                        busy,
                    });
                }
            }
        }
        found
    }

    /// Whether `talker` is one of this computer's entities, as the
    /// daemon's pool talkers are.
    fn is_ours(&self, talker: EntityId) -> bool {
        is_from(talker, self.mac)
    }
}

/// Whether `entity` has an ID [`crate::entity_id`] makes from `mac`.
fn is_from(entity: EntityId, mac: MacAddress) -> bool {
    let bytes = entity.0.to_be_bytes();
    bytes[..3] == mac.0[..3] && bytes[3] == 0xff && bytes[5..] == mac.0[3..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_computers_entity_ids_are_told_apart() {
        let mac = MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x26]);
        assert!(is_from(crate::entity_id(mac, 0x70), mac));
        assert!(is_from(crate::entity_id(mac, CONTROLLER_INSTANCE), mac));
        assert!(!is_from(EntityId(0x001b_92ff_fe01_23ab), mac));
        let other = MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x27]);
        assert!(!is_from(crate::entity_id(other, 0x70), mac));
    }
}
