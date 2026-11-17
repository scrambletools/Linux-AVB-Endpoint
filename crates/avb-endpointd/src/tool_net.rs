//! What the commands that ask other devices for something share: the
//! socket, a controller of their own, `FF-FA` in its entity ID's instance
//! octets beside the daemon's `FF-FB`, and finding a device by its entity
//! ID or name. A controller of theirs reads the models of the devices it
//! looks for and nothing else, and asks them nothing more unless asked.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use atdecc::adp::EntityCapabilities;
use atdecc::controller::{CommandId, Config, Controller, Event, Outcome};
use atdecc::{ADP_ACMP_MULTICAST, EntityId, Instant, MacAddress};
use avb_net::Socket;

/// The instance octet of these commands' controller.
pub const CONTROLLER_INSTANCE: u8 = 0xfa;

/// A device asked for: its entity ID, or its entity name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted {
    Id(EntityId),
    Name(String),
}

impl Wanted {
    /// `0x` and sixteen hex digits, or sixteen alone, is an entity ID;
    /// anything else a name.
    pub fn of(text: &str) -> Self {
        let digits = text.strip_prefix("0x").unwrap_or(text);
        match u64::from_str_radix(digits, 16) {
            Ok(id) if digits.len() == 16 => Wanted::Id(EntityId(id)),
            _ => Wanted::Name(text.to_owned()),
        }
    }

    /// Whether an entity advertised as `id`, with `capabilities`, may be
    /// this one, so its model is worth reading: by ID only that one, by
    /// name those whose memory Address Access reads.
    pub fn may_be(&self, id: EntityId, capabilities: EntityCapabilities) -> bool {
        match self {
            Wanted::Id(wanted) => *wanted == id,
            Wanted::Name(_) => capabilities.contains(EntityCapabilities::ADDRESS_ACCESS_SUPPORTED),
        }
    }

    /// Whether the entity `id`, named `name`, is this one.
    pub fn is(&self, id: EntityId, name: &str) -> bool {
        match self {
            Wanted::Id(wanted) => *wanted == id,
            Wanted::Name(wanted) => wanted == name,
        }
    }
}

/// The controller these commands run: it reads only the models it is
/// told to, registers for nothing and advertises nothing.
pub fn controller(mac: MacAddress) -> Controller {
    let mut config = Config::new(avb_endpointd::entity_id(mac, CONTROLLER_INSTANCE));
    config.enumerate = false;
    config.register_unsolicited = false;
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
    config.advertise = None;
    // Apart from an earlier run's, whose answers may still come.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |since| since.as_nanos() as u64);
    config.first_sequence_id = seed as u16;
    config.random_seed = seed;
    Controller::new(config)
}

/// The AVTP socket on `interface`, joined to ADP and ACMP, and the
/// interface's address.
pub fn open(interface: &str) -> Result<(Socket, MacAddress), String> {
    // Raw sockets come from avb-netraw, as the daemon's do.
    #[cfg(target_os = "linux")]
    let _ = avb_net::broker::use_helper("avb-netraw", &[Path::new("/usr/lib/avb-endpointd")]);
    let mac = avb_net::interfaces()
        .into_iter()
        .find(|found| found.name == interface)
        .map(|found| found.mac)
        .ok_or_else(|| format!("no interface named {interface}"))?;
    let socket =
        Socket::open(interface, atdecc::ETHERTYPE_AVTP).map_err(|error| error.to_string())?;
    socket
        .join_multicast(ADP_ACMP_MULTICAST)
        .map_err(|error| error.to_string())?;
    let _ = socket.keep_payloads_starting(atdecc::avtp::subtype::ADP..=atdecc::avtp::subtype::MAAP);
    Ok((socket, mac))
}

/// A controller on a socket, run a step at a time by a command that does
/// one thing after another.
pub struct Link {
    pub socket: Socket,
    pub controller: Controller,
    started: std::time::Instant,
    buffer: Vec<u8>,
    events: VecDeque<Event>,
}

impl Link {
    pub fn open(interface: &str) -> Result<Self, String> {
        let (socket, mac) = open(interface)?;
        Ok(Link {
            socket,
            controller: controller(mac),
            started: std::time::Instant::now(),
            buffer: vec![0; 1600],
            events: VecDeque::new(),
        })
    }

    pub fn now(&self) -> Instant {
        Instant::from_nanos(self.started.elapsed().as_nanos() as u64)
    }

    /// Runs the controller until it has an event, or until `until`.
    fn step(&mut self, until: Instant) -> Result<(), String> {
        loop {
            let now = self.now();
            self.controller.handle_timeout(now);
            while let Ok(Some(transmit)) = self.controller.poll_transmit(&mut self.buffer) {
                let _ = self
                    .socket
                    .send(transmit.destination, &self.buffer[..transmit.length]);
            }
            self.events
                .extend(std::iter::from_fn(|| self.controller.poll_event()));
            if !self.events.is_empty() || now >= until {
                return Ok(());
            }
            let wake = self
                .controller
                .poll_timeout()
                .map_or(until, |wake| wake.min(until));
            let wait = wake
                .saturating_duration_since(now)
                .clamp(Duration::from_millis(1), Duration::from_millis(200));
            match self.socket.receive(&mut self.buffer, Some(wait)) {
                Ok(Some(received)) => {
                    let now = self.now();
                    let _ = self.controller.handle_frame(
                        now,
                        received.source,
                        &self.buffer[..received.length],
                    );
                }
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    /// Finds the device `wanted` within `within` and reads its model:
    /// returns its entity ID once read.
    pub fn find(&mut self, wanted: &Wanted, within: Duration) -> Result<EntityId, String> {
        match wanted {
            Wanted::Id(id) => self.controller.discover(Some(*id)),
            Wanted::Name(_) => self.controller.discover(None),
        }
        let until = self.now() + within;
        loop {
            self.step(until)?;
            if self.events.is_empty() && self.now() >= until {
                return Err(format!("no device {} found", describe(wanted)));
            }
            while let Some(event) = self.events.pop_front() {
                let now = self.now();
                match event {
                    Event::EntityOnline(id) => {
                        let capabilities = self
                            .controller
                            .entity(id)
                            .map(|entity| entity.adp.entity_capabilities);
                        if let Some(capabilities) = capabilities
                            && wanted.may_be(id, capabilities)
                            && self.controller.model(id).is_none()
                        {
                            self.controller.enumerate(now, id);
                        }
                    }
                    Event::EntityEnumerated(id) => {
                        let name = self
                            .controller
                            .model(id)
                            .and_then(|model| model.entity_name())
                            .unwrap_or_default();
                        if wanted.is(id, name) {
                            return Ok(id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Runs the controller until `command` finishes, `within` at most.
    pub fn wait(&mut self, command: CommandId, within: Duration) -> Result<Outcome, String> {
        let until = self.now() + within;
        loop {
            self.step(until)?;
            while let Some(event) = self.events.pop_front() {
                if let Event::CommandFinished(finished, outcome) = event
                    && finished == command
                {
                    return Ok(outcome);
                }
            }
            if self.now() >= until {
                return Err("no answer".into());
            }
        }
    }
}

/// A device asked for, as it was asked for.
pub fn describe(wanted: &Wanted) -> String {
    match wanted {
        Wanted::Id(id) => format!("0x{:016x}", id.0),
        Wanted::Name(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_named_by_id_or_name() {
        assert_eq!(
            Wanted::of("0x8c1f6436c1200000"),
            Wanted::Id(EntityId(0x8c1f_6436_c120_0000))
        );
        assert_eq!(
            Wanted::of("8c1f6436c1200000"),
            Wanted::Id(EntityId(0x8c1f_6436_c120_0000))
        );
        assert_eq!(Wanted::of("esp-mic"), Wanted::Name("esp-mic".into()));
        assert_eq!(Wanted::of("cafe"), Wanted::Name("cafe".into()));
        let named = Wanted::Name("esp-mic".into());
        assert!(named.may_be(EntityId(1), EntityCapabilities::ADDRESS_ACCESS_SUPPORTED));
        assert!(!named.may_be(EntityId(1), EntityCapabilities::AEM_SUPPORTED));
        assert!(named.is(EntityId(9), "esp-mic") && !named.is(EntityId(9), "esp-amp"));
    }
}
