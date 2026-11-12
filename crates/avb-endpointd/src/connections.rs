//! When the daemon connects a network device's remote stream to one of the
//! default entity's pool streams, and when it lets it go, without I/O: the
//! caller says each turn which devices apps use and how each remote stream
//! stands, and carries out the connects and disconnects it gets back with
//! its controller, saying how each connect went.
//!
//! - An app's first stream linking to a device connects it, where a pool
//!   stream is free and the remote stream is idle: a busy one, bound or
//!   acquired by another controller, is left be, with no retrying.
//! - 30 s after the last app stream goes it disconnects, so pauses, seeks
//!   and track changes cost no ACMP and MSRP round each.
//! - It disconnects only what it connected and no one changed since: a
//!   remote stream that went busy it forgets, and so a destination off
//!   the network, where nothing would hear a disconnect. One no longer
//!   shown, hidden or unpinned while connected, it disconnects at once,
//!   or its listener would stay bound to a pool talker that may go to
//!   another device next. A source off the network it disconnects too:
//!   the binding is the daemon's own pool listener's.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::devices::{DeviceState, RemoteStream, Way};

/// How long after the last app stream goes a connection stays.
pub const RELEASE: Duration = Duration::from_secs(30);

/// What to do with the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Connect `stream` to pool stream `pool`: for a destination, the
    /// default entity's pool talker `pool` to the remote listener input;
    /// for a source, the remote talker output to pool listener `pool`.
    Connect { stream: RemoteStream, pool: u16 },
    /// Undo the connection it made.
    Disconnect { stream: RemoteStream, pool: u16 },
}

/// A device as the caller sees it this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub stream: RemoteStream,
    /// An app plays into it or records from it.
    pub in_use: bool,
    /// How its remote stream stands as the controller read it: idle, busy
    /// or another state; a stream the daemon connected reads idle.
    pub remote: DeviceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Asked for; the controller's answer is to come.
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    pool: u16,
    phase: Phase,
    /// When the last app stream went, while none is linked.
    unused_since: Option<Duration>,
}

/// The connections the daemon made, each on its pool stream.
#[derive(Debug, Clone)]
pub struct Connections {
    talkers: u16,
    listeners: u16,
    links: BTreeMap<RemoteStream, Link>,
    /// Devices in use that found no pool stream free, this turn.
    starved: Vec<RemoteStream>,
    /// Devices whose connect failed, not tried again until no app uses
    /// them.
    refused: BTreeSet<RemoteStream>,
    /// Pool listeners another controller bound, which no source takes
    /// until they are unbound.
    held: BTreeSet<u16>,
}

impl Connections {
    /// With `talkers` pool talkers for destinations and `listeners` pool
    /// listeners for sources.
    pub fn new(talkers: u16, listeners: u16) -> Self {
        Connections {
            talkers,
            listeners,
            links: BTreeMap::new(),
            starved: Vec::new(),
            refused: BTreeSet::new(),
            held: BTreeSet::new(),
        }
    }

    /// Which pool listeners another controller holds bound, as the caller
    /// read them this turn: a pool listener is the daemon's own input, so
    /// any controller may bind it, and the daemon never takes it back.
    pub fn set_held_listeners(&mut self, pools: impl IntoIterator<Item = u16>) {
        self.held = pools.into_iter().collect();
    }

    /// The connects and disconnects this turn calls for, `now` on the
    /// caller's clock.
    pub fn update(&mut self, now: Duration, seen: &[Seen]) -> Vec<Action> {
        let mut actions = Vec::new();
        self.starved.clear();
        // Forgotten: those another controller took, which are not ours to
        // disconnect, and destinations off the network. Undone: those no
        // longer shown, and sources off the network.
        self.links.retain(|stream, link| {
            let remote = seen
                .iter()
                .find(|device| device.stream == *stream)
                .map(|device| device.remote);
            match remote {
                Some(DeviceState::Idle) => true,
                Some(DeviceState::Busy) => false,
                Some(DeviceState::Offline) if stream.way == Way::Destination => false,
                _ => {
                    actions.push(Action::Disconnect {
                        stream: *stream,
                        pool: link.pool,
                    });
                    false
                }
            }
        });
        // A device no app uses may be tried again once one does.
        self.refused.retain(|stream| {
            seen.iter()
                .any(|device| device.stream == *stream && device.in_use)
        });
        for device in seen {
            match self.links.get_mut(&device.stream) {
                Some(link) => {
                    if device.in_use {
                        link.unused_since = None;
                    } else {
                        let since = *link.unused_since.get_or_insert(now);
                        if link.phase == Phase::Connected && now.saturating_sub(since) >= RELEASE {
                            actions.push(Action::Disconnect {
                                stream: device.stream,
                                pool: link.pool,
                            });
                            self.links.remove(&device.stream);
                        }
                    }
                }
                None if device.in_use
                    && device.remote == DeviceState::Idle
                    && !self.refused.contains(&device.stream) =>
                {
                    match self.free_pool(device.stream.way) {
                        Some(pool) => {
                            self.links.insert(
                                device.stream,
                                Link {
                                    pool,
                                    phase: Phase::Connecting,
                                    unused_since: None,
                                },
                            );
                            actions.push(Action::Connect {
                                stream: device.stream,
                                pool,
                            });
                        }
                        None => self.starved.push(device.stream),
                    }
                }
                None => {}
            }
        }
        actions
    }

    /// How the connect the controller made for `stream` went: done, or
    /// refused or unanswered, which frees its pool stream and leaves the
    /// device unconnected until an app uses it again.
    pub fn connected(&mut self, stream: &RemoteStream, done: bool) {
        if done {
            if let Some(link) = self.links.get_mut(stream) {
                link.phase = Phase::Connected;
            }
        } else {
            self.links.remove(stream);
            self.refused.insert(*stream);
        }
    }

    /// How a device stands as far as connecting goes: connected, or in use
    /// with no pool stream free; `None` otherwise, the remote stream's own
    /// state standing.
    pub fn state(&self, stream: &RemoteStream) -> Option<DeviceState> {
        if self
            .links
            .get(stream)
            .is_some_and(|link| link.phase == Phase::Connected)
        {
            Some(DeviceState::Connected)
        } else if self.starved.contains(stream) {
            Some(DeviceState::NoTalker)
        } else {
            None
        }
    }

    /// The pool stream `stream` is connected to, or being connected to.
    pub fn pool(&self, stream: &RemoteStream) -> Option<u16> {
        self.links.get(stream).map(|link| link.pool)
    }

    /// Each device connected or being connected, with its pool stream.
    pub fn links(&self) -> Vec<(RemoteStream, u16)> {
        self.links
            .iter()
            .map(|(stream, link)| (*stream, link.pool))
            .collect()
    }

    fn free_pool(&self, way: Way) -> Option<u16> {
        let size = match way {
            Way::Destination => self.talkers,
            Way::Source => self.listeners,
        };
        (0..size).find(|pool| {
            !(way == Way::Source && self.held.contains(pool))
                && !self
                    .links
                    .iter()
                    .any(|(stream, link)| stream.way == way && link.pool == *pool)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atdecc::EntityId;

    fn speaker(input: u16) -> RemoteStream {
        RemoteStream::destination(EntityId(0x001b_92ff_fe01_23ab), input)
    }

    fn seen(stream: RemoteStream, in_use: bool, remote: DeviceState) -> Seen {
        Seen {
            stream,
            in_use,
            remote,
        }
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// An app's stream connects a device, and 30 s after it goes the
    /// connection is undone; a pause shorter than that keeps it.
    #[test]
    fn a_device_connects_in_use_and_lets_go_30_s_after() {
        let mut connections = Connections::new(4, 4);
        let main = speaker(0);
        assert_eq!(
            connections.update(secs(0), &[seen(main, true, DeviceState::Idle)]),
            [Action::Connect {
                stream: main,
                pool: 0
            }]
        );
        assert_eq!(connections.state(&main), None, "not connected yet");
        connections.connected(&main, true);
        assert_eq!(connections.state(&main), Some(DeviceState::Connected));
        // A 20 s pause, then playing again: no disconnect.
        for at in 1..=20 {
            assert!(
                connections
                    .update(secs(at), &[seen(main, false, DeviceState::Idle)])
                    .is_empty()
            );
        }
        assert!(
            connections
                .update(secs(21), &[seen(main, true, DeviceState::Idle)])
                .is_empty()
        );
        // Stopped for good: undone 30 s after.
        assert!(
            connections
                .update(secs(30), &[seen(main, false, DeviceState::Idle)])
                .is_empty()
        );
        assert!(
            connections
                .update(secs(59), &[seen(main, false, DeviceState::Idle)])
                .is_empty()
        );
        assert_eq!(
            connections.update(secs(60), &[seen(main, false, DeviceState::Idle)]),
            [Action::Disconnect {
                stream: main,
                pool: 0
            }]
        );
        assert_eq!(connections.state(&main), None);
    }

    /// Each device in use takes a pool stream of its own; one past the
    /// pool says so, and connects once one is free.
    #[test]
    fn the_pool_runs_out_and_comes_back() {
        let mut connections = Connections::new(2, 4);
        let devices: Vec<RemoteStream> = (0..3).map(speaker).collect();
        let all = |in_use: [bool; 3]| -> Vec<Seen> {
            devices
                .iter()
                .zip(in_use)
                .map(|(stream, in_use)| seen(*stream, in_use, DeviceState::Idle))
                .collect()
        };
        let actions = connections.update(secs(0), &all([true, true, true]));
        assert_eq!(actions.len(), 2);
        for device in &devices[..2] {
            connections.connected(device, true);
        }
        assert_eq!(connections.state(&devices[2]), Some(DeviceState::NoTalker));
        // The first stops; 30 s on its pool stream is free for the third.
        connections.update(secs(1), &all([false, true, true]));
        let actions = connections.update(secs(31), &all([false, true, true]));
        assert!(actions.contains(&Action::Disconnect {
            stream: devices[0],
            pool: 0
        }));
        assert!(actions.contains(&Action::Connect {
            stream: devices[2],
            pool: 0
        }));
    }

    /// A busy remote stream is left be; one another controller takes from
    /// the daemon, or that leaves the network, is forgotten without a
    /// disconnect; a refused connect frees its pool stream.
    #[test]
    fn what_is_not_the_daemons_it_leaves_alone() {
        let mut connections = Connections::new(4, 4);
        let main = speaker(0);
        let zone = speaker(1);
        assert!(
            connections
                .update(secs(0), &[seen(zone, true, DeviceState::Busy)])
                .is_empty()
        );
        connections.update(secs(0), &[seen(main, true, DeviceState::Idle)]);
        connections.connected(&main, true);
        // Another controller binds the listener elsewhere.
        assert!(
            connections
                .update(secs(5), &[seen(main, true, DeviceState::Busy)])
                .is_empty()
        );
        assert_eq!(connections.pool(&main), None);
        // Free again and still in use: the daemon connects it once more.
        let actions = connections.update(secs(6), &[seen(main, true, DeviceState::Idle)]);
        assert_eq!(actions.len(), 1);
        connections.connected(&main, false);
        assert_eq!(
            connections.pool(&main),
            None,
            "refused, its pool stream freed"
        );
        // Refused, it is not tried again while the app goes on, but is
        // once the app stopped and plays again.
        assert!(
            connections
                .update(secs(7), &[seen(main, true, DeviceState::Idle)])
                .is_empty()
        );
        connections.update(secs(8), &[seen(main, false, DeviceState::Idle)]);
        let actions = connections.update(secs(9), &[seen(main, true, DeviceState::Idle)]);
        assert_eq!(actions.len(), 1);
        connections.connected(&main, true);
        // Gone from the network: forgotten.
        assert!(
            connections
                .update(secs(10), &[seen(main, true, DeviceState::Offline)])
                .is_empty()
        );
        assert_eq!(connections.pool(&main), None);
    }

    /// A device no longer shown is disconnected at once, app or not, and
    /// so a source off the network, its pool listener being the daemon's.
    #[test]
    fn what_stops_showing_is_disconnected() {
        let mut connections = Connections::new(4, 4);
        let main = speaker(0);
        let mic = RemoteStream::source(EntityId(0x001b_92ff_fe01_4567), 0);
        assert_eq!(
            connections.update(
                secs(0),
                &[
                    seen(main, true, DeviceState::Idle),
                    seen(mic, true, DeviceState::Idle)
                ]
            ),
            [
                Action::Connect {
                    stream: main,
                    pool: 0
                },
                Action::Connect {
                    stream: mic,
                    pool: 0
                }
            ],
            "each way has a pool of its own"
        );
        connections.connected(&main, true);
        connections.connected(&mic, true);
        // Hidden while playing: its listener lets go of the pool talker.
        assert_eq!(
            connections.update(secs(1), &[seen(mic, true, DeviceState::Idle)]),
            [Action::Disconnect {
                stream: main,
                pool: 0
            }]
        );
        assert_eq!(connections.pool(&main), None);
        // The talker leaves the network: the pool listener is unbound.
        assert_eq!(
            connections.update(secs(2), &[seen(mic, true, DeviceState::Offline)]),
            [Action::Disconnect {
                stream: mic,
                pool: 0
            }]
        );
        assert_eq!(connections.pool(&mic), None);
    }

    /// A pool listener another controller holds is passed over; once it is
    /// let go, it is free again.
    #[test]
    fn a_held_pool_listener_is_passed_over() {
        let mut connections = Connections::new(4, 2);
        let mic = RemoteStream::source(EntityId(0x001b_92ff_fe01_4567), 0);
        let room = RemoteStream::source(EntityId(0x001b_92ff_fe01_4567), 1);
        connections.set_held_listeners([0]);
        assert_eq!(
            connections.update(secs(0), &[seen(mic, true, DeviceState::Idle)]),
            [Action::Connect {
                stream: mic,
                pool: 1
            }]
        );
        connections.connected(&mic, true);
        // Both pool listeners taken: the next source says so.
        connections.update(
            secs(1),
            &[
                seen(mic, true, DeviceState::Idle),
                seen(room, true, DeviceState::Idle),
            ],
        );
        assert_eq!(connections.state(&room), Some(DeviceState::NoTalker));
        connections.set_held_listeners([]);
        assert_eq!(
            connections.update(
                secs(2),
                &[
                    seen(mic, true, DeviceState::Idle),
                    seen(room, true, DeviceState::Idle),
                ],
            ),
            [Action::Connect {
                stream: room,
                pool: 0
            }]
        );
    }
}
