//! App rings: a stream's samples shared with an app through memory that
//! the daemon's talker or listener thread reads or writes one frame's
//! block at a time, each block with its gPTP presentation time, with
//! nothing buffered, resampled or converted between. A stream whose
//! `audio` is `"ring"` has one in place of a virtual audio device.
//!
//! An app connects to the daemon's socket for the interface, asks for a
//! stream's ring by its entity's instance and its place among that
//! entity's inputs or outputs, and maps the memory it is given, with an
//! eventfd that wakes it as often as it asks. The stream threads take no
//! lock and make no system call for a ring but that wakeup.
//!
//! Samples are the wire's as 32-bit words in this computer's byte order:
//! a 32-bit format's exactly, every bit, a 24-bit or AM824 one in the high
//! 24 bits, a float's scaled from the full range. A ring has the channels
//! of its stream as configured; a format of fewer fills its first ones,
//! the rest silent, and one of more gives it its first.
//!
//! A ring is made when its stream is configured and kept while it is, so
//! its talker or listener starting and stopping, or changing format,
//! keeps an app attached: an inbound ring goes on filling while no app
//! reads it, and an outbound one goes out as silence while nothing is
//! queued.

// Shared memory, and descriptors passed over a socket, are system calls
// libc gives only as unsafe functions.
#![allow(unsafe_code)]

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering, fence};
use std::time::Duration;

#[cfg(feature = "daemon")]
pub mod serve;

/// The frames a ring holds where its stream's configuration names none,
/// about 340 ms at 48 kHz, and the fewest and most it may.
pub const DEFAULT_FRAMES: u32 = 1 << 14;
pub const FEWEST_FRAMES: u32 = 1 << 12;
pub const MOST_FRAMES: u32 = 1 << 18;

const MAGIC: u64 = u64::from_le_bytes(*b"AVBRING\0");
const VERSION: u32 = 1;
/// Octets of the header, ahead of the samples.
const HEADER_LEN: usize = 4096;
const RECORD_LEN: usize = std::mem::size_of::<Record>();
const PAGE: usize = 4096;
/// A presentation time not known.
const NO_TIME: i64 = i64::MIN;

/// The ring's header. What the daemon writes and what the app writes are
/// on cache lines of their own.
#[repr(C)]
struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    /// 0 inbound, network to app; 1 outbound.
    direction: AtomicU32,
    channels: AtomicU32,
    /// Frames of samples and records the ring holds, each a power of two.
    frames: AtomicU32,
    records: AtomicU32,
    _spare: [AtomicU32; 9],
    daemon: DaemonLine,
    app: AppLine,
}

#[repr(C, align(64))]
struct DaemonLine {
    /// Counts the stream's starts: each is a new timeline.
    generation: AtomicU32,
    /// 1 while the talker or listener runs.
    running: AtomicU32,
    /// 1 once the daemon no longer has the ring, as its stream went.
    retired: AtomicU32,
    /// The stream's sample rate, 0 before it first runs.
    rate: AtomicU32,
    /// Inbound, the frames written; outbound, the next frame to go out.
    head: AtomicU64,
    /// Records written.
    records: AtomicU64,
    /// Outbound, a sequence lock around `head` and `presented`: odd while
    /// they change.
    sequence: AtomicU32,
    _spare: AtomicU32,
    /// Outbound, when frame `head` is presented, in gPTP nanoseconds.
    presented: AtomicI64,
    /// Inbound, blocks handled after their presentation time; outbound,
    /// frames left out as too late to send.
    late: AtomicU64,
    /// Inbound, frames the sequence numbers say went missing.
    missed: AtomicU64,
    /// Outbound, frames sent as silence for want of queued audio, and
    /// the times the queue ran dry.
    dry_frames: AtomicU64,
    dry_runs: AtomicU64,
    /// Outbound, blocks built that no destination took.
    not_sent: AtomicU64,
    /// The least margin before presentation inbound, and the fewest
    /// frames queued at a send outbound, since the app last took them.
    least_margin: AtomicI64,
    least_queued: AtomicI64,
}

#[repr(C, align(64))]
struct AppLine {
    /// The daemon writes the eventfd as its head passes a multiple of
    /// this; never at 0.
    wake_frames: AtomicU32,
    _spare: AtomicU32,
    /// Outbound, frames from `begin` to `end` are queued audio, and the
    /// queue ends there on purpose, not dry, while `ended` is `end`.
    begin: AtomicU64,
    end: AtomicU64,
    ended: AtomicU64,
}

/// One block's record: its first frame, its presentation time, and its
/// frames, flags and missed frames, packed.
#[repr(C)]
struct Record {
    frame: AtomicU64,
    presented: AtomicI64,
    info: AtomicU64,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_LEN);

impl Record {
    #[cfg_attr(not(feature = "daemon"), allow(dead_code))]
    fn store(&self, frame: u64, presented: Option<i64>, frames: u64, flags: Flags, missed: u8) {
        self.frame.store(frame, Ordering::Relaxed);
        self.presented
            .store(presented.unwrap_or(NO_TIME), Ordering::Relaxed);
        let info =
            frames.min(u64::from(u32::MAX)) | u64::from(flags.0) << 32 | u64::from(missed) << 48;
        self.info.store(info, Ordering::Relaxed);
    }

    fn load(&self) -> (u64, Option<i64>, usize, Flags, u8) {
        let frame = self.frame.load(Ordering::Relaxed);
        let presented = self.presented.load(Ordering::Relaxed);
        let info = self.info.load(Ordering::Relaxed);
        (
            frame,
            (presented != NO_TIME).then_some(presented),
            (info & 0xffff_ffff) as usize,
            Flags((info >> 32) as u16),
            (info >> 48) as u8,
        )
    }
}

/// What happened to a block, as its record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// The stream started again before it: a new timeline, its
    /// presentation times not following on from those before.
    pub const RESTART: Flags = Flags(1);
    /// Its mr bit differs from the block before's.
    pub const MEDIA_RESET: Flags = Flags(1 << 1);
    /// Inbound, handled after its presentation time; outbound, left out
    /// as too late to send, never sent.
    pub const LATE: Flags = Flags(1 << 2);
    /// Inbound, presented more than 50 ms after it was handled.
    pub const EARLY: Flags = Flags(1 << 3);
    /// Inbound, frames went missing just before it.
    pub const GAP: Flags = Flags(1 << 4);
    /// Inbound, no presentation time: gPTP time is not a grandmaster's.
    pub const NO_TIME: Flags = Flags(1 << 5);
    /// Outbound, frames of it were queued audio.
    pub const AUDIO: Flags = Flags(1 << 8);
    /// Outbound, frames of it went as silence for want of queued audio.
    pub const DRY: Flags = Flags(1 << 9);
    /// Outbound, frames of it went as silence on purpose: before what was
    /// queued after a dry run, or after an end the app set.
    pub const IDLE: Flags = Flags(1 << 10);
    /// Outbound, built but taken by no destination, as while no listener
    /// is connected.
    pub const NOT_SENT: Flags = Flags(1 << 11);

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, other: Flags) {
        self.0 |= other.0;
    }
}

/// The places a ring's parts take in its memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    channels: usize,
    frames: u64,
    records: u64,
}

impl Shape {
    #[cfg_attr(not(feature = "daemon"), allow(dead_code))]
    fn new(channels: u16, frames: u32) -> Self {
        let frames = frames.clamp(FEWEST_FRAMES, MOST_FRAMES).next_power_of_two();
        Shape {
            channels: usize::from(channels.max(1)),
            frames: u64::from(frames),
            // A block has 6 frames at the fewest.
            records: u64::from(frames / 4).next_power_of_two(),
        }
    }

    fn samples_len(&self) -> usize {
        self.frames as usize * self.channels
    }

    fn records_at(&self) -> usize {
        HEADER_LEN + (self.samples_len() * 4).next_multiple_of(64)
    }

    fn len(&self) -> usize {
        (self.records_at() + self.records as usize * RECORD_LEN).next_multiple_of(PAGE)
    }
}

/// A ring's memory, mapped.
struct Map {
    base: *mut u8,
    len: usize,
}

// The mapping's contents are atomics only, read and written as such.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    fn of(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        // SAFETY: a new shared mapping of the whole of a file at least `len`
        // long, checked by the callers.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map {
            base: base.cast(),
            len,
        })
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `of`, which nothing refers to past
        // the view that owns it.
        unsafe {
            libc::munmap(self.base.cast(), self.len);
        }
    }
}

/// A ring's memory as its parts.
struct View {
    map: Map,
    shape: Shape,
}

impl View {
    fn header(&self) -> &Header {
        // SAFETY: the mapping is page aligned and at least HEADER_LEN long,
        // and a Header is atomics alone.
        unsafe { &*self.map.base.cast::<Header>() }
    }

    fn samples(&self) -> &[AtomicI32] {
        // SAFETY: within the mapping, as `Shape::len` lays it out, and
        // aligned; atomics alone.
        unsafe {
            std::slice::from_raw_parts(
                self.map.base.add(HEADER_LEN).cast::<AtomicI32>(),
                self.shape.samples_len(),
            )
        }
    }

    fn records(&self) -> &[Record] {
        // SAFETY: as for the samples.
        unsafe {
            std::slice::from_raw_parts(
                self.map.base.add(self.shape.records_at()).cast::<Record>(),
                self.shape.records as usize,
            )
        }
    }

    fn record(&self, at: u64) -> &Record {
        &self.records()[(at % self.shape.records) as usize]
    }

    /// Frame `frame`'s samples.
    fn slot(&self, frame: u64) -> &[AtomicI32] {
        let at = (frame % self.shape.frames) as usize * self.shape.channels;
        &self.samples()[at..at + self.shape.channels]
    }

    /// Outbound, the next frame to go out and when it is presented.
    fn cursor(&self) -> Cursor {
        let daemon = &self.header().daemon;
        loop {
            let before = daemon.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let frame = daemon.head.load(Ordering::Relaxed);
            let presented = daemon.presented.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if daemon.sequence.load(Ordering::Relaxed) == before {
                return Cursor {
                    frame,
                    presented: (presented != NO_TIME).then_some(presented),
                };
            }
        }
    }
}

/// Where the daemon serves an interface's rings.
pub fn socket_path(interface: &str) -> Option<PathBuf> {
    crate::store::runtime_dir().map(|dir| dir.join(format!("{interface}.app")))
}

/// A stream of one of the daemon's entities: the entity's instance, and
/// the stream's place among its inputs or outputs as the configuration
/// lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamRef {
    pub instance: u8,
    pub index: u16,
}

/// Why a ring could not be had, or read, or written.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// No such stream has a ring.
    NoRing,
    /// Another attachment has it.
    Taken,
    /// The daemon's answer, or its ring, is not one this program knows.
    Unknown,
    /// Inbound, the ring was written past what was read: those frames are
    /// gone, and reading goes on from the oldest kept.
    Overrun {
        lost_frames: u64,
    },
    /// The daemon no longer has the ring, as its stream went or the
    /// daemon stopped: attach again.
    Retired,
    /// Outbound, the ring has room for only this many frames more.
    Full {
        room: usize,
    },
    /// Outbound, the frames to change are too near to going out, or went.
    TooLate,
    /// Outbound, the frames to change are not queued.
    NotQueued,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "{error}"),
            Error::NoRing => write!(formatter, "no such stream has a ring"),
            Error::Taken => write!(formatter, "the ring is attached elsewhere"),
            Error::Unknown => write!(formatter, "not a ring this program knows"),
            Error::Overrun { lost_frames } => write!(formatter, "{lost_frames} frames lost"),
            Error::Retired => write!(formatter, "the daemon no longer has the ring"),
            Error::Full { room } => write!(formatter, "room for {room} frames only"),
            Error::TooLate => write!(formatter, "too late"),
            Error::NotQueued => write!(formatter, "not queued"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// Messages on the socket, one a packet: an attach, `A`, its version, the
/// direction (0 in, 1 out), the instance and the index; the answer, `R`,
/// with a status, and on success the ring's memory and eventfd.
const ATTACH: u8 = b'A';
const ANSWER: u8 = b'R';
const MESSAGE_LEN: usize = 8;
const ATTACHED: u8 = 0;
const NO_RING: u8 = 1;
const TAKEN: u8 = 2;
#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
const BAD_REQUEST: u8 = 3;

fn attach_message(out: bool, stream: StreamRef) -> [u8; MESSAGE_LEN] {
    let index = stream.index.to_le_bytes();
    [
        ATTACH,
        VERSION as u8,
        u8::from(out),
        stream.instance,
        index[0],
        index[1],
        0,
        0,
    ]
}

/// An attach message's direction and stream.
#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
fn read_attach(message: &[u8]) -> Option<(bool, StreamRef)> {
    match message {
        [
            ATTACH,
            version,
            direction @ (0 | 1),
            instance,
            low,
            high,
            ..,
        ] if u32::from(*version) == VERSION => Some((
            *direction == 1,
            StreamRef {
                instance: *instance,
                index: u16::from_le_bytes([*low, *high]),
            },
        )),
        _ => None,
    }
}

/// A packet socket on the Unix socket at `path`.
fn connect(path: &Path) -> io::Result<OwnedFd> {
    let socket = seqpacket()?;
    let (address, len) = address(path)?;
    // SAFETY: a valid address of `len` octets.
    let made = unsafe { libc::connect(socket.as_raw_fd(), (&raw const address).cast(), len) };
    if made < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

fn seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: a plain socket call; the result is checked.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor no one else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: all zeros is a valid sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the socket's path is too long",
        ));
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((address, len as libc::socklen_t))
}

/// Sends one packet, with `fds` beside it.
fn send(socket: BorrowedFd<'_>, message: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr() as *mut libc::c_void,
        iov_len: message.len(),
    };
    let raw: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let fds_len = std::mem::size_of_val(raw.as_slice()) as u32;
    // SAFETY: CMSG_SPACE is a computation.
    let space = unsafe { libc::CMSG_SPACE(fds_len) } as usize;
    let mut control = vec![0u8; if raw.is_empty() { 0 } else { space }];
    // SAFETY: all zeros is a valid msghdr.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &raw mut iov;
    header.msg_iovlen = 1;
    if !raw.is_empty() {
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control.len() as _;
        // SAFETY: the control buffer has room for one header and the
        // descriptors, as CMSG_SPACE gave.
        unsafe {
            let first = libc::CMSG_FIRSTHDR(&raw const header);
            (*first).cmsg_level = libc::SOL_SOCKET;
            (*first).cmsg_type = libc::SCM_RIGHTS;
            (*first).cmsg_len = libc::CMSG_LEN(fds_len) as _;
            std::ptr::copy_nonoverlapping(
                raw.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(first),
                fds_len as usize,
            );
        }
    }
    // SAFETY: the header points at live buffers.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &raw const header, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Takes one packet into `message`, with up to two descriptors beside it:
/// its length and the descriptors.
fn receive(socket: BorrowedFd<'_>, message: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: message.as_mut_ptr().cast(),
        iov_len: message.len(),
    };
    // SAFETY: CMSG_SPACE is a computation.
    let space = unsafe { libc::CMSG_SPACE(2 * std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: all zeros is a valid msghdr.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &raw mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control.len() as _;
    // SAFETY: the header points at live buffers.
    let got = unsafe { libc::recvmsg(socket.as_raw_fd(), &raw mut header, libc::MSG_CMSG_CLOEXEC) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote, as the
    // macros do.
    unsafe {
        let mut at = libc::CMSG_FIRSTHDR(&raw const header);
        while !at.is_null() {
            if (*at).cmsg_level == libc::SOL_SOCKET && (*at).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(at);
                let count = ((*at).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for place in 0..count {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(place));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            at = libc::CMSG_NXTHDR(&raw const header, at);
        }
    }
    Ok((got as usize, fds))
}

/// The length of the file behind `fd`.
fn file_len(fd: BorrowedFd<'_>) -> io::Result<usize> {
    // SAFETY: all zeros is a valid stat, which fstat fills.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a live descriptor and a stat to fill.
    if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.st_size as usize)
}

/// A connection to the daemon's rings on an interface. Rings attached
/// through it stay attached while any of them, or it, is kept.
pub struct App {
    socket: Arc<OwnedFd>,
}

impl App {
    /// Connects to the daemon on `interface`.
    pub fn connect(interface: &str) -> io::Result<App> {
        let path = socket_path(interface).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no runtime folder, XDG_RUNTIME_DIR",
            )
        })?;
        Self::connect_at(&path)
    }

    /// Connects to the daemon's socket at `path`.
    pub fn connect_at(path: &Path) -> io::Result<App> {
        Ok(App {
            socket: Arc::new(connect(path)?),
        })
    }

    /// Attaches the ring of an input of the stream's entity, the network's
    /// samples to this program.
    pub fn inbound(&self, stream: StreamRef) -> Result<Inbound, Error> {
        let ring = self.attach(false, stream)?;
        let daemon = &ring.view.header().daemon;
        let next_record = daemon.records.load(Ordering::Acquire);
        let next_frame = daemon.head.load(Ordering::Acquire);
        Ok(Inbound {
            ring,
            next_record,
            next_frame,
        })
    }

    /// Attaches the ring of an output of the stream's entity, this
    /// program's samples to the network. What an attachment before queued
    /// is dropped.
    pub fn outbound(&self, stream: StreamRef) -> Result<Outbound, Error> {
        let ring = self.attach(true, stream)?;
        let header = ring.view.header();
        let now = ring.view.cursor().frame;
        header.app.ended.store(now, Ordering::Relaxed);
        header.app.begin.store(now, Ordering::Relaxed);
        header.app.end.store(now, Ordering::Release);
        let next_record = header.daemon.records.load(Ordering::Acquire);
        Ok(Outbound {
            ring,
            next_record,
            lead: Duration::from_millis(2),
        })
    }

    fn attach(&self, out: bool, stream: StreamRef) -> Result<Attached, Error> {
        send(self.socket.as_fd(), &attach_message(out, stream), &[])?;
        let mut answer = [0u8; MESSAGE_LEN];
        let (len, mut fds) = receive(self.socket.as_fd(), &mut answer)?;
        match (len, answer[0], answer[1]) {
            (MESSAGE_LEN, ANSWER, ATTACHED) if fds.len() == 2 => {}
            (MESSAGE_LEN, ANSWER, NO_RING) => return Err(Error::NoRing),
            (MESSAGE_LEN, ANSWER, TAKEN) => return Err(Error::Taken),
            (0, ..) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "the daemon closed the connection",
                )));
            }
            _ => return Err(Error::Unknown),
        }
        let wake = fds.pop().ok_or(Error::Unknown)?;
        let memory = fds.pop().ok_or(Error::Unknown)?;
        let len = file_len(memory.as_fd())?;
        if len < HEADER_LEN {
            return Err(Error::Unknown);
        }
        let first = View {
            map: Map::of(memory.as_fd(), HEADER_LEN)?,
            shape: Shape {
                channels: 1,
                frames: 1,
                records: 1,
            },
        };
        let header = first.header();
        let read = |value: &AtomicU32| value.load(Ordering::Relaxed);
        let (channels, frames, records) = (
            read(&header.channels) as usize,
            u64::from(read(&header.frames)),
            u64::from(read(&header.records)),
        );
        let shape = Shape {
            channels,
            frames,
            records,
        };
        let fits = header.magic.load(Ordering::Relaxed) == MAGIC
            && read(&header.version) == VERSION
            && read(&header.direction) == u32::from(out)
            && channels > 0
            && frames.is_power_of_two()
            && records.is_power_of_two()
            && shape.len() <= len;
        drop(first);
        if !fits {
            return Err(Error::Unknown);
        }
        Ok(Attached {
            view: View {
                map: Map::of(memory.as_fd(), shape.len())?,
                shape,
            },
            wake,
            _app: self.socket.clone(),
        })
    }
}

/// A ring as an attachment has it.
struct Attached {
    view: View,
    wake: OwnedFd,
    _app: Arc<OwnedFd>,
}

impl Attached {
    fn daemon(&self) -> &DaemonLine {
        &self.view.header().daemon
    }

    fn check(&self) -> Result<(), Error> {
        match self.daemon().retired.load(Ordering::Acquire) {
            0 => Ok(()),
            _ => Err(Error::Retired),
        }
    }

    fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.map_or(-1, |timeout| {
            timeout.as_millis().min(i32::MAX as u128) as i32
        });
        // SAFETY: one live pollfd.
        let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::Interrupted => Ok(false),
                _ => Err(error),
            };
        }
        if ready == 0 {
            return Ok(false);
        }
        let mut count = [0u8; 8];
        // SAFETY: reads 8 octets into a buffer of 8; the eventfd doesn't
        // block, so a wake another read took reads nothing.
        unsafe { libc::read(self.wake.as_raw_fd(), count.as_mut_ptr().cast(), 8) };
        Ok(true)
    }

    fn stats(&self) -> RingStats {
        let daemon = self.daemon();
        let count = |value: &AtomicU64| value.load(Ordering::Relaxed);
        let least_margin = daemon.least_margin.swap(i64::MAX, Ordering::Relaxed);
        let least_queued = daemon.least_queued.swap(i64::MAX, Ordering::Relaxed);
        RingStats {
            late: count(&daemon.late),
            missed: count(&daemon.missed),
            dry_frames: count(&daemon.dry_frames),
            dry_runs: count(&daemon.dry_runs),
            not_sent: count(&daemon.not_sent),
            least_margin: (least_margin != i64::MAX).then_some(least_margin),
            least_queued: (least_queued != i64::MAX).then_some(least_queued as u64),
        }
    }
}

/// A ring's counts since it was made, and its worst since they were last
/// taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingStats {
    /// Inbound, blocks handled after their presentation time; outbound,
    /// frames left out as too late to send.
    pub late: u64,
    /// Inbound, frames the sequence numbers say went missing.
    pub missed: u64,
    /// Outbound, frames sent as silence for want of queued audio, and
    /// the times the queue ran dry.
    pub dry_frames: u64,
    pub dry_runs: u64,
    /// Outbound, blocks built that no destination took.
    pub not_sent: u64,
    /// Inbound, the least time before its presentation time a block was
    /// handled, in nanoseconds, negative when late.
    pub least_margin: Option<i64>,
    /// Outbound, the fewest frames queued past one going out.
    pub least_queued: Option<u64>,
}

/// What a ring's handle has in common, either way.
macro_rules! ring_common {
    () => {
        /// The ring's channels, which its frames have.
        pub fn channels(&self) -> usize {
            self.ring.view.shape.channels
        }

        /// The stream's sample rate; 0 before it first runs.
        pub fn rate(&self) -> u32 {
            self.ring.daemon().rate.load(Ordering::Relaxed)
        }

        /// Whether the stream's talker or listener runs.
        pub fn running(&self) -> bool {
            self.ring.daemon().running.load(Ordering::Relaxed) == 1
        }

        /// Counts the stream's starts.
        pub fn generation(&self) -> u32 {
            self.ring.daemon().generation.load(Ordering::Relaxed)
        }

        /// The daemon wakes this ring's eventfd as its stream passes each
        /// multiple of `frames`; never at 0.
        pub fn wake_every(&self, frames: u32) {
            self.ring
                .view
                .header()
                .app
                .wake_frames
                .store(frames, Ordering::Relaxed);
        }

        /// The eventfd, to wait on with others, as through epoll: it
        /// reads as ready after a wake, until read.
        pub fn wake_fd(&self) -> BorrowedFd<'_> {
            self.ring.wake.as_fd()
        }

        /// Waits for a wake, as long as `timeout` at most, saying whether
        /// one came.
        pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
            self.ring.wait(timeout)
        }

        /// The ring's counts, and its worst since they were last taken.
        pub fn take_stats(&self) -> RingStats {
            self.ring.stats()
        }
    };
}

/// A block an inbound ring received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Its first frame's number in the ring.
    pub frame: u64,
    pub frames: usize,
    /// When its first frame is presented, in gPTP nanoseconds.
    pub presented: Option<i64>,
    pub flags: Flags,
    /// Frames missing just before it, by the sequence numbers.
    pub missed: u8,
}

/// An input's ring: the network's samples, block by block. A block's
/// frames follow the one before's in the ring with no silence for frames
/// missed; place them by their presentation times.
pub struct Inbound {
    ring: Attached,
    next_record: u64,
    /// The frame after the last block read.
    next_frame: u64,
}

impl Inbound {
    ring_common!();

    /// The next block, its frames in `samples`, interleaved; none while
    /// there is none yet.
    pub fn next(&mut self, samples: &mut Vec<i32>) -> Result<Option<Received>, Error> {
        samples.clear();
        let view = &self.ring.view;
        let daemon = self.ring.daemon();
        let records = daemon.records.load(Ordering::Acquire);
        if records == self.next_record {
            self.ring.check()?;
            return Ok(None);
        }
        if records - self.next_record >= view.shape.records {
            return Err(self.overrun(records));
        }
        let (frame, presented, frames, flags, missed) = view.record(self.next_record).load();
        let head = daemon.head.load(Ordering::Acquire);
        if head.saturating_sub(frame) > view.shape.frames || frames as u64 > view.shape.frames {
            return Err(self.overrun(records));
        }
        samples.reserve(frames * view.shape.channels);
        for at in frame..frame + frames as u64 {
            samples.extend(
                view.slot(at)
                    .iter()
                    .map(|word| word.load(Ordering::Relaxed)),
            );
        }
        // What was copied stands only if the daemon did not write over it
        // meanwhile.
        fence(Ordering::Acquire);
        let records = daemon.records.load(Ordering::Relaxed);
        let head = daemon.head.load(Ordering::Relaxed);
        if records - self.next_record > view.shape.records
            || head.saturating_sub(frame) > view.shape.frames
        {
            samples.clear();
            return Err(self.overrun(records));
        }
        self.next_record += 1;
        self.next_frame = frame + frames as u64;
        Ok(Some(Received {
            frame,
            frames,
            presented,
            flags,
            missed,
        }))
    }

    /// Goes on from the latest block written, saying how many frames were
    /// passed.
    fn overrun(&mut self, records: u64) -> Error {
        let head = self.ring.daemon().head.load(Ordering::Acquire);
        let lost_frames = head.saturating_sub(self.next_frame);
        self.next_record = records;
        self.next_frame = head;
        Error::Overrun { lost_frames }
    }
}

/// The next frame of an outbound ring to go out, and when it is
/// presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub frame: u64,
    pub presented: Option<i64>,
}

/// Where frames queued went in an outbound ring: the first's number, and
/// when it is presented while the stream's timeline holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub frame: u64,
    pub presented: Option<i64>,
}

/// A block an outbound ring's talker built, or left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub frame: u64,
    pub frames: usize,
    /// When its first frame is presented, in gPTP nanoseconds, as it went
    /// out stamped.
    pub presented: Option<i64>,
    pub flags: Flags,
}

/// An output's ring: this program's samples to the network. Frames are
/// queued ahead of the talker, which takes them as each block is due, and
/// sends silence where nothing is queued.
pub struct Outbound {
    ring: Attached,
    next_record: u64,
    /// How far ahead of the talker a write lands after a dry run.
    lead: Duration,
}

impl Outbound {
    ring_common!();

    /// How far ahead of the talker a write lands when nothing is queued;
    /// 2 ms at first.
    pub fn set_lead(&mut self, lead: Duration) {
        self.lead = lead;
    }

    /// The lead in frames, at the stream's rate, 48 kHz before it runs.
    fn lead_frames(&self) -> u64 {
        let rate = match self.rate() {
            0 => 48_000,
            rate => u64::from(rate),
        };
        (self.lead.as_nanos() as u64 * rate / 1_000_000_000).max(self.guard())
    }

    /// Frames ahead of the talker that it may be reading: two of the
    /// longest blocks, 250 µs.
    fn guard(&self) -> u64 {
        (u64::from(self.rate()) / 2000).max(24)
    }

    fn app(&self) -> &AppLine {
        &self.ring.view.header().app
    }

    /// The next frame to go out, and when it is presented.
    pub fn cursor(&self) -> Cursor {
        self.ring.view.cursor()
    }

    /// Frames queued ahead of the talker.
    pub fn queued(&self) -> u64 {
        self.app()
            .end
            .load(Ordering::Relaxed)
            .saturating_sub(self.cursor().frame)
    }

    /// When frame `frame` is presented, from the cursor, while the
    /// stream's timeline holds.
    fn presented_at(&self, cursor: Cursor, frame: u64) -> Option<i64> {
        let rate = i64::from(self.rate());
        let presented = cursor.presented?;
        (rate > 0).then(|| presented + (frame as i64 - cursor.frame as i64) * 1_000_000_000 / rate)
    }

    /// Queues `samples`, interleaved frames, after what is queued, or the
    /// lead ahead of the talker where nothing is.
    pub fn write(&mut self, samples: &[i32]) -> Result<Placed, Error> {
        self.ring.check()?;
        let channels = self.channels();
        let frames = (samples.len() / channels) as u64;
        let cursor = self.cursor();
        let app = self.app();
        let end = app.end.load(Ordering::Relaxed);
        let start = if end < cursor.frame + self.guard() {
            cursor.frame + self.lead_frames()
        } else {
            end
        };
        let room = (cursor.frame + self.ring.view.shape.frames).saturating_sub(start);
        if frames > room {
            return Err(Error::Full {
                room: room as usize,
            });
        }
        if start != end {
            app.begin.store(start, Ordering::Relaxed);
        }
        let view = &self.ring.view;
        for (at, frame) in (start..).zip(samples.chunks_exact(channels)) {
            for (slot, word) in view.slot(at).iter().zip(frame) {
                slot.store(*word, Ordering::Relaxed);
            }
        }
        app.end.store(start + frames, Ordering::Release);
        Ok(Placed {
            frame: start,
            presented: self.presented_at(cursor, start),
        })
    }

    /// Writes `samples` over queued frames from `frame` on, as for a fade
    /// or a duck, while they are far enough from going out; `TooLate` if
    /// the talker reached them, which may have sent some as they were.
    pub fn overwrite(&mut self, frame: u64, samples: &[i32]) -> Result<(), Error> {
        self.ring.check()?;
        let channels = self.channels();
        let frames = (samples.len() / channels) as u64;
        if frame < self.cursor().frame + self.guard() {
            return Err(Error::TooLate);
        }
        let app = self.app();
        if frame < app.begin.load(Ordering::Relaxed)
            || frame + frames > app.end.load(Ordering::Relaxed)
        {
            return Err(Error::NotQueued);
        }
        let view = &self.ring.view;
        for (at, values) in (frame..).zip(samples.chunks_exact(channels)) {
            for (slot, word) in view.slot(at).iter().zip(values) {
                slot.store(*word, Ordering::Relaxed);
            }
        }
        fence(Ordering::SeqCst);
        if self.cursor().frame > frame {
            return Err(Error::TooLate);
        }
        Ok(())
    }

    /// Ends what is queued at frame `end`, or at the talker where it is
    /// past that: the frames after are dropped, and silence follows,
    /// flagged idle rather than dry.
    pub fn truncate(&mut self, end: u64) -> Cursor {
        let cursor = self.cursor();
        let end = end.max(cursor.frame);
        let app = self.app();
        if end < app.end.load(Ordering::Relaxed) {
            app.ended.store(end, Ordering::Relaxed);
            app.end.store(end, Ordering::Release);
        }
        cursor
    }

    /// Drops everything queued: silence from the talker on.
    pub fn flush(&mut self) -> Cursor {
        let cursor = self.cursor();
        self.truncate(cursor.frame)
    }

    /// Says what is queued ends on purpose: the silence after it is idle,
    /// not dry.
    pub fn finish(&mut self) {
        let app = self.app();
        app.ended
            .store(app.end.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// The next block the talker built or left out; none while there is
    /// none yet.
    pub fn sent(&mut self) -> Result<Option<Sent>, Error> {
        let view = &self.ring.view;
        let records = self.ring.daemon().records.load(Ordering::Acquire);
        if records == self.next_record {
            self.ring.check()?;
            return Ok(None);
        }
        if records - self.next_record >= view.shape.records {
            self.next_record = records - 1;
        }
        let (frame, presented, frames, flags, _) = view.record(self.next_record).load();
        fence(Ordering::Acquire);
        if self.ring.daemon().records.load(Ordering::Relaxed) - self.next_record
            > view.shape.records
        {
            self.next_record = records;
            return Ok(None);
        }
        self.next_record += 1;
        Ok(Some(Sent {
            frame,
            frames,
            presented,
            flags,
        }))
    }
}
