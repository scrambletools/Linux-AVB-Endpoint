//! Network devices: the listener inputs (destinations) and talker outputs
//! (sources) on other devices that get a PipeWire device of their own on
//! this computer, so any app plays to a speaker or records from a
//! microphone on the network as it would to a local one.
//!
//! Which of them get one is the interface's file's to say
//! ([`crate::config::Config::shows`]): in "all" mode every compatible one
//! found but those hidden, in "pinned" mode only those pinned. A stream is
//! compatible where it takes one of [`CHANNEL_COUNTS`].

use std::fmt;
use std::str::FromStr;

use atdecc::EntityId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::Config;
use crate::status::DeviceStatus;

/// The channel counts the daemon's network devices and their streams
/// carry, and no others.
pub const CHANNEL_COUNTS: [u16; 4] = [2, 8, 16, 32];

/// The widest of [`CHANNEL_COUNTS`] among `accepted`, a remote stream's
/// formats' channel counts: its device's width. `None` where it takes
/// none of them.
pub fn device_channels(accepted: impl IntoIterator<Item = u16>) -> Option<u16> {
    accepted
        .into_iter()
        .filter(|channels| CHANNEL_COUNTS.contains(channels))
        .max()
}

/// The channel count to set a remote stream to, at connect, where its
/// current format's `current` count is none of [`CHANNEL_COUNTS`]: the
/// smallest of them it accepts that holds the current one, else the
/// widest it accepts, as the reservation follows the format.
pub fn connect_channels(current: u16, accepted: impl IntoIterator<Item = u16>) -> Option<u16> {
    let accepted: Vec<u16> = accepted
        .into_iter()
        .filter(|channels| CHANNEL_COUNTS.contains(channels))
        .collect();
    if accepted.contains(&current) {
        return Some(current);
    }
    accepted
        .iter()
        .copied()
        .filter(|channels| *channels >= current)
        .min()
        .or_else(|| accepted.iter().copied().max())
}

/// Which way a network device goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Way {
    /// A listener input on another device: a sink here that apps play
    /// into.
    Destination,
    /// A talker output on another device: a source here that apps record
    /// from.
    Source,
}

/// One stream on another device: its entity and the index of its
/// STREAM_INPUT descriptor (a destination) or STREAM_OUTPUT descriptor (a
/// source), counted from 0 as AEM counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteStream {
    pub entity: EntityId,
    pub way: Way,
    pub index: u16,
}

impl RemoteStream {
    pub fn destination(entity: EntityId, input: u16) -> Self {
        RemoteStream {
            entity,
            way: Way::Destination,
            index: input,
        }
    }

    pub fn source(entity: EntityId, output: u16) -> Self {
        RemoteStream {
            entity,
            way: Way::Source,
            index: output,
        }
    }

    /// Its device's PipeWire node name, which never changes, so that
    /// WirePlumber puts an app's stream back on it and apps find it by
    /// name: `avb-dest-<entity id>-in-<input>` or
    /// `avb-src-<entity id>-out-<output>`.
    pub fn node_name(&self) -> String {
        match self.way {
            Way::Destination => format!("avb-dest-{:016x}-in-{}", self.entity.0, self.index),
            Way::Source => format!("avb-src-{:016x}-out-{}", self.entity.0, self.index),
        }
    }
}

impl fmt::Display for RemoteStream {
    /// `<entity id>:in<input>` or `<entity id>:out<output>`, as the tool
    /// takes it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let way = match self.way {
            Way::Destination => "in",
            Way::Source => "out",
        };
        write!(formatter, "0x{:016x}:{way}{}", self.entity.0, self.index)
    }
}

impl FromStr for RemoteStream {
    type Err = String;

    /// Takes what [`fmt::Display`] writes: an entity ID in hex, with or
    /// without `0x`, a colon, and `in` or `out` with the index.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (entity, stream) = text
            .split_once(':')
            .ok_or_else(|| format!("{text}: not <entity id>:in<n> or <entity id>:out<n>"))?;
        let entity = parse_entity_id(entity)?;
        let (way, index) = if let Some(index) = stream.strip_prefix("in") {
            (Way::Destination, index)
        } else if let Some(index) = stream.strip_prefix("out") {
            (Way::Source, index)
        } else {
            return Err(format!("{stream}: not in<n> or out<n>"));
        };
        let index = index
            .parse()
            .map_err(|_| format!("{index}: not a stream index"))?;
        Ok(RemoteStream { entity, way, index })
    }
}

/// An entity ID in hex, with or without `0x`.
pub fn parse_entity_id(text: &str) -> Result<EntityId, String> {
    let digits = text
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if digits.is_empty() || digits.len() > 16 {
        return Err(format!("{text}: not an entity ID"));
    }
    u64::from_str_radix(digits, 16)
        .map(EntityId)
        .map_err(|_| format!("{text}: not an entity ID"))
}

/// An entity ID as the file writes it, `0x` and sixteen hex digits.
pub(crate) mod entity_id_text {
    use super::*;

    pub fn serialize<S: Serializer>(id: &EntityId, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{:016x}", id.0))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<EntityId, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_entity_id(&text).map_err(serde::de::Error::custom)
    }
}

/// How a network device stands, as an app shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "kebab-case")]
pub enum DeviceState {
    /// On the network, with no app playing into or recording from it.
    Idle,
    /// Connected to one of the daemon's pool streams for an app.
    Connected,
    /// Bound to another stream, or acquired or locked by another
    /// controller: the daemon leaves it be.
    Busy,
    /// Not on the network; pinned, it keeps its device.
    Offline,
    /// The pool has no stream free for it, or the talkers' addresses ran
    /// out.
    NoTalker,
    /// The bridge has no bandwidth left for it.
    NoBandwidth,
    /// It takes none of [`CHANNEL_COUNTS`].
    NoFormat,
    /// A state this reader does not know, from a newer daemon.
    #[serde(other)]
    Other,
}

/// A stream the daemon found on another device, as its controller read
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundStream {
    pub stream: RemoteStream,
    /// The device's name and the stream's.
    pub name: String,
    /// The channel counts of the formats it takes.
    pub accepted: Vec<u16>,
    /// The channel count of its current format, where known.
    pub current: Option<u16>,
    /// Its current format, where known, which a pool talker connecting
    /// to it takes.
    pub format: Option<atdecc::stream_format::StreamFormat>,
    /// A listener input bound to a talker that is not the daemon's, or
    /// acquired or locked by another controller.
    pub busy: bool,
}

/// The streams on other devices as the status lists them: each one found,
/// whether it has a device here and how it stands; then each the file
/// pins or hides that is not on the network, a pinned one keeping its
/// device. By way, then name.
pub fn statuses(config: &Config, found: &[FoundStream]) -> Vec<DeviceStatus> {
    let mut statuses: Vec<DeviceStatus> = found
        .iter()
        .map(|found| {
            let stream = found.stream;
            let channels = device_channels(found.accepted.iter().copied());
            let shown = channels.is_some() && config.shows(&stream);
            let state = match channels {
                None => DeviceState::NoFormat,
                Some(_) if found.busy => DeviceState::Busy,
                Some(_) => DeviceState::Idle,
            };
            let mut status = listed_status(config, stream, &found.name);
            status.shown = shown;
            status.node = shown.then(|| stream.node_name());
            status.channels = channels.unwrap_or(0);
            status.live_channels = found
                .current
                .map_or(0, |current| current.min(status.channels));
            status.state = state;
            status
        })
        .collect();
    for entry in config.pin.iter().chain(config.hide.iter()) {
        let Some(stream) = entry.stream() else {
            continue;
        };
        if found.iter().any(|found| found.stream == stream)
            || statuses
                .iter()
                .any(|status| status.stream() == Some(stream))
        {
            continue;
        }
        let mut status = listed_status(config, stream, &entry.name);
        status.channels = entry.channels.unwrap_or(0);
        status.shown = status.pinned;
        status.node = status.pinned.then(|| stream.node_name());
        status.state = DeviceState::Offline;
        statuses.push(status);
    }
    statuses.sort_by(|one, other| {
        (one.way, &one.name, &one.entity_id, one.index).cmp(&(
            other.way,
            &other.name,
            &other.entity_id,
            other.index,
        ))
    });
    statuses
}

/// A stream's status as far as the file says it: pinned, remembered or
/// hidden.
fn listed_status(config: &Config, stream: RemoteStream, name: &str) -> DeviceStatus {
    let pinned = config
        .pin
        .iter()
        .find(|entry| entry.stream() == Some(stream));
    DeviceStatus {
        entity_id: format!("0x{:016x}", stream.entity.0),
        way: stream.way,
        index: stream.index,
        name: name.into(),
        pinned: pinned.is_some(),
        remembered: pinned.is_some_and(|entry| entry.remembered),
        hidden: config.is_hidden(&stream),
        ..DeviceStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Shown;

    const SPEAKER: EntityId = EntityId(0x001b_92ff_fe01_23ab);
    const AMP: EntityId = EntityId(0x001b_92ff_fe01_4567);

    fn found(stream: RemoteStream, name: &str, accepted: &[u16]) -> FoundStream {
        FoundStream {
            stream,
            name: name.into(),
            accepted: accepted.to_vec(),
            current: accepted.first().copied(),
            format: None,
            busy: false,
        }
    }

    /// In "all" mode every compatible stream found has a device, but those
    /// hidden; one that takes none of the counts has none and says so; a
    /// busy one keeps its device; sources wait to be pinned.
    #[test]
    fn found_streams_show_as_the_file_says() {
        let mut config = Config::default();
        let living = RemoteStream::destination(SPEAKER, 0);
        let sub = RemoteStream::destination(SPEAKER, 1);
        let mono = RemoteStream::destination(AMP, 0);
        let mic = RemoteStream::source(AMP, 0);
        config.hide(sub, "Speaker Sub");
        let mut busy = found(RemoteStream::destination(AMP, 1), "Amp Zone 2", &[2, 8]);
        busy.busy = true;
        let statuses = statuses(
            &config,
            &[
                found(living, "Speaker Main", &[2, 8]),
                found(sub, "Speaker Sub", &[2]),
                found(mono, "Amp Mono", &[1]),
                found(mic, "Amp Mic", &[2]),
                busy,
            ],
        );
        let by_stream = |stream: RemoteStream| {
            statuses
                .iter()
                .find(|status| status.stream() == Some(stream))
                .unwrap()
        };
        let main = by_stream(living);
        assert!(main.shown && main.state == DeviceState::Idle);
        assert_eq!((main.channels, main.live_channels), (8, 2));
        assert_eq!(main.node.as_deref(), Some("avb-dest-001b92fffe0123ab-in-0"));
        assert!(by_stream(sub).hidden && !by_stream(sub).shown);
        assert_eq!(by_stream(mono).state, DeviceState::NoFormat);
        assert!(!by_stream(mono).shown && by_stream(mono).node.is_none());
        assert!(!by_stream(mic).shown, "sources wait to be pinned");
        let zone = by_stream(RemoteStream::destination(AMP, 1));
        assert!(zone.shown && zone.state == DeviceState::Busy);
        // Destinations come before sources, each by name.
        assert_eq!(statuses.last().unwrap().way, Way::Source);
        assert_eq!(statuses[0].name, "Amp Mono");
    }

    /// A pinned stream off the network keeps its device and says it is
    /// offline; a hidden one off the network is listed, with none.
    #[test]
    fn pinned_streams_off_the_network_keep_their_devices() {
        let mut config = Config::default();
        let living = RemoteStream::destination(SPEAKER, 0);
        let mic = RemoteStream::source(AMP, 0);
        config.pin(living, "Speaker Main", true);
        config.hide(mic, "Amp Mic");
        config.set_shown(Way::Destination, Shown::Pinned);
        let statuses = statuses(&config, &[]);
        assert_eq!(statuses.len(), 2);
        let main = &statuses[0];
        assert_eq!(main.stream(), Some(living));
        assert!(main.shown && main.pinned && main.remembered);
        assert_eq!(main.state, DeviceState::Offline);
        assert!(main.node.is_some());
        let hidden = &statuses[1];
        assert!(hidden.hidden && !hidden.shown && hidden.node.is_none());
        // Found again, it is idle, and in "pinned" mode still shown.
        let back = super::statuses(&config, &[found(living, "Speaker Main", &[8])]);
        assert_eq!(back[0].state, DeviceState::Idle);
        assert!(back[0].shown);
    }

    #[test]
    fn a_device_is_as_wide_as_the_widest_count_its_stream_takes() {
        assert_eq!(device_channels([1, 2, 6, 8]), Some(8));
        assert_eq!(device_channels([2, 32, 16]), Some(32));
        assert_eq!(device_channels([1, 6]), None);
    }

    #[test]
    fn a_connect_takes_the_smallest_count_that_holds_the_current_one() {
        // A 6-channel listener that takes 8, 16 and 32 goes to 8, not 32.
        assert_eq!(connect_channels(6, [2, 6, 8, 16, 32]), Some(8));
        assert_eq!(connect_channels(8, [2, 8, 32]), Some(8));
        // None large enough: the widest it takes.
        assert_eq!(connect_channels(12, [2, 8]), Some(8));
        assert_eq!(connect_channels(1, [1]), None);
    }

    #[test]
    fn node_names_carry_the_entity_and_the_index() {
        let entity = EntityId(0x001b_92ff_fe01_23ab);
        assert_eq!(
            RemoteStream::destination(entity, 0).node_name(),
            "avb-dest-001b92fffe0123ab-in-0"
        );
        assert_eq!(
            RemoteStream::source(entity, 3).node_name(),
            "avb-src-001b92fffe0123ab-out-3"
        );
    }

    #[test]
    fn a_stream_reads_back_as_written() {
        let stream = RemoteStream::destination(EntityId(0x001b_92ff_fe01_23ab), 2);
        assert_eq!(stream.to_string(), "0x001b92fffe0123ab:in2");
        assert_eq!(stream.to_string().parse::<RemoteStream>(), Ok(stream));
        assert_eq!(
            "1b92fffe0123ab:out0".parse::<RemoteStream>(),
            Ok(RemoteStream::source(EntityId(0x001b_92ff_fe01_23ab), 0))
        );
        assert!("0x1:sideways0".parse::<RemoteStream>().is_err());
        assert!("nonsense".parse::<RemoteStream>().is_err());
    }

    #[test]
    fn an_unknown_state_reads_as_other() {
        #[derive(Deserialize)]
        struct Holder {
            state: DeviceState,
        }
        let read: Holder = toml::from_str("state = \"no-bandwidth\"").unwrap();
        assert_eq!(read.state, DeviceState::NoBandwidth);
        let read: Holder = toml::from_str("state = \"sleeping\"").unwrap();
        assert_eq!(read.state, DeviceState::Other);
    }
}
