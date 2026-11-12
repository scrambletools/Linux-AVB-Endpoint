//! Network devices: the listener inputs (destinations) and talker outputs
//! (sources) on other devices that get a PipeWire device of their own on
//! this computer, so any app plays to a speaker or records from a
//! microphone on the network as it would to a local one.
//!
//! Which of them get one is the interface's file's to say
//! ([`crate::config::Config::shows`]): in "all" mode every compatible one
//! found but those hidden, in "pinned" mode only those pinned. A stream is
//! compatible where it takes [`MOST_CHANNELS`] or fewer.
//!
//! The devices have one of [`CHANNEL_COUNTS`]; the pool streams carry
//! whatever a remote stream's current format holds, up to
//! [`MOST_CHANNELS`]. Their AAF and AM824 formats are up-to formats (IEEE
//! 1722.1-2021, 7.3.3), so a pool stream adapts to a remote stream of 6
//! channels, say, rather than the daemon setting the remote stream to 8.

use std::fmt;
use std::str::FromStr;

use atdecc::EntityId;
use atdecc::stream_format::{FormatKind, StreamFormat};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::Config;
use crate::status::DeviceStatus;

/// The channel counts the daemon's network devices have, and no others.
pub const CHANNEL_COUNTS: [u16; 4] = [2, 8, 16, 32];

/// The most channels a pool stream carries, and so a remote stream may
/// have to connect.
pub const MOST_CHANNELS: u16 = 32;

/// A network device's width: the smallest of [`CHANNEL_COUNTS`] that holds
/// the widest of `accepted`, a remote stream's formats' channel counts, of
/// those up to [`MOST_CHANNELS`]. `None` where it takes none of those.
pub fn device_channels(accepted: impl IntoIterator<Item = u16>) -> Option<u16> {
    let widest = accepted
        .into_iter()
        .filter(|channels| (1..=MOST_CHANNELS).contains(channels))
        .max()?;
    CHANNEL_COUNTS.into_iter().find(|count| *count >= widest)
}

/// `format` as an up-to format, where it is AAF or IEC 61883-6: as an
/// entry in a pool stream's formats, it admits the same format with any
/// count of channels up to its own.
pub fn up_to(format: StreamFormat) -> StreamFormat {
    match format.kind() {
        FormatKind::Aaf(_) => StreamFormat(format.0 | AAF_UP_TO),
        FormatKind::Iec61883_6(_) => StreamFormat(format.0 | IEC_UP_TO),
        _ => format,
    }
}

/// `format` without its up-to flag, as a stream's current format.
pub fn exact(format: StreamFormat) -> StreamFormat {
    match format.kind() {
        FormatKind::Aaf(_) => StreamFormat(format.0 & !AAF_UP_TO),
        FormatKind::Iec61883_6(_) => StreamFormat(format.0 & !IEC_UP_TO),
        _ => format,
    }
}

/// The up-to flag of an AAF format, and of an IEC 61883-6 one.
const AAF_UP_TO: u64 = 0x0080_0000_0000_0000;
const IEC_UP_TO: u64 = 0x20 << 24;

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
    /// It takes no format of [`MOST_CHANNELS`] or fewer, or its current
    /// format holds more, which no pool stream carries.
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
    /// The talker output a listener input is bound to, where it is.
    pub bound_to: Option<(EntityId, u16)>,
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
            let too_wide = found.current.is_some_and(|current| current > MOST_CHANNELS);
            let state = match channels {
                None => DeviceState::NoFormat,
                Some(_) if too_wide => DeviceState::NoFormat,
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
            bound_to: None,
        }
    }

    /// In "all" mode every compatible stream found has a device, but those
    /// hidden; a mono one has a device of 2, its one channel live; one of
    /// more channels than a pool stream carries has none and says so; a
    /// busy one keeps its device; sources wait to be pinned.
    #[test]
    fn found_streams_show_as_the_file_says() {
        let mut config = Config::default();
        let living = RemoteStream::destination(SPEAKER, 0);
        let sub = RemoteStream::destination(SPEAKER, 1);
        let mono = RemoteStream::destination(AMP, 0);
        let madi = RemoteStream::destination(AMP, 2);
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
                found(madi, "Amp MADI", &[64]),
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
        let mono = by_stream(mono);
        assert!(mono.shown && mono.state == DeviceState::Idle);
        assert_eq!((mono.channels, mono.live_channels), (2, 1), "1 of 2");
        assert_eq!(by_stream(madi).state, DeviceState::NoFormat);
        assert!(!by_stream(madi).shown && by_stream(madi).node.is_none());
        assert!(!by_stream(mic).shown, "sources wait to be pinned");
        let zone = by_stream(RemoteStream::destination(AMP, 1));
        assert!(zone.shown && zone.state == DeviceState::Busy);
        // Destinations come before sources, each by name.
        assert_eq!(statuses.last().unwrap().way, Way::Source);
        assert_eq!(statuses[0].name, "Amp MADI");
    }

    /// A stream that takes some count up to 32 but is set to more: it keeps
    /// its device, which says no pool stream carries what it holds now.
    #[test]
    fn a_stream_set_wider_than_a_pool_stream_says_no_format() {
        let config = Config::default();
        let wide = RemoteStream::destination(AMP, 0);
        let mut stream = found(wide, "Amp Wide", &[8, 32, 64]);
        stream.current = Some(64);
        let statuses = statuses(&config, &[stream]);
        assert_eq!(statuses[0].channels, 32);
        assert!(statuses[0].shown);
        assert_eq!(statuses[0].state, DeviceState::NoFormat);
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

    /// A device has the smallest of the four counts that holds the widest
    /// count its stream takes, up to 32.
    #[test]
    fn a_device_holds_the_widest_count_its_stream_takes() {
        assert_eq!(device_channels([1, 2, 6, 8]), Some(8));
        assert_eq!(device_channels([2, 32, 16]), Some(32));
        assert_eq!(device_channels([1, 6]), Some(8));
        assert_eq!(device_channels([1]), Some(2));
        assert_eq!(device_channels([12]), Some(16));
        assert_eq!(
            device_channels([8, 64]),
            Some(8),
            "64 is past a pool stream"
        );
        assert_eq!(device_channels([64]), None);
        assert_eq!(device_channels([]), None);
    }

    /// A pool stream's up-to entries admit a remote stream's current
    /// format at any count up to theirs, in AAF and in AM824, as the
    /// bench's devices have them; the current format stays exact. The
    /// pool's formats come from avb-stream, which only the daemon takes.
    #[cfg(feature = "daemon")]
    #[test]
    fn up_to_entries_admit_unusual_counts() {
        use avb_stream::formats;
        let pool: Vec<StreamFormat> = formats::offered(32, false).into_iter().map(up_to).collect();
        let admitted = |format: StreamFormat| pool.iter().any(|entry| entry.admits(format));
        for channels in [1, 2, 6, 8, 12, 32] {
            assert!(admitted(formats::aaf(
                48_000,
                channels,
                32,
                formats::CLASS_A_FRAMES
            )));
            assert!(admitted(formats::aaf(
                48_000,
                channels,
                24,
                formats::CLASS_A_FRAMES
            )));
        }
        // esp-mic's OUT 0 and IN 0: AAF at the class B cadence.
        assert!(admitted(StreamFormat(0x0205_0220_0080_c000)));
        assert!(admitted(StreamFormat(0x0205_0220_0200_c000)));
        // The MOTU 8D's current AM824 format, and one of 6 channels.
        assert!(admitted(StreamFormat(0x00a0_0208_4000_0800)));
        assert!(admitted(formats::am824(48_000, 6)));
        // Past a pool stream, or at a rate it doesn't take.
        assert!(!admitted(formats::aaf(
            48_000,
            33,
            32,
            formats::CLASS_A_FRAMES
        )));
        assert!(
            !admitted(StreamFormat(0x0204_0220_0080_6000)),
            "AAF 44.1 kHz"
        );
        // A current format is exact, and up_to then exact gives it back.
        let six = formats::aaf(48_000, 6, 32, formats::CLASS_A_FRAMES);
        assert_eq!(exact(up_to(six)), six);
        assert_ne!(up_to(six), six);
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
