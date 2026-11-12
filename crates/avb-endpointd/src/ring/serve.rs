//! The daemon's side of the app rings: each ring's memory, which a
//! stream's talker or listener thread reads or writes as a block source
//! or sink, and the socket apps attach rings through.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;

use avb_stream::audio::{Arrived, Block, BlockSink, BlockSource};

use super::{
    ANSWER, ATTACHED, BAD_REQUEST, Flags, MAGIC, MESSAGE_LEN, Map, NO_RING, NO_TIME, Shape,
    StreamRef, TAKEN, VERSION, View, address, read_attach, receive, send, seqpacket, socket_path,
};

/// Inbound, a block presented further ahead than this counts as early.
const EARLY: i64 = 50_000_000;
/// How long the socket's thread waits at once, so it sees a stop.
const POLL_MILLIS: i32 = 200;

/// A stream's ring, which the daemon made and keeps while the stream is
/// configured. Dropping the last of it tells an attached app it went.
pub struct Ring {
    view: View,
    memory: OwnedFd,
    wake: OwnedFd,
    out: bool,
    /// The talker thread's own, between filling a block and sending it:
    /// its flags, its dry frames and the frames queued past it, packed.
    filled: AtomicU64,
    /// The mr bit of the last block, 2 before the first.
    media_reset: AtomicU8,
    /// A start no record has told yet.
    restarted: AtomicBool,
    /// Outbound, whether the queue ran dry and has not had audio since.
    dry: AtomicBool,
}

impl Ring {
    /// A ring for a stream of `channels`, outbound where `out`, holding
    /// `frames` frames, a power of two within the bounds.
    pub fn new(out: bool, channels: u16, frames: u32) -> io::Result<Arc<Ring>> {
        let shape = Shape::new(channels, frames);
        let memory = memfd()?;
        // SAFETY: plain calls on a descriptor this owns; results checked.
        unsafe {
            if libc::ftruncate(memory.as_raw_fd(), shape.len() as libc::off_t) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Its size fixed, so no app can shrink it under the daemon.
            let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
            if libc::fcntl(memory.as_raw_fd(), libc::F_ADD_SEALS, seals) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let view = View {
            map: Map::of(memory.as_fd(), shape.len())?,
            shape,
        };
        let header = view.header();
        let set = |value: &std::sync::atomic::AtomicU32, to: u64| {
            value.store(to as u32, Ordering::Relaxed);
        };
        set(&header.version, u64::from(VERSION));
        set(&header.direction, u64::from(out));
        set(&header.channels, shape.channels as u64);
        set(&header.frames, shape.frames);
        set(&header.records, shape.records);
        header.daemon.presented.store(NO_TIME, Ordering::Relaxed);
        header
            .daemon
            .least_margin
            .store(i64::MAX, Ordering::Relaxed);
        header
            .daemon
            .least_queued
            .store(i64::MAX, Ordering::Relaxed);
        header.magic.store(MAGIC, Ordering::Release);
        Ok(Arc::new(Ring {
            view,
            memory,
            wake: eventfd()?,
            out,
            filled: AtomicU64::new(0),
            media_reset: AtomicU8::new(2),
            restarted: AtomicBool::new(false),
            dry: AtomicBool::new(false),
        }))
    }

    /// Whether it is an outbound ring, an output's.
    pub fn is_out(&self) -> bool {
        self.out
    }

    /// Its channels.
    pub fn channels(&self) -> usize {
        self.view.shape.channels
    }

    /// The frames it holds.
    pub fn frames(&self) -> u64 {
        self.view.shape.frames
    }

    /// Its stream starts, at `rate`: a new timeline.
    pub fn start(&self, rate: u32) {
        let daemon = &self.view.header().daemon;
        daemon.rate.store(rate, Ordering::Relaxed);
        daemon.generation.fetch_add(1, Ordering::Relaxed);
        self.restarted.store(true, Ordering::Relaxed);
        daemon.running.store(1, Ordering::Release);
    }

    /// Whether its stream runs, as the daemon last said.
    pub fn set_running(&self, running: bool) {
        self.view
            .header()
            .daemon
            .running
            .store(u32::from(running), Ordering::Release);
    }

    /// The memory and the eventfd an app maps and waits on.
    fn fds(&self) -> [BorrowedFd<'_>; 2] {
        [self.memory.as_fd(), self.wake.as_fd()]
    }

    fn wake_now(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 octets to the ring's own eventfd, which never
        // blocks.
        unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), 8) };
    }

    /// Wakes the app where the head moved past a multiple of what it asked.
    fn wake_past(&self, before: u64, after: u64) {
        let every = u64::from(self.view.header().app.wake_frames.load(Ordering::Relaxed));
        if every > 0 && before / every != after / every {
            self.wake_now();
        }
    }

    /// Adds what a block says of the stream to `flags`: a start, and an
    /// mr bit that changed.
    fn mark(&self, flags: &mut Flags, media_reset: Option<bool>) {
        if self.restarted.swap(false, Ordering::Relaxed) {
            *flags |= Flags::RESTART;
        }
        if let Some(bit) = media_reset {
            let last = self.media_reset.swap(u8::from(bit), Ordering::Relaxed);
            if last != 2 && last != u8::from(bit) {
                *flags |= Flags::MEDIA_RESET;
            }
        }
    }

    /// Writes the next record, of a block from the head.
    fn record(&self, presented: Option<i64>, frames: u64, flags: Flags, missed: u8) {
        let daemon = &self.view.header().daemon;
        let at = daemon.records.load(Ordering::Relaxed);
        let head = daemon.head.load(Ordering::Relaxed);
        self.view
            .record(at)
            .store(head, presented, frames, flags, missed);
        daemon.records.store(at + 1, Ordering::Release);
    }

    /// Outbound, moves the cursor past `frames`, the next presented at
    /// `presented`.
    fn advance(&self, frames: u64, presented: Option<i64>) {
        let daemon = &self.view.header().daemon;
        let head = daemon.head.load(Ordering::Relaxed);
        let sequence = daemon.sequence.load(Ordering::Relaxed);
        daemon
            .sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        daemon.head.store(head + frames, Ordering::Relaxed);
        daemon
            .presented
            .store(presented.unwrap_or(NO_TIME), Ordering::Relaxed);
        daemon
            .sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
        self.wake_past(head, head + frames);
    }

    /// When the frame `frames` after one presented at `presented` is.
    fn later(&self, presented: Option<i64>, frames: u64) -> Option<i64> {
        let rate = i64::from(self.view.header().daemon.rate.load(Ordering::Relaxed));
        let presented = presented?;
        (rate > 0).then(|| presented + frames as i64 * 1_000_000_000 / rate)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.view
            .header()
            .daemon
            .retired
            .store(1, Ordering::Release);
        self.wake_now();
    }
}

impl BlockSource for Ring {
    fn fill(&self, block: &Block, words: &mut [i32]) {
        let view = &self.view;
        let header = view.header();
        let cursor = header.daemon.head.load(Ordering::Relaxed);
        let end = header.app.end.load(Ordering::Acquire);
        let begin = header.app.begin.load(Ordering::Relaxed);
        let ended = header.app.ended.load(Ordering::Relaxed);
        // Frames past a ring's length ahead would be ones written over.
        let limit = cursor + view.shape.frames;
        let channels = block.channels.max(1);
        let mut flags = Flags::default();
        let mut dry = 0u64;
        for (at, frame) in (cursor..).zip(words.chunks_exact_mut(channels)) {
            if at >= begin && at < end && at < limit {
                let slot = view.slot(at);
                for (place, word) in frame.iter_mut().enumerate() {
                    *word = slot
                        .get(place)
                        .map_or(0, |value| value.load(Ordering::Relaxed));
                }
                flags |= Flags::AUDIO;
            } else {
                frame.fill(0);
                if at < begin || ended >= end {
                    flags |= Flags::IDLE;
                } else {
                    flags |= Flags::DRY;
                    dry += 1;
                }
            }
        }
        let queued = end.saturating_sub(cursor + block.samples as u64);
        self.filled.store(
            u64::from(flags.0) | dry.min(0xffff) << 16 | queued.min(u64::from(u32::MAX)) << 32,
            Ordering::Relaxed,
        );
    }

    fn sent(&self, block: &Block, sent: bool) {
        let daemon = &self.view.header().daemon;
        let filled = self.filled.swap(0, Ordering::Relaxed);
        let mut flags = Flags(filled as u16);
        let dry = (filled >> 16) & 0xffff;
        let queued = filled >> 32;
        if !sent {
            flags |= Flags::NOT_SENT;
            daemon.not_sent.fetch_add(1, Ordering::Relaxed);
        }
        self.mark(&mut flags, Some(block.media_reset));
        if dry > 0 {
            daemon.dry_frames.fetch_add(dry, Ordering::Relaxed);
            if !self.dry.swap(true, Ordering::Relaxed) {
                daemon.dry_runs.fetch_add(1, Ordering::Relaxed);
            }
        } else if flags.contains(Flags::AUDIO) {
            self.dry.store(false, Ordering::Relaxed);
        }
        if flags.contains(Flags::AUDIO) {
            daemon
                .least_queued
                .fetch_min(queued as i64, Ordering::Relaxed);
        }
        let frames = block.samples as u64;
        self.record(Some(block.presented), frames, flags, 0);
        self.advance(frames, self.later(Some(block.presented), frames));
    }

    fn left_out(&self, samples: u64) {
        let daemon = &self.view.header().daemon;
        self.filled.store(0, Ordering::Relaxed);
        let mut flags = Flags::LATE;
        self.mark(&mut flags, None);
        let presented = daemon.presented.load(Ordering::Relaxed);
        let presented = (presented != NO_TIME).then_some(presented);
        daemon.late.fetch_add(samples, Ordering::Relaxed);
        self.record(presented, samples, flags, 0);
        self.advance(samples, self.later(presented, samples));
    }
}

impl BlockSink for Ring {
    fn take(&self, arrived: &Arrived, words: &[i32]) {
        let view = &self.view;
        let daemon = &view.header().daemon;
        let head = daemon.head.load(Ordering::Relaxed);
        let channels = arrived.channels.max(1);
        let frames = (words.len() / channels).min(view.shape.frames as usize) as u64;
        for (at, frame) in (head..head + frames).zip(words.chunks_exact(channels)) {
            for (place, value) in view.slot(at).iter().enumerate() {
                value.store(frame.get(place).copied().unwrap_or(0), Ordering::Relaxed);
            }
        }
        let mut flags = Flags::default();
        match arrived.margin {
            None => flags |= Flags::NO_TIME,
            Some(margin) => {
                if margin < 0 {
                    flags |= Flags::LATE;
                    daemon.late.fetch_add(1, Ordering::Relaxed);
                } else if margin > EARLY {
                    flags |= Flags::EARLY;
                }
                daemon.least_margin.fetch_min(margin, Ordering::Relaxed);
            }
        }
        if arrived.missed > 0 {
            flags |= Flags::GAP;
            daemon
                .missed
                .fetch_add(u64::from(arrived.missed) * frames, Ordering::Relaxed);
        }
        self.mark(&mut flags, Some(arrived.media_reset));
        let at = daemon.records.load(Ordering::Relaxed);
        view.record(at)
            .store(head, arrived.presented, frames, flags, arrived.missed);
        daemon.head.store(head + frames, Ordering::Release);
        daemon.records.store(at + 1, Ordering::Release);
        self.wake_past(head, head + frames);
    }
}

fn memfd() -> io::Result<OwnedFd> {
    // SAFETY: a plain call with a nul-terminated name; the result checked.
    let fd = unsafe {
        libc::memfd_create(
            c"avb-ring".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor no one else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: a plain call; the result checked.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor no one else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The rings apps may attach, by direction (true outbound) and stream.
pub type Rings = Arc<Mutex<BTreeMap<(bool, StreamRef), Arc<Ring>>>>;

/// The socket apps attach rings through, served on a thread of its own.
pub struct Server {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl Server {
    /// Serves `rings` on the interface's socket.
    pub fn start(interface: &str, rings: Rings) -> io::Result<Server> {
        let path = socket_path(interface).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no runtime folder, XDG_RUNTIME_DIR",
            )
        })?;
        Self::start_at(path, rings)
    }

    /// Serves `rings` on a socket at `path`.
    pub fn start_at(path: PathBuf, rings: Rings) -> io::Result<Server> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        // One daemon runs an interface, so a socket left there is an old
        // one's.
        let _ = std::fs::remove_file(&path);
        let listener = seqpacket()?;
        let (address, len) = address(&path)?;
        // SAFETY: a valid address of `len` octets, and a live socket.
        unsafe {
            if libc::bind(listener.as_raw_fd(), (&raw const address).cast(), len) < 0
                || libc::listen(listener.as_raw_fd(), 8) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("app rings".into())
                .spawn(move || serve(&listener, &rings, &stop))?
        };
        Ok(Server {
            stop,
            handle: Some(handle),
            path,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// An app's connection, and the rings it attached: weakly, so a ring
/// whose stream goes is dropped, and the app told, while it is attached.
struct Client {
    socket: OwnedFd,
    attached: Vec<Weak<Ring>>,
}

fn serve(listener: &OwnedFd, rings: &Rings, stop: &AtomicBool) {
    let mut clients: Vec<Client> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let mut polls: Vec<libc::pollfd> = std::iter::once(listener.as_raw_fd())
            .chain(clients.iter().map(|client| client.socket.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: live pollfds, as many as given.
        let ready =
            unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, POLL_MILLIS) };
        if ready <= 0 {
            continue;
        }
        // The apps' first, by place, as accepting adds to the list.
        for place in (0..clients.len()).rev() {
            let events = polls[place + 1].revents;
            if events == 0 {
                continue;
            }
            let mut message = [0u8; MESSAGE_LEN];
            let gone = if events & libc::POLLIN != 0 {
                let received = receive(clients[place].socket.as_fd(), &mut message);
                match received {
                    Ok((0, _)) | Err(_) => true,
                    Ok((len, _)) => answer(&mut clients, place, &message[..len], rings).is_err(),
                }
            } else {
                events & (libc::POLLHUP | libc::POLLERR) != 0
            };
            if gone {
                clients.swap_remove(place);
            }
        }
        if polls[0].revents & libc::POLLIN != 0 {
            // SAFETY: accepting on a live socket; the result checked.
            let fd = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd >= 0 {
                clients.push(Client {
                    // SAFETY: a new descriptor no one else owns.
                    socket: unsafe { OwnedFd::from_raw_fd(fd) },
                    attached: Vec::new(),
                });
            }
        }
    }
}

/// Answers an app's attach.
fn answer(clients: &mut [Client], place: usize, message: &[u8], rings: &Rings) -> io::Result<()> {
    let reply = |status: u8| [ANSWER, status, 0, 0, 0, 0, 0, 0];
    let socket = clients[place].socket.as_fd();
    let Some(key) = read_attach(message) else {
        return send(socket, &reply(BAD_REQUEST), &[]);
    };
    let ring = rings
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    let Some(ring) = ring else {
        return send(socket, &reply(NO_RING), &[]);
    };
    let taken = clients.iter().any(|client| {
        client
            .attached
            .iter()
            .any(|held| std::ptr::eq(held.as_ptr(), Arc::as_ptr(&ring)))
    });
    if taken {
        return send(clients[place].socket.as_fd(), &reply(TAKEN), &[]);
    }
    send(clients[place].socket.as_fd(), &reply(ATTACHED), &ring.fds())?;
    let attached = &mut clients[place].attached;
    attached.retain(|held| held.strong_count() > 0);
    attached.push(Arc::downgrade(&ring));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{App, Error, Flags as RingFlags};
    use std::time::Duration;

    /// A server on a socket of its own, with the given rings.
    fn served(rings: &[((bool, StreamRef), Arc<Ring>)]) -> (Server, PathBuf) {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "avb-ring-test-{}-{}.app",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let listed: Rings = Rings::default();
        listed.lock().unwrap().extend(rings.iter().cloned());
        (Server::start_at(path.clone(), listed).unwrap(), path)
    }

    const ROOM: StreamRef = StreamRef {
        instance: 3,
        index: 0,
    };

    fn block(presented: i64) -> Block {
        Block {
            presented,
            samples: 6,
            channels: 2,
            media_reset: false,
        }
    }

    /// A talker's turn: fills a block and sends it.
    fn send(ring: &Ring, presented: i64) -> Vec<i32> {
        let mut words = vec![-1; 12];
        ring.fill(&block(presented), &mut words);
        ring.sent(&block(presented), true);
        words
    }

    /// What an app queues goes out word for word, after the lead, idle
    /// before it and dry after; a flush and a truncate end it at once,
    /// idle; an overwrite changes what is still queued.
    #[test]
    fn an_outbound_ring_sends_what_the_app_queues() {
        let ring = Ring::new(true, 2, 4096).unwrap();
        ring.start(48_000);
        let (_server, path) = served(&[((true, ROOM), ring.clone())]);
        let app = App::connect_at(&path).unwrap();
        let mut out = app.outbound(ROOM).unwrap();
        assert_eq!(out.channels(), 2);
        assert!(matches!(app.outbound(ROOM), Err(Error::Taken)));
        assert!(matches!(app.inbound(ROOM), Err(Error::NoRing)));

        // Nothing queued: idle silence, not dry.
        assert_eq!(send(&ring, 1_000_000), vec![0; 12]);
        let sent = out.sent().unwrap().unwrap();
        assert_eq!((sent.frame, sent.frames), (0, 6));
        assert_eq!(sent.presented, Some(1_000_000));
        assert!(sent.flags.contains(RingFlags::IDLE) && !sent.flags.contains(RingFlags::DRY));
        assert!(sent.flags.contains(RingFlags::RESTART));
        let cursor = out.cursor();
        assert_eq!(cursor.frame, 6);
        assert_eq!(cursor.presented, Some(1_125_000));

        // 2 ms ahead: frame 6 + 96.
        out.set_lead(Duration::from_millis(2));
        let samples: Vec<i32> = (0..24).map(|word| word * 0x0101_0101).collect();
        let placed = out.write(&samples).unwrap();
        assert_eq!(placed.frame, 102);
        assert_eq!(placed.presented, Some(1_125_000 + 2_000_000));
        let mut presented = 1_125_000;
        for _ in 0..16 {
            assert_eq!(send(&ring, presented), vec![0; 12]);
            presented += 125_000;
        }
        assert_eq!(out.cursor().frame, 102);
        assert_eq!(send(&ring, presented), samples[..12]);
        assert_eq!(send(&ring, presented + 125_000), samples[12..]);
        assert_eq!(send(&ring, presented + 250_000), vec![0; 12]);
        let flags: Vec<RingFlags> = std::iter::from_fn(|| out.sent().unwrap())
            .map(|sent| sent.flags)
            .collect();
        assert_eq!(flags.len(), 19);
        assert!(
            flags[..16]
                .iter()
                .all(|flags| flags.contains(RingFlags::IDLE))
        );
        assert!(flags[16].contains(RingFlags::AUDIO) && flags[17].contains(RingFlags::AUDIO));
        assert!(flags[18].contains(RingFlags::DRY));
        let stats = out.take_stats();
        assert_eq!((stats.dry_runs, stats.dry_frames), (1, 6));

        // Queued, then flushed: idle at once.
        out.write(&samples).unwrap();
        let cut = out.flush();
        assert_eq!(out.queued(), 0);
        let first = send(&ring, 0);
        assert_eq!(first, vec![0; 12]);
        assert!(out.sent().unwrap().unwrap().flags.contains(RingFlags::IDLE));
        assert_eq!(cut.frame + 6, out.cursor().frame);

        // Queued, then faded: the second block written over, and what
        // follows it truncated.
        let placed = out.write(&samples).unwrap();
        out.write(&samples).unwrap();
        let halved: Vec<i32> = samples[12..].iter().map(|word| word / 2).collect();
        out.overwrite(placed.frame + 6, &halved).unwrap();
        assert!(matches!(
            out.overwrite(out.cursor().frame, &halved),
            Err(Error::TooLate)
        ));
        out.truncate(placed.frame + 12);
        let mut words = Vec::new();
        while out.cursor().frame < placed.frame + 12 {
            words = send(&ring, 0);
        }
        assert_eq!(words, halved);
        assert_eq!(send(&ring, 0), vec![0; 12]);
        let last = std::iter::from_fn(|| out.sent().unwrap()).last().unwrap();
        assert!(last.flags.contains(RingFlags::IDLE), "truncated, not dry");

        // Left out as late: the cursor moves past, flagged.
        let before = out.cursor().frame;
        ring.left_out(18);
        assert_eq!(out.cursor().frame, before + 18);
        let late = out.sent().unwrap().unwrap();
        assert!(late.flags.contains(RingFlags::LATE));
        assert_eq!(late.frames, 18);
    }

    /// What a listener takes comes to the app block by block, with its
    /// presentation time and what happened to it; one fallen a ring behind
    /// hears so and goes on.
    #[test]
    fn an_inbound_ring_gives_the_app_each_block() {
        let ring = Ring::new(false, 2, 4096).unwrap();
        ring.start(48_000);
        let (_server, path) = served(&[((false, ROOM), ring.clone())]);
        let app = App::connect_at(&path).unwrap();
        let mut input = app.inbound(ROOM).unwrap();
        input.wake_every(6);
        let mut samples = Vec::new();
        assert!(input.next(&mut samples).unwrap().is_none());

        let words: Vec<i32> = (0..12).map(|word| i32::MIN + word).collect();
        let arrived = |presented: Option<i64>, missed: u8| Arrived {
            presented,
            margin: presented.map(|presented| presented - 1_000_000),
            samples: 6,
            channels: 2,
            missed,
            media_reset: false,
        };
        ring.take(&arrived(Some(3_000_000), 0), &words);
        assert!(input.wait(Some(Duration::from_secs(1))).unwrap(), "woken");
        let got = input.next(&mut samples).unwrap().unwrap();
        assert_eq!(samples, words, "every bit");
        assert_eq!(got.presented, Some(3_000_000));
        assert_eq!((got.frame, got.frames), (0, 6));
        assert!(got.flags.contains(RingFlags::RESTART));

        // Late, and after a gap; then with no time.
        ring.take(&arrived(Some(500_000), 2), &words);
        ring.take(&arrived(None, 0), &words);
        let late = input.next(&mut samples).unwrap().unwrap();
        assert!(late.flags.contains(RingFlags::LATE) && late.flags.contains(RingFlags::GAP));
        assert_eq!(late.missed, 2);
        let untimed = input.next(&mut samples).unwrap().unwrap();
        assert!(untimed.flags.contains(RingFlags::NO_TIME));
        assert_eq!(untimed.presented, None);
        let stats = input.take_stats();
        assert_eq!(stats.late, 1);
        assert_eq!(stats.least_margin, Some(-500_000));

        // A ring and more behind: the frames passed are told.
        for _ in 0..1000 {
            ring.take(&arrived(Some(9_000_000), 0), &words);
        }
        match input.next(&mut samples) {
            Err(Error::Overrun { lost_frames }) => assert_eq!(lost_frames, 6000),
            other => panic!("{other:?}"),
        }
        assert!(input.next(&mut samples).unwrap().is_none());
        ring.take(&arrived(Some(9_000_000), 0), &words);
        assert!(input.next(&mut samples).unwrap().is_some());
    }

    /// A ring whose stream went tells its app so; the app's rings go
    /// with its connection, so another app may attach them.
    #[test]
    fn rings_go_with_their_stream_and_their_app() {
        let ring = Ring::new(false, 2, 4096).unwrap();
        let (server, path) = served(&[((false, ROOM), ring.clone())]);
        let first = App::connect_at(&path).unwrap();
        let mut input = first.inbound(ROOM).unwrap();
        drop((first, input));
        // The server sees the hangup within its poll.
        let again = App::connect_at(&path).unwrap();
        let mut attached = Err(Error::Taken);
        for _ in 0..20 {
            attached = again.inbound(ROOM);
            if attached.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        input = attached.unwrap();
        drop(server);
        drop(ring);
        let mut samples = Vec::new();
        assert!(matches!(input.next(&mut samples), Err(Error::Retired)));
    }
}
