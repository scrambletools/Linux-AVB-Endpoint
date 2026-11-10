//! The entities to run on an interface, from `<interface>.toml` in the
//! configuration folder: each of this computer's entities there with its
//! model, its streams, their channels and where their audio comes from or
//! goes.
//!
//! One of them is the default entity, this computer on the network for
//! any app: the daemon makes it whenever it runs, from its default values
//! where the file lacks it, and no app can remove it. Any app, or a person
//! by hand, adds and removes the others. The apps and the daemon each
//! change the file with [`edit`], which holds the file's edit lock while
//! it reads, changes and writes it, so none writes over another's change.
//! Reading it takes no lock, as every write replaces the file whole.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use atdecc::EntityId;
use atdecc::stream_format::StreamFormat;
#[cfg(feature = "daemon")]
use avb_stream::audio::{Sink, Source};
use serde::{Deserialize, Serialize};

use crate::devices::{RemoteStream, Way};
use crate::store::{self, LoadError};

/// The entities on one interface, the file's name: wired, with a PTP
/// hardware clock that gptpd stamps gPTP's messages with; and which
/// streams on other devices get a device of their own here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Whether the endpoints may fall back to AVB Lite.
    pub avb_lite: LiteChoice,
    /// Whether, in AVB Lite, a talker with more listeners than it serves
    /// unicast escalates the stream to multicast rather than refusing
    /// them: only where the media VLAN is confined (AVB Lite profile, 6).
    /// Controllers set it with SET_LITE_CONFIG.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub multicast_escalation: bool,
    /// How many listeners a talker serves a stream to unicast in AVB Lite,
    /// 5 when not given, as the profile recommends; at least 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unicast_fanout: Option<u8>,
    /// The VLAN AVB Lite's streams and CVU SRP go in, 2 when not given,
    /// as in AVB; 0 tags them with their priority only, for switches
    /// whose VLANs cannot be set up (AVB Lite profile, 7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_vlan: Option<u16>,
    /// Which listener inputs on other devices get a sink here: every
    /// compatible one but those hidden, the default, or only those pinned.
    #[serde(skip_serializing_if = "Shown::is_all")]
    pub destinations: Shown,
    /// Which talker outputs on other devices get a recording device here:
    /// only those pinned, the default, or every compatible one but those
    /// hidden.
    #[serde(skip_serializing_if = "Shown::is_pinned")]
    pub sources: Shown,
    /// How many talkers the default entity keeps for destinations, 4
    /// when not given: one for each destination an app plays into at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_talkers: Option<u8>,
    /// How many listeners the default entity keeps for sources, 4 when
    /// not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_listeners: Option<u8>,
    #[serde(rename = "endpoint")]
    pub endpoints: Vec<EndpointConfig>,
    /// Streams on other devices that get no device here in "all" mode.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<DeviceEntry>,
    /// Streams on other devices that keep their device here, whether on
    /// the network or not: pinned by a person or an app, or remembered
    /// as an app first used them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pin: Vec<DeviceEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            avb_lite: LiteChoice::default(),
            multicast_escalation: false,
            unicast_fanout: None,
            media_vlan: None,
            destinations: Shown::All,
            sources: Shown::Pinned,
            destination_talkers: None,
            source_listeners: None,
            endpoints: Vec::new(),
            hide: Vec::new(),
            pin: Vec::new(),
        }
    }
}

/// Which streams on other devices one way gets a device of its own here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shown {
    /// Every compatible one found, but those hidden; and those pinned.
    All,
    /// Only those pinned.
    Pinned,
}

impl Shown {
    fn is_all(&self) -> bool {
        *self == Shown::All
    }

    fn is_pinned(&self) -> bool {
        *self == Shown::Pinned
    }
}

/// The talkers or listeners a pool keeps when the file gives no number.
pub const POOL_SIZE: u8 = 4;

/// A stream on another device as the file lists it, to pin or to hide:
/// its entity, `input` for a destination or `output` for a source, and
/// its name, for a person reading the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    #[serde(with = "crate::devices::entity_id_text")]
    pub entity: EntityId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u16>,
    /// The device's name and the stream's, as the daemon last read them.
    /// Only for reading: no node name uses it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Its device's channels, as the daemon last found the stream, so
    /// that a pinned one off the network keeps its device as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    /// Pinned by the daemon as an app first played into or recorded from
    /// it, rather than by a person or an app.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remembered: bool,
}

impl DeviceEntry {
    pub fn new(stream: RemoteStream, name: impl Into<String>) -> Self {
        let (input, output) = match stream.way {
            Way::Destination => (Some(stream.index), None),
            Way::Source => (None, Some(stream.index)),
        };
        DeviceEntry {
            entity: stream.entity,
            input,
            output,
            name: name.into(),
            channels: None,
            remembered: false,
        }
    }

    /// The stream it names: `None` where it names both an input and an
    /// output, or neither.
    pub fn stream(&self) -> Option<RemoteStream> {
        match (self.input, self.output) {
            (Some(input), None) => Some(RemoteStream::destination(self.entity, input)),
            (None, Some(output)) => Some(RemoteStream::source(self.entity, output)),
            _ => None,
        }
    }
}

/// When the endpoints run AVB Lite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiteChoice {
    /// When nothing on the interface answers as an AVB bridge would
    /// (AVB Lite profile, 2.2).
    #[default]
    Auto,
    /// Never.
    Off,
    /// Always, as an operator may set it.
    On,
}

/// The SR class a talker starts its stream in, its formats then leading
/// with that class's own; the format a controller picks decides it after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SrClass {
    /// 8000 frames a second's reservation on priority 3, 2 ms.
    #[default]
    A,
    /// 4000 a second's reservation on priority 2, 12 ms, as a stream to a
    /// Wi-Fi station must be (AVB Wireless profile, 4).
    B,
}

impl SrClass {
    fn is_a(&self) -> bool {
        *self == SrClass::A
    }
}

/// Which way an entity's stream goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
}

/// One of an entity's streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub direction: Direction,
    /// A media clock stream, CRF, rather than audio: it has no channels
    /// and no audio.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clock: bool,
    /// For an input, the entity's clock source: its talkers follow the
    /// clock the input's stream carries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clock_source: bool,
    /// Its name, "Out 1", "In 1", "Media Clock Out 1" and on when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default = "eight")]
    pub channels: u16,
    /// Where its audio comes from or goes: "virtual", its own virtual
    /// audio device, when left out; "silence" or "tone" for an output, or
    /// "tones", a tone of its own on each channel; "discard" for an input,
    /// or "analyze", checked against the tones it carries; "default"; or
    /// a device's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// The device channel its first channel comes from or goes to, where
    /// its audio is a device.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub first_channel: u16,
    /// The stream format a controller last chose, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The max transit time a controller set for an output, in
    /// nanoseconds; its class's default when left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transit: Option<u32>,
    /// For an input, the talker's stream output it is bound to, as
    /// `bound` keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<String>,
    /// For an input, the stream channel each of its audio's channels
    /// plays, as controllers mapped them: a number from 1 for each audio
    /// channel in turn, or - for silence, such as "1 2 2 -"; one to one
    /// when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mappings: Option<String>,
    /// The SR class an output starts in, `"A"` when left out; the format
    /// a controller picks decides it after.
    #[serde(default, skip_serializing_if = "SrClass::is_a")]
    pub class: SrClass,
}

fn is_zero(value: &u16) -> bool {
    *value == 0
}

impl StreamConfig {
    /// A new stream of `direction`, with its own virtual audio device.
    pub fn new(direction: Direction) -> Self {
        StreamConfig {
            direction,
            clock: false,
            clock_source: false,
            name: String::new(),
            channels: 8,
            audio: None,
            first_channel: 0,
            format: None,
            transit: None,
            bound: None,
            mappings: None,
            class: SrClass::A,
        }
    }

    /// A new media clock stream of `direction`.
    pub fn clock(direction: Direction) -> Self {
        StreamConfig {
            clock: true,
            channels: 0,
            ..StreamConfig::new(direction)
        }
    }

    /// The stream format a controller last chose.
    pub fn stream_format(&self) -> Option<StreamFormat> {
        parse_format(self.format.as_deref()?)
    }

    /// The talker and stream output it is bound to.
    pub fn bound_to(&self) -> Option<(EntityId, u16)> {
        parse_binding(self.bound.as_deref()?)
    }

    /// An input's mappings, as `(stream channel, audio channel)` from 0.
    pub fn mapping_pairs(&self) -> Option<Vec<(u16, u16)>> {
        let text = self.mappings.as_deref()?;
        Some(
            text.split_whitespace()
                .enumerate()
                .filter_map(|(audio, stream)| {
                    let stream: u16 = stream.parse().ok()?;
                    Some((stream.checked_sub(1)?, audio as u16))
                })
                .collect(),
        )
    }
}

/// Mappings as `mappings` keeps them, for `channels` audio channels.
pub fn mappings_text(pairs: impl IntoIterator<Item = (u16, u16)>, channels: u16) -> String {
    let mut played = vec![None; usize::from(channels)];
    for (stream, audio) in pairs {
        if let Some(slot) = played.get_mut(usize::from(audio)) {
            *slot = Some(stream);
        }
    }
    played
        .iter()
        .map(|stream| stream.map_or_else(|| "-".to_owned(), |stream| (stream + 1).to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

impl StreamConfig {
    /// One of the default entity's pool streams, the `number`th of its
    /// way counted from 1: of 32 channels, its formats taking 2, 8 and 16
    /// as well, and silent until the daemon connects it to a network
    /// device.
    pub fn pool(direction: Direction, number: usize) -> Self {
        let name = match direction {
            Direction::Out => format!("Destination {number}"),
            Direction::In => format!("Source {number}"),
        };
        StreamConfig {
            name,
            channels: 32,
            audio: Some(POOL_STREAM.into()),
            ..StreamConfig::new(direction)
        }
    }

    /// Whether it is one of the default entity's pool streams, which the
    /// daemon adds for the network devices rather than the file listing
    /// it.
    pub fn is_pool(&self) -> bool {
        self.audio.as_deref() == Some(POOL_STREAM)
    }

    /// Whether its audio is its own virtual device; never a media
    /// clock's.
    pub fn is_virtual(&self) -> bool {
        !self.clock && self.audio.as_deref().is_none_or(|audio| audio == VIRTUAL)
    }

    /// Whether an output sends a tone of its own on each channel.
    pub fn has_tones(&self) -> bool {
        !self.clock && self.audio.as_deref() == Some(TONES)
    }

    /// Whether an input's audio is checked against the tones it carries.
    pub fn is_analyzed(&self) -> bool {
        !self.clock && self.audio.as_deref() == Some(ANALYZE)
    }

    /// Where an output's audio comes from, but for a virtual device.
    #[cfg(feature = "daemon")]
    pub fn source(&self) -> Source {
        source_of(self.audio.as_deref(), self.first_channel)
    }

    /// Where an input's audio goes, but for a virtual device.
    #[cfg(feature = "daemon")]
    pub fn sink(&self) -> Sink {
        sink_of(self.audio.as_deref(), self.first_channel)
    }
}

/// The audio of a stream's own virtual device.
pub const VIRTUAL: &str = "virtual";
/// A pool stream's audio: silence, or a network device's while the
/// daemon connects it to one.
pub const POOL_STREAM: &str = "pool";

/// An output's tones, one on each channel, and an input's analysis.
pub const TONES: &str = "tones";
pub const ANALYZE: &str = "analyze";

/// This computer as an entity, with any number of stream outputs and
/// inputs, as hardware presents itself, each set up on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "EndpointEntry")]
pub struct EndpointConfig {
    /// Kept apart from the others' to keep its entity ID stable, from 0 to
    /// 239; its place in the list when left out. [`edit`] writes it down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u8>,
    /// It is the default entity, written `default = true`: the one no app
    /// can remove, which the daemon makes again where it is removed by
    /// hand.
    #[serde(rename = "default", skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
    /// Its model: the vendor, model name, firmware version and logos
    /// controllers see, from `<model>.toml` in the models folder (see
    /// [`crate::model`]); a generic model when left out or not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub name: String,
    /// Its streams, in order; one output and one input, each with its own
    /// virtual audio device, when there are none.
    #[serde(rename = "stream", skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<StreamConfig>,
}

/// An endpoint as files have it: an entity of any streams, or triib
/// 1.0's talker or listener of one stream, whose stream's settings are
/// the entry's own.
#[derive(Deserialize)]
struct EndpointEntry {
    #[serde(default)]
    kind: EntryKind,
    #[serde(default)]
    instance: Option<u8>,
    #[serde(default, rename = "default")]
    is_default: bool,
    #[serde(default)]
    model: Option<String>,
    name: String,
    #[serde(default = "eight")]
    channels: u16,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    sink: Option<String>,
    #[serde(default)]
    first_channel: u16,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    transit: Option<u32>,
    #[serde(default)]
    bound: Option<String>,
    #[serde(default)]
    class: SrClass,
    #[serde(rename = "stream", default)]
    streams: Vec<StreamConfig>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum EntryKind {
    Talker,
    Listener,
    #[default]
    Device,
}

impl From<EndpointEntry> for EndpointConfig {
    /// A talker or listener is an entity of its one stream, named as the
    /// entity is, with the audio, format, binding and class it had.
    fn from(entry: EndpointEntry) -> Self {
        let one = |direction, audio: Option<String>, otherwise: &str| StreamConfig {
            name: entry.name.clone(),
            channels: entry.channels,
            audio: Some(audio.unwrap_or_else(|| otherwise.into())),
            first_channel: entry.first_channel,
            format: entry.format.clone(),
            ..StreamConfig::new(direction)
        };
        let streams = match entry.kind {
            EntryKind::Talker => vec![StreamConfig {
                transit: entry.transit,
                class: entry.class,
                ..one(Direction::Out, entry.source.clone(), "silence")
            }],
            EntryKind::Listener => vec![StreamConfig {
                bound: entry.bound.clone(),
                ..one(Direction::In, entry.sink.clone(), "discard")
            }],
            EntryKind::Device => entry.streams,
        };
        EndpointConfig {
            instance: entry.instance,
            is_default: entry.is_default,
            model: entry.model,
            name: entry.name,
            streams,
        }
    }
}

/// A binding as `bound` keeps it.
pub fn binding_text(talker: EntityId, output: u16) -> String {
    format!("{talker}:{output}")
}

fn eight() -> u16 {
    8
}

/// The most streams each way: the outputs' stream IDs take an octet.
pub const MOST_STREAMS: usize = 64;

/// The highest instance an entity takes: 240 and up are the apps'
/// controllers' and others'.
pub const MOST_INSTANCE: u8 = 239;

fn parse_binding(text: &str) -> Option<(EntityId, u16)> {
    let (talker, output) = text.rsplit_once(':')?;
    Some((talker.parse().ok()?, output.parse().ok()?))
}

fn parse_format(text: &str) -> Option<StreamFormat> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16)
        .ok()
        .map(StreamFormat)
}

impl EndpointConfig {
    /// An entity named `name` with a stream each way, each with its own
    /// virtual audio device.
    pub fn new(name: impl Into<String>, instance: Option<u8>) -> Self {
        EndpointConfig {
            instance,
            is_default: false,
            model: None,
            name: name.into(),
            streams: vec![
                StreamConfig::new(Direction::Out),
                StreamConfig::new(Direction::In),
            ],
        }
    }

    /// Its streams as listed, a stream each way when none are, no more
    /// than the most each way.
    pub fn stream_configs(&self) -> Vec<StreamConfig> {
        let streams = match self.streams.is_empty() {
            true => EndpointConfig::new("", None).streams,
            false => self.streams.clone(),
        };
        let (mut outputs, mut inputs) = (0, 0);
        streams
            .into_iter()
            .filter(|stream| {
                let count = match stream.direction {
                    Direction::Out => &mut outputs,
                    Direction::In => &mut inputs,
                };
                *count += 1;
                *count <= MOST_STREAMS
            })
            .collect()
    }

    /// Its instance, as its entity ID takes it, when it is `place` in the
    /// list.
    pub fn instance_at(&self, place: usize) -> u8 {
        self.instance.unwrap_or(place as u8).min(MOST_INSTANCE)
    }

    /// The default entity as the daemon makes it, at `instance`: named
    /// `name`, with avb-endpointd's model, a stream each way of two
    /// channels, "Playback" for apps to play into and "Listen" for them
    /// to record from, each with its own virtual audio device.
    pub fn default_entity(name: impl Into<String>, instance: u8) -> Self {
        let stream = |direction, name: &str| StreamConfig {
            name: name.into(),
            channels: 2,
            audio: Some(VIRTUAL.into()),
            ..StreamConfig::new(direction)
        };
        EndpointConfig {
            instance: Some(instance),
            is_default: true,
            model: Some(DEFAULT_MODEL.into()),
            name: name.into(),
            streams: vec![
                stream(Direction::Out, "Playback"),
                stream(Direction::In, "Listen"),
            ],
        }
    }
}

/// The default entity's instance, where no other entity takes it: plaa's
/// own entity's before the default took its place, so the bindings
/// controllers made to plaa's by entity ID and index carry over.
pub const DEFAULT_INSTANCE: u8 = 0x70;

/// The default entity's model, which avb-endpointd's package installs.
pub const DEFAULT_MODEL: &str = "avb-endpointd";

/// This computer's name, which the default entity takes: its host name.
pub fn computer_name() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .ok()
        })
        .unwrap_or_else(|| "This computer".into())
}

/// A source as the file names it.
#[cfg(feature = "daemon")]
fn source_of(audio: Option<&str>, first_channel: u16) -> Source {
    match audio {
        None | Some("silence" | VIRTUAL | TONES | POOL_STREAM) => Source::Silence,
        Some("tone") => Source::Tone {
            hertz: 1000.0,
            level: 0.1,
        },
        Some("default") => Source::Device {
            name: None,
            first_channel,
        },
        Some(name) => Source::Device {
            name: Some(name.to_owned()),
            first_channel,
        },
    }
}

/// A sink as the file names it.
#[cfg(feature = "daemon")]
fn sink_of(audio: Option<&str>, first_channel: u16) -> Sink {
    match audio {
        None | Some("discard" | VIRTUAL | ANALYZE | POOL_STREAM) => Sink::Discard,
        Some("default") => Sink::Device {
            name: None,
            first_channel,
        },
        Some(name) => Sink::Device {
            name: Some(name.to_owned()),
            first_channel,
        },
    }
}

impl Config {
    /// Writes down each entity's instance, which its place in the list
    /// gives one without, so removing an entry above it leaves its entity
    /// ID as it is.
    pub fn pin_instances(&mut self) {
        for (place, endpoint) in self.endpoints.iter_mut().enumerate() {
            endpoint.instance = Some(endpoint.instance_at(place));
        }
    }

    /// The lowest instance no entity takes, for one an app adds: `None`
    /// when all are taken.
    pub fn free_instance(&self) -> Option<u8> {
        let taken: Vec<u8> = self
            .endpoints
            .iter()
            .enumerate()
            .map(|(place, endpoint)| endpoint.instance_at(place))
            .collect();
        (0..=MOST_INSTANCE).find(|instance| !taken.contains(instance))
    }

    /// The entity with `instance`, as its entity ID takes it.
    pub fn entity(&self, instance: u8) -> Option<&EndpointConfig> {
        self.endpoints
            .iter()
            .enumerate()
            .find(|(place, endpoint)| endpoint.instance_at(*place) == instance)
            .map(|(_, endpoint)| endpoint)
    }

    /// The entity with `instance`, to change.
    pub fn entity_mut(&mut self, instance: u8) -> Option<&mut EndpointConfig> {
        self.endpoints
            .iter_mut()
            .enumerate()
            .find(|(place, endpoint)| endpoint.instance_at(*place) == instance)
            .map(|(_, endpoint)| endpoint)
    }

    /// The default entity, where the file has it.
    pub fn default_entity(&self) -> Option<&EndpointConfig> {
        self.endpoints.iter().find(|endpoint| endpoint.is_default)
    }

    /// The default entity, to change, as a controller renames it.
    pub fn default_entity_mut(&mut self) -> Option<&mut EndpointConfig> {
        self.endpoints
            .iter_mut()
            .find(|endpoint| endpoint.is_default)
    }

    /// Makes the default entity from its default values where the file
    /// has none, named what `name` gives, at [`DEFAULT_INSTANCE`] or,
    /// where another entity takes that, the lowest free instance; and
    /// keeps only the first entity marked default as the default. Whether
    /// it made one.
    pub fn ensure_default(&mut self, name: impl FnOnce() -> String) -> bool {
        self.keep_one_default();
        if self.default_entity().is_some() {
            return false;
        }
        let instance = match self.entity(DEFAULT_INSTANCE) {
            None => DEFAULT_INSTANCE,
            Some(_) => match self.free_instance() {
                Some(free) => free,
                None => return false,
            },
        };
        // The others' instances written down first, as the default goes
        // ahead of them.
        self.pin_instances();
        self.endpoints
            .insert(0, EndpointConfig::default_entity(name(), instance));
        true
    }

    /// Which streams one way gets a device here.
    pub fn shown(&self, way: Way) -> Shown {
        match way {
            Way::Destination => self.destinations,
            Way::Source => self.sources,
        }
    }

    /// Sets which streams one way gets a device here; whether that
    /// changed it.
    pub fn set_shown(&mut self, way: Way, shown: Shown) -> bool {
        let mode = match way {
            Way::Destination => &mut self.destinations,
            Way::Source => &mut self.sources,
        };
        let changed = *mode != shown;
        *mode = shown;
        changed
    }

    /// The talkers the default entity keeps for destinations.
    pub fn talker_pool(&self) -> u8 {
        self.destination_talkers.unwrap_or(POOL_SIZE)
    }

    /// The listeners the default entity keeps for sources.
    pub fn listener_pool(&self) -> u8 {
        self.source_listeners.unwrap_or(POOL_SIZE)
    }

    /// Whether any stream on another device may get a device here, so
    /// the daemon looks for them: in "all" mode either way, or pinned.
    pub fn wants_devices(&self) -> bool {
        self.destinations == Shown::All || self.sources == Shown::All || !self.pin.is_empty()
    }

    /// Whether `stream`, found on the network, gets a device here: pinned,
    /// or in "all" mode and not hidden. A pinned one keeps its device off
    /// the network too.
    pub fn shows(&self, stream: &RemoteStream) -> bool {
        self.is_pinned(stream) || (self.shown(stream.way) == Shown::All && !self.is_hidden(stream))
    }

    pub fn is_pinned(&self, stream: &RemoteStream) -> bool {
        listed(&self.pin, stream).is_some()
    }

    pub fn is_hidden(&self, stream: &RemoteStream) -> bool {
        listed(&self.hide, stream).is_some()
    }

    /// Pins `stream`, named `name`, so its device stays whether it is on
    /// the network or not, and shows it where it was hidden. A person's or
    /// an app's pin, not `remembered`, makes a remembered one their own.
    /// Whether that changed anything.
    pub fn pin(&mut self, stream: RemoteStream, name: &str, remembered: bool) -> bool {
        let mut changed = unlist(&mut self.hide, &stream);
        match self
            .pin
            .iter_mut()
            .find(|entry| entry.stream() == Some(stream))
        {
            Some(entry) => {
                if entry.remembered && !remembered {
                    entry.remembered = false;
                    changed = true;
                }
                if entry.name.is_empty() && !name.is_empty() {
                    entry.name = name.into();
                    changed = true;
                }
            }
            None => {
                self.pin.push(DeviceEntry {
                    remembered,
                    ..DeviceEntry::new(stream, name)
                });
                changed = true;
            }
        }
        changed
    }

    /// Unpins `stream`, remembered or not; whether it was pinned.
    pub fn unpin(&mut self, stream: &RemoteStream) -> bool {
        unlist(&mut self.pin, stream)
    }

    /// Hides `stream`, named `name`, and unpins it, so it gets no device
    /// here in either mode; whether that changed anything.
    pub fn hide(&mut self, stream: RemoteStream, name: &str) -> bool {
        let unpinned = unlist(&mut self.pin, &stream);
        if listed(&self.hide, &stream).is_some() {
            return unpinned;
        }
        self.hide.push(DeviceEntry::new(stream, name));
        true
    }

    /// Shows `stream` again where it was hidden; whether it was.
    pub fn show(&mut self, stream: &RemoteStream) -> bool {
        unlist(&mut self.hide, stream)
    }

    /// Keeps `stream`'s name and its device's channels current wherever
    /// the file lists it, as the daemon found it; whether that changed
    /// anything.
    pub fn note_stream(&mut self, stream: &RemoteStream, name: &str, channels: u16) -> bool {
        let mut changed = false;
        for entry in self.pin.iter_mut().chain(self.hide.iter_mut()) {
            if entry.stream().as_ref() != Some(stream) {
                continue;
            }
            if !name.is_empty() && entry.name != name {
                entry.name = name.into();
                changed = true;
            }
            if channels > 0 && entry.channels != Some(channels) {
                entry.channels = Some(channels);
                changed = true;
            }
        }
        changed
    }

    /// The settings of the port itself and of the default entity's pools,
    /// as against the entities and the network devices: a change to them
    /// starts the daemon again.
    pub fn port_settings(&self) -> (LiteChoice, bool, Option<u8>, Option<u16>, u8, u8) {
        (
            self.avb_lite,
            self.multicast_escalation,
            self.unicast_fanout,
            self.media_vlan,
            self.talker_pool(),
            self.listener_pool(),
        )
    }

    /// Keeps the first entity marked default as the default, the others
    /// ordinary.
    fn keep_one_default(&mut self) {
        let mut seen = false;
        for endpoint in &mut self.endpoints {
            if endpoint.is_default {
                endpoint.is_default = !seen;
                seen = true;
            }
        }
    }
}

fn listed<'a>(entries: &'a [DeviceEntry], stream: &RemoteStream) -> Option<&'a DeviceEntry> {
    entries
        .iter()
        .find(|entry| entry.stream().as_ref() == Some(stream))
}

fn unlist(entries: &mut Vec<DeviceEntry>, stream: &RemoteStream) -> bool {
    let before = entries.len();
    entries.retain(|entry| entry.stream().as_ref() != Some(stream));
    entries.len() != before
}

/// Where the entities on `interface` are listed: `<interface>.toml` in the
/// configuration folder.
pub fn path(interface: &str) -> Option<PathBuf> {
    store::config_dir().map(|dir| dir.join(format!("{interface}.toml")))
}

/// The entities listed at `path`, without the edit lock: as some write
/// left the file, whole.
pub fn load(path: &Path) -> Result<Config, LoadError> {
    store::load(path)
}

/// Changes the entities on `interface`, as [`edit_at`] does with its file.
pub fn edit<R>(interface: &str, change: impl FnOnce(&mut Config) -> R) -> io::Result<R> {
    let path = path(interface).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no configuration folder: neither XDG_CONFIG_HOME nor HOME is set",
        )
    })?;
    edit_at(&path, change)
}

/// Reads the entities at `path`, none where there is no file, lets
/// `change` change them, and writes them back where it changed anything,
/// with every entity's instance written down, all under the file's edit
/// lock, so no other app's or the daemon's change in between is lost. A
/// file that does not parse is an `InvalidData` error and stays as it is.
///
/// The default entity stays, whatever `change` does to the list, even
/// replacing it whole: an app may rename it or change its streams'
/// settings, but not remove it, nor make another the default.
pub fn edit_at<R>(path: &Path, change: impl FnOnce(&mut Config) -> R) -> io::Result<R> {
    let _lock = edit_lock(path)?;
    let before = match store::load::<Config>(path) {
        Ok(config) => config,
        Err(error) if error.is_missing() => Config::default(),
        Err(LoadError::Io(error)) => return Err(error),
        Err(LoadError::Parse(error)) => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }
    };
    let mut config = before.clone();
    let default = before
        .endpoints
        .iter()
        .enumerate()
        .find(|(_, endpoint)| endpoint.is_default)
        .map(|(place, endpoint)| (place, endpoint.instance_at(place), endpoint.clone()));
    let result = change(&mut config);
    if let Some((place, instance, entity)) = default {
        config.keep_one_default();
        let ours = config.default_entity().map(|kept| kept.instance) == Some(entity.instance);
        if !ours {
            // Put back as it was: marked again where only its mark went,
            // else the entry itself, where it stood.
            for endpoint in &mut config.endpoints {
                endpoint.is_default = false;
            }
            match config.entity_mut(instance) {
                Some(kept) => kept.is_default = true,
                None => {
                    let place = place.min(config.endpoints.len());
                    config.endpoints.insert(place, entity);
                }
            }
        }
    } else {
        // Only the daemon makes the default.
        for endpoint in &mut config.endpoints {
            endpoint.is_default = false;
        }
    }
    config.pin_instances();
    if config != before {
        store::save(path, &config)?;
    }
    Ok(result)
}

/// Holds the edit lock of the file at `path`, `<file>.lock` beside it,
/// until dropped: a file of its own, as each write replaces the file
/// itself. It is not the daemon's lock, which it holds while it runs.
pub(crate) fn edit_lock(path: &Path) -> io::Result<File> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_os_string();
    name.push(".lock");
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_file_name(name))?;
    file.lock()?;
    Ok(file)
}

/// triib's list of entities from before the daemon was its own, for one
/// interface, as `entities.toml` in triib's data folder held it, or
/// triib 1.0's `endpoints.toml`.
#[derive(Deserialize)]
struct TriibInterface {
    #[serde(default)]
    interface: String,
}

/// triib's entities for `interface`, from its data folder, their
/// instances written down: `None` when triib listed none there, or for
/// another interface.
pub fn triib_entities(interface: &str) -> Option<Config> {
    triib_entities_in(&store::triib_data_dir()?, interface)
}

fn triib_entities_in(dir: &Path, interface: &str) -> Option<Config> {
    let path = ["entities.toml", "endpoints.toml"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())?;
    triib_entities_at(&path, interface)
}

/// triib's entities for `interface` from the file at `path`, as triib
/// kept them where a person chose another file for them: each with
/// triib's model, and its instance written down.
pub fn triib_entities_at(path: &Path, interface: &str) -> Option<Config> {
    let named: TriibInterface = store::load(path).ok()?;
    let mut config: Config = store::load(path).ok()?;
    if named.interface != interface || config.endpoints.is_empty() {
        return None;
    }
    config.pin_instances();
    for endpoint in &mut config.endpoints {
        endpoint.model.get_or_insert_with(|| "triib".into());
    }
    Some(config)
}

/// The entities to run from the file at `path`, as the daemon first reads
/// it, under the edit lock: with no file, triib's for `interface`; with
/// the default entity, made from its default values where there is none,
/// named `name`; each entity's instance written down; written back where
/// any of that changed it.
#[cfg(feature = "daemon")]
pub fn first_read(path: &Path, interface: &str, name: &str) -> Result<Config, LoadError> {
    let _lock = edit_lock(path).map_err(LoadError::Io)?;
    let (mut config, before) = match store::load::<Config>(path) {
        Ok(config) => (config.clone(), Some(config)),
        Err(error) if error.is_missing() => (triib_entities(interface).unwrap_or_default(), None),
        Err(error) => return Err(error),
    };
    config.ensure_default(|| name.to_owned());
    config.pin_instances();
    if before.as_ref() != Some(&config) {
        store::save(path, &config).map_err(LoadError::Io)?;
    }
    Ok(config)
}

/// Makes the default entity again from its default values, named `name`,
/// where the file at `path` lacks it, as when a person removed it by
/// hand, under the edit lock: the entities then, and whether it made it.
#[cfg(feature = "daemon")]
pub fn restore_default(path: &Path, name: &str) -> io::Result<(Config, bool)> {
    let _lock = edit_lock(path)?;
    let mut config = match store::load::<Config>(path) {
        Ok(config) => config,
        Err(error) if error.is_missing() => Config::default(),
        Err(LoadError::Io(error)) => return Err(error),
        Err(LoadError::Parse(error)) => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }
    };
    let made = config.ensure_default(|| name.to_owned());
    if made {
        config.pin_instances();
        store::save(path, &config)?;
    }
    Ok((config, made))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> EndpointConfig {
        let config: Config = toml::from_str(text).expect("parses");
        config.endpoints.into_iter().next().expect("an endpoint")
    }

    /// triib 1.0's talkers and listeners read as entities of their one
    /// stream, named as they are, with their audio, format, binding,
    /// transit time and class; written again as such. An entity without
    /// streams has one each way, virtual.
    #[test]
    fn talkers_and_listeners_read_as_devices() {
        let talker = entry(
            "[[endpoint]]\nkind = \"talker\"\ninstance = 3\nname = \"T\"\nsource = \"tone\"\n\
             channels = 2\nformat = \"0x0205022002006000\"\ntransit = 5000000\nclass = \"B\"\n",
        );
        assert_eq!((talker.instance, talker.name.as_str()), (Some(3), "T"));
        let [stream] = talker.streams.as_slice() else {
            panic!("{talker:?}");
        };
        assert_eq!(
            (stream.direction, stream.name.as_str(), stream.channels),
            (Direction::Out, "T", 2)
        );
        assert_eq!(stream.audio.as_deref(), Some("tone"));
        assert_eq!(
            stream.stream_format(),
            Some(StreamFormat(0x0205_0220_0200_6000))
        );
        assert_eq!(
            (stream.transit, stream.class),
            (Some(5_000_000), SrClass::B)
        );
        let listener = entry(
            "[[endpoint]]\nkind = \"listener\"\nname = \"L\"\nbound = \"0xf0a731ff00f40f14:0\"\n",
        );
        let [stream] = listener.streams.as_slice() else {
            panic!("{listener:?}");
        };
        assert_eq!(stream.direction, Direction::In);
        #[cfg(feature = "daemon")]
        assert_eq!(stream.sink(), Sink::Discard);
        assert_eq!(
            stream.bound_to(),
            Some((EntityId(0xf0a7_31ff_00f4_0f14), 0))
        );
        let text = toml::to_string(&Config {
            endpoints: vec![talker.clone()],
            ..Config::default()
        })
        .expect("writes");
        assert!(!text.contains("kind"), "{text}");
        assert_eq!(entry(&text), talker);
        let device = entry("[[endpoint]]\nname = \"D\"\n");
        let streams = device.stream_configs();
        assert_eq!(
            streams
                .iter()
                .map(|stream| stream.direction)
                .collect::<Vec<_>>(),
            [Direction::Out, Direction::In]
        );
        assert!(streams.iter().all(StreamConfig::is_virtual));
    }

    /// An entity's streams each keep their own settings.
    #[test]
    fn a_devices_streams_keep_their_own_settings() {
        let device = entry(
            "[[endpoint]]\nkind = \"device\"\nname = \"D\"\n\
             [[endpoint.stream]]\ndirection = \"out\"\nchannels = 2\nname = \"Mix\"\n\
             [[endpoint.stream]]\ndirection = \"in\"\naudio = \"default\"\nfirst_channel = 4\n\
             format = \"0x0205022002006000\"\nbound = \"0xf0a731ff00f40f14:0\"\n",
        );
        let streams = device.stream_configs();
        assert_eq!((streams[0].channels, streams[0].name.as_str()), (2, "Mix"));
        assert!(streams[0].is_virtual());
        assert_eq!(
            streams[1].stream_format(),
            Some(StreamFormat(0x0205_0220_0200_6000))
        );
        assert_eq!(
            streams[1].bound_to(),
            Some((EntityId(0xf0a7_31ff_00f4_0f14), 0))
        );
        #[cfg(feature = "daemon")]
        assert_eq!(
            streams[1].sink(),
            Sink::Device {
                name: None,
                first_channel: 4
            }
        );
        let text = toml::to_string(&Config {
            endpoints: vec![device.clone()],
            ..Config::default()
        })
        .expect("writes");
        assert_eq!(entry(&text), device);
    }

    /// Test tones and analysis are neither virtual nor a device, and send
    /// silence or play nowhere where their threads can't start.
    #[test]
    fn tones_and_analysis_are_their_own() {
        let device = entry(
            "[[endpoint]]\nkind = \"device\"\nname = \"Test\"\n\
             [[endpoint.stream]]\ndirection = \"out\"\naudio = \"tones\"\n\
             [[endpoint.stream]]\ndirection = \"in\"\naudio = \"analyze\"\n",
        );
        let streams = device.stream_configs();
        assert!(streams[0].has_tones() && !streams[0].is_virtual());
        assert!(streams[1].is_analyzed() && !streams[1].is_virtual());
        #[cfg(feature = "daemon")]
        {
            assert_eq!(streams[0].source(), Source::Silence);
            assert_eq!(streams[1].sink(), Sink::Discard);
        }
        assert!(!streams[1].has_tones() && !streams[0].is_analyzed());
    }

    /// triib's entities come over for the interface triib listed them
    /// on, with their instances written down so their entity IDs stay;
    /// none for another interface. triib 1.0's endpoints.toml counts
    /// where entities.toml is not there. triib's file stays as it is.
    #[test]
    fn triibs_entities_come_over_for_their_interface() {
        let dir = std::env::temp_dir().join(format!("avb-triib-entities-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(triib_entities_in(&dir, "enp2s0").is_none());
        let text = "interface = \"enp2s0\"\navb_lite = \"off\"\n\
                    [[endpoint]]\nname = \"A\"\n[[endpoint]]\ninstance = 7\nname = \"B\"\n\
                    [[endpoint]]\nname = \"C\"\n";
        std::fs::write(dir.join("endpoints.toml"), text).unwrap();
        let config = triib_entities_in(&dir, "enp2s0").expect("imported");
        assert_eq!(config.avb_lite, LiteChoice::Off);
        let instances: Vec<Option<u8>> = config
            .endpoints
            .iter()
            .map(|entry| entry.instance)
            .collect();
        assert_eq!(instances, [Some(0), Some(7), Some(2)]);
        assert!(triib_entities_in(&dir, "enp6s0").is_none());
        std::fs::write(
            dir.join("entities.toml"),
            "interface = \"enp6s0\"\n[[endpoint]]\nname = \"D\"\n",
        )
        .unwrap();
        assert!(triib_entities_in(&dir, "enp2s0").is_none());
        assert_eq!(
            triib_entities_in(&dir, "enp6s0")
                .expect("imported")
                .endpoints[0]
                .name,
            "D"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("endpoints.toml")).unwrap(),
            text
        );
        // Written again, the interface is the file's name, not a setting.
        let written = toml::to_string(&config).unwrap();
        assert!(!written.contains("interface"), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avb-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The default entity is marked as such, with its model, and comes
    /// back; the others are not.
    #[test]
    fn the_default_entity_comes_back() {
        let default = EndpointConfig::default_entity("desk", DEFAULT_INSTANCE);
        let by_hand = entry("[[endpoint]]\ninstance = 3\nname = \"Mine\"\n");
        let config = Config {
            endpoints: vec![default.clone(), by_hand.clone()],
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert_eq!(text.matches("default = true").count(), 1, "{text}");
        assert!(text.find("default = true").unwrap() < text.find("name").unwrap());
        let read: Config = toml::from_str(&text).unwrap();
        assert_eq!(read, config);
        let found = read.default_entity().expect("a default");
        assert_eq!(found.instance, Some(0x70));
        assert_eq!(found.model.as_deref(), Some(DEFAULT_MODEL));
        let streams: Vec<_> = found
            .streams
            .iter()
            .map(|stream| (stream.direction, stream.name.as_str(), stream.channels))
            .collect();
        assert_eq!(
            streams,
            [
                (Direction::Out, "Playback", 2),
                (Direction::In, "Listen", 2)
            ]
        );
        assert!(found.streams.iter().all(StreamConfig::is_virtual));
        assert!(!read.endpoints[1].is_default);
        assert_eq!(
            read.entity(3).map(|entry| entry.name.as_str()),
            Some("Mine")
        );
        assert_eq!(read.free_instance(), Some(0));
        assert!(!computer_name().is_empty());
    }

    /// The default is made at 0x70 where no other takes it, else at the
    /// lowest free instance, ahead of the others, whose instances stay;
    /// where the file has one, nothing is made, and only the first marked
    /// stays the default.
    #[test]
    fn the_default_is_made_where_there_is_none() {
        let mut config = Config {
            endpoints: vec![EndpointConfig::new("A", None)],
            ..Config::default()
        };
        assert!(config.ensure_default(|| "desk".into()));
        assert_eq!(config.endpoints[0].instance, Some(DEFAULT_INSTANCE));
        assert_eq!(config.endpoints[0].name, "desk");
        assert_eq!(config.endpoints[1].instance, Some(0));
        assert!(!config.ensure_default(|| unreachable!()));
        let mut taken = Config {
            endpoints: vec![EndpointConfig::new("A", Some(DEFAULT_INSTANCE))],
            ..Config::default()
        };
        assert!(taken.ensure_default(|| "desk".into()));
        assert_eq!(taken.default_entity().unwrap().instance, Some(0));
        let mut twice = Config {
            endpoints: vec![
                EndpointConfig::default_entity("one", 0x70),
                EndpointConfig::default_entity("two", 0x71),
            ],
            ..Config::default()
        };
        assert!(!twice.ensure_default(|| unreachable!()));
        assert_eq!(twice.default_entity().unwrap().name, "one");
        assert!(!twice.endpoints[1].is_default);
    }

    /// No edit removes the default entity, whether it removes its entry,
    /// replaces the whole list, as a preset does, or takes its mark; nor
    /// makes another the default. It may rename it.
    #[test]
    fn an_edit_keeps_the_default_entity() {
        let dir = scratch("keep-default");
        let path = dir.join("enp2s0.toml");
        let start = Config {
            endpoints: vec![
                EndpointConfig::new("A", Some(0)),
                EndpointConfig::default_entity("desk", DEFAULT_INSTANCE),
            ],
            ..Config::default()
        };
        crate::store::save(&path, &start).unwrap();
        edit_at(&path, |config| {
            config.default_entity_mut().unwrap().name = "studio".into();
        })
        .unwrap();
        edit_at(&path, |config| config.endpoints.clear()).unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(config.endpoints[0].name, "studio");
        assert!(config.endpoints[0].is_default);
        edit_at(&path, |config| {
            config.endpoints = vec![EndpointConfig::new("Preset", Some(4))];
        })
        .unwrap();
        let names: Vec<_> = load(&path)
            .unwrap()
            .endpoints
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        assert_eq!(names, ["studio", "Preset"]);
        edit_at(&path, |config| {
            for entry in &mut config.endpoints {
                entry.is_default = !entry.is_default;
            }
        })
        .unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.endpoints.len(), 2);
        assert_eq!(config.default_entity().unwrap().name, "studio");
        assert!(!config.endpoints[1].is_default);
        // Where there is no default, as before the daemon first ran, an
        // app can't make one.
        let fresh = dir.join("enp6s0.toml");
        edit_at(&fresh, |config| {
            config
                .endpoints
                .push(EndpointConfig::default_entity("mine", 0x70));
        })
        .unwrap();
        assert!(load(&fresh).unwrap().default_entity().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Apps and the daemon each changing their own entities at once lose
    /// none of the others' changes, as each holds the edit lock from
    /// reading the file to writing it; every instance ends up written
    /// down.
    #[test]
    fn edits_at_once_keep_each_others_changes() {
        let dir = scratch("edits");
        let path = dir.join("enp2s0.toml");
        // The file as triib wrote it before instances were written down.
        std::fs::write(&path, "[[endpoint]]\nname = \"T\"\n").unwrap();
        let editors: Vec<_> = (0..4u8)
            .map(|editor| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for count in 1..=20 {
                        edit_at(&path, |config| {
                            let instance = 100 + editor;
                            let name = format!("{editor} {count}");
                            match config.entity_mut(instance) {
                                Some(entry) => entry.name = name,
                                None => config
                                    .endpoints
                                    .push(EndpointConfig::new(name, Some(instance))),
                            }
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for editor in editors {
            editor.join().unwrap();
        }
        let config = load(&path).unwrap();
        assert_eq!(config.endpoints.len(), 5);
        for editor in 0..4u8 {
            let entry = config.entity(100 + editor).expect("kept");
            assert_eq!(entry.name, format!("{editor} 20"));
        }
        assert_eq!(config.endpoints[0].instance, Some(0));
        assert!(
            config
                .endpoints
                .iter()
                .all(|entry| entry.instance.is_some())
        );
        // An edit that changes nothing leaves the file alone.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        edit_at(&path, |_| ()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An edit makes the file where there was none, and leaves one that
    /// doesn't parse as it is, saying so.
    #[test]
    fn an_edit_makes_the_file_and_spares_a_broken_one() {
        let dir = scratch("make");
        let path = dir.join("sub").join("enp2s0.toml");
        edit_at(&path, |config| {
            config.endpoints.push(EndpointConfig::new("New", None));
        })
        .unwrap();
        assert_eq!(load(&path).unwrap().endpoints[0].instance, Some(0));
        std::fs::write(&path, "[[endpoint]\n").unwrap();
        let error = edit_at(&path, |config| config.endpoints.clear()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[endpoint]\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// triib's entities come over with triib's model, where its file
    /// names none.
    #[test]
    fn triibs_entities_come_over_with_triibs_model() {
        let dir = scratch("triib-model");
        std::fs::write(
            dir.join("entities.toml"),
            "interface = \"enp2s0\"\n[[endpoint]]\nname = \"A\"\n\
             [[endpoint]]\nmodel = \"other\"\nname = \"B\"\n",
        )
        .unwrap();
        let config = triib_entities_in(&dir, "enp2s0").expect("imported");
        let models: Vec<_> = config
            .endpoints
            .iter()
            .map(|entry| entry.model.as_deref())
            .collect();
        assert_eq!(models, [Some("triib"), Some("other")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon's first read adds the default entity and writes down the
    /// instances a file left to the entities' places, and leaves a file
    /// that has both alone; with no file, it holds the default alone where
    /// triib listed nothing. A default removed by hand is made again from
    /// its default values.
    #[cfg(feature = "daemon")]
    #[test]
    fn the_first_read_adds_the_default_and_pins_instances() {
        let dir = scratch("first-read");
        let path = dir.join("enp2s0.toml");
        std::fs::write(
            &path,
            "[[endpoint]]\nname = \"A\"\n[[endpoint]]\ninstance = 7\nname = \"B\"\n",
        )
        .unwrap();
        let config = first_read(&path, "test-none", "desk").unwrap();
        let instances: Vec<_> = config
            .endpoints
            .iter()
            .map(|entry| (entry.instance, entry.is_default))
            .collect();
        assert_eq!(
            instances,
            [(Some(0x70), true), (Some(0), false), (Some(7), false)]
        );
        assert_eq!(load(&path).unwrap(), config);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(first_read(&path, "test-none", "desk").unwrap(), config);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // Renamed, then removed by hand: made again as it first was.
        edit_at(&path, |config| {
            config.default_entity_mut().unwrap().name = "studio".into();
        })
        .unwrap();
        let mut by_hand = load(&path).unwrap();
        by_hand.endpoints.retain(|entry| !entry.is_default);
        crate::store::save(&path, &by_hand).unwrap();
        let (restored, made) = restore_default(&path, "desk").unwrap();
        assert!(made);
        assert_eq!(restored, config);
        assert!(!restore_default(&path, "desk").unwrap().1);
        let fresh = dir.join("enp6s0.toml");
        let alone = first_read(&fresh, "test-none", "desk").unwrap();
        assert_eq!(alone.endpoints.len(), 1);
        assert!(alone.endpoints[0].is_default);
        std::fs::write(&path, "[[endpoint]\n").unwrap();
        assert!(matches!(
            first_read(&path, "test-none", "desk"),
            Err(LoadError::Parse(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A media clock stream says so, and an audio stream says nothing of
    /// it.
    #[test]
    fn a_media_clock_stream_is_marked() {
        let device = entry(
            "[[endpoint]]\nkind = \"device\"\nname = \"D\"\n\
             [[endpoint.stream]]\ndirection = \"out\"\nclock = true\n\
             [[endpoint.stream]]\ndirection = \"in\"\n",
        );
        let streams = device.stream_configs();
        assert!(streams[0].clock && !streams[0].is_virtual());
        assert!(!streams[1].clock && streams[1].is_virtual());
        let text = toml::to_string(&Config {
            endpoints: vec![device.clone()],
            ..Config::default()
        })
        .expect("writes");
        assert_eq!(text.matches("clock = true").count(), 1, "{text}");
        assert_eq!(entry(&text), device);
    }

    const SPEAKER: EntityId = EntityId(0x001b_92ff_fe01_23ab);

    /// With nothing set, destinations show in "all" mode and sources only
    /// pinned, and the file says neither.
    #[test]
    fn network_devices_default_to_all_destinations_and_pinned_sources() {
        let config = Config::default();
        let input = RemoteStream::destination(SPEAKER, 0);
        let output = RemoteStream::source(SPEAKER, 0);
        assert!(config.shows(&input));
        assert!(!config.shows(&output));
        assert_eq!(
            (config.talker_pool(), config.listener_pool()),
            (POOL_SIZE, POOL_SIZE)
        );
        let written = toml::to_string(&config).unwrap();
        assert!(!written.contains("destinations"), "{written}");
        assert!(!written.contains("sources"), "{written}");
        // A file that says nothing of them reads the same.
        let read: Config = toml::from_str("avb_lite = \"auto\"\n").unwrap();
        assert_eq!(read, config);
    }

    /// Hiding takes a stream out of "all" mode and unpins it; pinning
    /// shows it in either mode and takes it off the hidden list; a
    /// person's pin makes a remembered one theirs.
    #[test]
    fn pins_and_hides_keep_each_other_straight() {
        let mut config = Config::default();
        let input = RemoteStream::destination(SPEAKER, 1);
        assert!(config.hide(input, "Sub"));
        assert!(!config.shows(&input));
        assert!(!config.hide(input, "Sub"), "hidden already");
        assert!(config.pin(input, "Sub", true));
        assert!(config.shows(&input) && !config.is_hidden(&input));
        assert!(config.pin[0].remembered);
        assert!(config.pin(input, "Sub", false), "a person's pin");
        assert!(!config.pin[0].remembered);
        assert!(!config.pin(input, "Sub", false), "pinned already");
        config.set_shown(Way::Destination, Shown::Pinned);
        assert!(config.shows(&input));
        assert!(config.unpin(&input));
        assert!(!config.shows(&input));
        config.set_shown(Way::Destination, Shown::All);
        assert!(config.shows(&input));
        assert!(config.hide(input, "Sub"));
        assert!(config.pin(input, "", false));
        assert!(config.hide.is_empty() && config.pin.len() == 1);
        assert!(config.hide(input, "Sub"));
        assert!(config.pin.is_empty(), "hiding unpins");
        assert!(config.show(&input));
        assert!(config.shows(&input));
    }

    /// Pins and hidden streams read back as written, as tables naming
    /// their entity in hex, an input or an output, and a name; one naming
    /// both an input and an output names no stream and shows nothing.
    #[test]
    fn device_entries_read_back_as_written() {
        let mut config = Config::default();
        config.set_shown(Way::Source, Shown::All);
        config.pin(RemoteStream::destination(SPEAKER, 0), "Living room", true);
        config.hide(RemoteStream::source(SPEAKER, 2), "Mic 3");
        config.endpoints = vec![EndpointConfig::default_entity("desk", DEFAULT_INSTANCE)];
        let written = toml::to_string(&config).expect("writes");
        assert!(written.contains("sources = \"all\""), "{written}");
        assert!(written.contains("[[pin]]"), "{written}");
        assert!(
            written.contains("entity = \"0x001b92fffe0123ab\""),
            "{written}"
        );
        assert!(written.contains("remembered = true"), "{written}");
        let read: Config = toml::from_str(&written).expect("reads");
        assert_eq!(read, config);
        let both: Config =
            toml::from_str("[[pin]]\nentity = \"0x1\"\ninput = 0\noutput = 0\n").unwrap();
        assert_eq!(both.pin[0].stream(), None);
        assert!(!both.shows(&RemoteStream::source(EntityId(1), 0)));
    }

    /// The daemon starts again for the port's settings, not for pins or
    /// modes, which it takes in place.
    #[test]
    fn only_the_ports_settings_start_the_daemon_again() {
        let before = Config::default();
        let mut pinned = before.clone();
        pinned.pin(RemoteStream::destination(SPEAKER, 0), "Living room", false);
        pinned.set_shown(Way::Destination, Shown::Pinned);
        assert_eq!(pinned.port_settings(), before.port_settings());
        let lite = Config {
            avb_lite: LiteChoice::Off,
            ..before.clone()
        };
        assert_ne!(lite.port_settings(), before.port_settings());
    }

    /// An app's pin goes through the edit lock and keeps the default
    /// entity, as any edit does.
    #[test]
    fn an_edit_pins_and_keeps_the_default() {
        let dir = scratch("pin");
        let path = dir.join("d0.toml");
        let mut config = Config::default();
        config.ensure_default(|| "desk".into());
        store::save(&path, &config).unwrap();
        let stream = RemoteStream::destination(SPEAKER, 0);
        let pinned = edit_at(&path, |config| {
            config.endpoints.clear();
            config.pin(stream, "Living room", false)
        })
        .unwrap();
        assert!(pinned);
        let read = load(&path).unwrap();
        assert!(read.is_pinned(&stream));
        assert!(read.default_entity().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
