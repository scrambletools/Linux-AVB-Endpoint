//! Extra class A load on a link, to test a card and a bridge under more
//! streams than the bench's own: 1 to 3 AAF streams of silence, 8
//! channels at 48 kHz, 8000 frames a second each, every one paced on gPTP
//! time by a thread of its own at a real-time priority below the streams'
//! 15 (asked of RealtimeKit), tagged PCP 3 on VLAN 2 and queued at socket
//! priority 3. It declares nothing, no MSRP and no ATDECC entity, and
//! takes no daemon's claim on the interface, so it runs beside
//! avb-endpointd. Its raw sockets come from avb-netraw, as the daemon's
//! do.
//!
//!   load_talker <interface> [--streams 1-3] [--dest maap|<unicast MAC>]
//!               [--rr 1-14] [--listen <seconds>]
//!
//! - `--dest maap`, the default: the streams go to a block of MAAP
//!   addresses claimed for them. It first listens for the blocks others
//!   hold, 33 s by default (`--listen`, 0 to 120), past one announce of
//!   each. This computer's daemon sends from the same address, so it
//!   never defends a block against this one. The claim gives way to any
//!   block it hears of, before or after, and the streams stop while it
//!   claims another.
//! - `--dest <MAC>`: every stream to that unicast address, with no MAAP.
//!   A multicast address is refused, as it could be another stream's.
//! - `--rr`: the threads' SCHED_RR priority, 14 by default.
//!
//! It stops within a second of SIGINT or SIGTERM and prints each stream's
//! frames sent, late (left out as due too late, the thread having woken
//! late) and refused by the interface.

// The pacing threads' sleeps and scheduling, as libc gives them.
#![allow(unsafe_code)]

use std::io;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use atdecc::Instant;
use atdecc::maap::{self, Maap, MaapEvent, Message, MessageType};
use avb_net::stream::FrameSender;
use avb_net::{MacAddress, Socket};
use avb_stream::MediaClock;
use avb_stream::formats;
use avb_stream::media::{Media, Packetizer};

const USAGE: &str = "usage: load_talker <interface> [--streams 1-3] \
                     [--dest maap|<unicast MAC>] [--rr 1-14] [--listen <seconds>]";
/// Exit status for arguments it cannot take.
const EXIT_USAGE: u8 = 2;
const MOST_STREAMS: usize = 3;
/// SCHED_RR priorities the threads may ask: below the streams' 15.
const PRIORITIES: std::ops::RangeInclusive<u32> = 1..=14;
const PRIORITY: u32 = 14;
/// Long enough to hear one announce of every block held: MAAP announces
/// each 30 s, give or take 2.
const LISTEN: Duration = Duration::from_secs(33);
const MOST_LISTEN: u64 = 120;
/// How long a block heard of is kept in mind after it was last heard:
/// past two announces.
const HEARD_FOR: Duration = Duration::from_secs(70);
/// Class A's priority and its usual VLAN.
const PCP: u16 = 3;
const VLAN_ID: u16 = 2;
const ETHERTYPE_VLAN: u16 = 0x8100;
/// The Ethernet header with its VLAN tag.
const ETHERNET_LEN: usize = 18;
/// The streams' unique IDs, after the interface's address: high, away
/// from the daemon's.
const FIRST_UNIQUE: u16 = 0xffe0;
/// Presentation times this far past sending, and a frame left out when
/// it would go less than `CROSSING` before its time, as avb-stream's
/// talkers do; in nanoseconds.
const TRANSIT: i64 = 2_000_000;
const CROSSING: i64 = 500_000;
/// A timeline starts on a millisecond at least this far ahead.
const LEAD: i64 = 1_000_000;
/// The longest a pacing thread sleeps at once, to follow a clock that
/// moved; how long it waits while its stream has nowhere to go; and how
/// long the claim waits for a frame before looking at the time.
const LONGEST_SLEEP: Duration = Duration::from_millis(20);
const IDLE: Duration = Duration::from_millis(10);
const POLL: Duration = Duration::from_millis(100);
/// The most CPU time a real-time thread may take without sleeping, in
/// microseconds, which RealtimeKit wants a process to promise first.
const MOST_TIME: u64 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    Maap,
    Unicast(MacAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    interface: String,
    streams: usize,
    dest: Dest,
    priority: u32,
    listen: Duration,
}

fn value<'a>(words: &mut impl Iterator<Item = &'a String>, name: &str) -> Result<&'a str, String> {
    words
        .next()
        .map(String::as_str)
        .ok_or_else(|| format!("{name} needs a value"))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut interface = None;
    let mut streams = 1;
    let mut dest = Dest::Maap;
    let mut priority = PRIORITY;
    let mut listen = LISTEN;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--streams" => {
                streams = value(&mut words, word)?
                    .parse()
                    .ok()
                    .filter(|count| (1..=MOST_STREAMS).contains(count))
                    .ok_or_else(|| format!("--streams takes 1 to {MOST_STREAMS}"))?;
            }
            "--dest" => dest = parse_dest(value(&mut words, word)?)?,
            "--rr" => {
                priority = value(&mut words, word)?
                    .parse()
                    .ok()
                    .filter(|priority| PRIORITIES.contains(priority))
                    .ok_or_else(|| {
                        format!(
                            "--rr takes {} to {}, below the streams' 15",
                            PRIORITIES.start(),
                            PRIORITIES.end()
                        )
                    })?;
            }
            "--listen" => {
                listen = value(&mut words, word)?
                    .parse()
                    .ok()
                    .filter(|seconds| *seconds <= MOST_LISTEN)
                    .map(Duration::from_secs)
                    .ok_or_else(|| format!("--listen takes 0 to {MOST_LISTEN} seconds"))?;
            }
            name if name.starts_with('-') => return Err(format!("no option {name}")),
            name if interface.is_none() => interface = Some(name.to_owned()),
            name => return Err(format!("one interface only, not {name} too")),
        }
    }
    Ok(Options {
        interface: interface.ok_or("no interface given")?,
        streams,
        dest,
        priority,
        listen,
    })
}

fn parse_dest(text: &str) -> Result<Dest, String> {
    if text == "maap" {
        return Ok(Dest::Maap);
    }
    let address: MacAddress = text
        .parse()
        .map_err(|_| format!("--dest takes maap or a MAC address, not {text}"))?;
    if address.is_multicast() {
        return Err(format!(
            "{address} is multicast, which could be another stream's: give maap, or a unicast address"
        ));
    }
    if address.0 == [0; 6] {
        return Err("--dest 00:00:00:00:00:00 goes nowhere".into());
    }
    Ok(Dest::Unicast(address))
}

fn number(address: MacAddress) -> u64 {
    let mut octets = [0; 8];
    octets[2..].copy_from_slice(&address.0);
    u64::from_be_bytes(octets)
}

fn address(number: u64) -> MacAddress {
    let mut octets = [0; 6];
    octets.copy_from_slice(&number.to_be_bytes()[2..]);
    MacAddress(octets)
}

/// A stream's ID: the interface's address and a unique ID of its own.
fn stream_id(mac: MacAddress, place: usize) -> u64 {
    (number(mac) << 16) | u64::from(FIRST_UNIQUE + place as u16)
}

/// The address the claim goes by: above every real one, so that it gives
/// way in every conflict, as the lower address keeps a block.
fn yielding(mac: MacAddress) -> MacAddress {
    MacAddress([0xff, 0xff, 0xff, mac.0[3], mac.0[4], mac.0[5]])
}

/// The Ethernet header, VLAN tag and AVTP ethertype, into a frame's first
/// [`ETHERNET_LEN`] octets.
fn write_header(frame: &mut [u8], destination: MacAddress, source: MacAddress) {
    frame[0..6].copy_from_slice(&destination.0);
    frame[6..12].copy_from_slice(&source.0);
    frame[12..14].copy_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
    frame[14..16].copy_from_slice(&((PCP << 13) | VLAN_ID).to_be_bytes());
    frame[16..18].copy_from_slice(&atdecc::ETHERTYPE_AVTP.to_be_bytes());
}

/// A new timeline's first frame time: on a millisecond, at least
/// [`LEAD`] past `now`, gPTP nanoseconds.
fn first_due(now: i64) -> i64 {
    let at = now + LEAD;
    at + (1_000_000 - at.rem_euclid(1_000_000)) % 1_000_000
}

/// One stream, as its thread and the rest share it.
struct Stream {
    id: u64,
    /// Where its frames go, as a number; 0 while it has nowhere.
    destination: AtomicU64,
    sent: AtomicU64,
    late: AtomicU64,
    refused: AtomicU64,
    /// The SCHED_RR priority its thread runs at, 0 if not real time.
    priority: AtomicU32,
}

impl Stream {
    fn new(id: u64) -> Self {
        Stream {
            id,
            destination: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            late: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            priority: AtomicU32::new(0),
        }
    }

    fn go_to(&self, destination: Option<MacAddress>) {
        self.destination
            .store(destination.map_or(0, number), Ordering::Relaxed);
    }
}

fn monotonic() -> Duration {
    avb_net::clock::monotonic_now()
}

/// Sleeps until the monotonic clock reaches `at`.
fn sleep_until(at: Duration) {
    // SAFETY: all zeros is a valid timespec, whatever padding it has.
    let mut until: libc::timespec = unsafe { std::mem::zeroed() };
    until.tv_sec = at.as_secs() as libc::time_t;
    until.tv_nsec = at.subsec_nanos() as libc::c_long;
    // SAFETY: a valid timespec that lives through the call, and no
    // remainder asked for.
    unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &raw const until,
            std::ptr::null_mut(),
        );
    }
}

/// Promises RealtimeKit the most CPU time a thread takes without
/// sleeping, which it wants before it raises one.
fn promise_time() {
    let limit = libc::rlimit {
        rlim_cur: MOST_TIME,
        rlim_max: MOST_TIME,
    };
    // SAFETY: a valid rlimit that lives through the call.
    unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &raw const limit) };
}

/// Asks RealtimeKit to run the calling thread SCHED_RR at `priority`,
/// and says the priority it runs at after, 0 if not real time.
fn raise(priority: u32) -> u32 {
    // SAFETY: gettid takes nothing and cannot fail.
    let thread = unsafe { libc::gettid() };
    let _ = Command::new("busctl")
        .args([
            "call",
            "--system",
            "org.freedesktop.RealtimeKit1",
            "/org/freedesktop/RealtimeKit1",
            "org.freedesktop.RealtimeKit1",
            "MakeThreadRealtimeWithPID",
            "ttu",
        ])
        .arg(std::process::id().to_string())
        .arg(thread.to_string())
        .arg(priority.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // SAFETY: plain calls on this thread, the parameter living through
    // the second.
    unsafe {
        let policy = libc::sched_getscheduler(0) & !libc::SCHED_RESET_ON_FORK;
        let mut param: libc::sched_param = std::mem::zeroed();
        if policy != libc::SCHED_RR || libc::sched_getparam(0, &raw mut param) < 0 {
            return 0;
        }
        param.sched_priority as u32
    }
}

/// A stream's thread: its frames, each when due on gPTP time, until
/// `stop`, while it has somewhere to go.
fn pace(
    sender: &FrameSender,
    mac: MacAddress,
    media: Media,
    stream: &Stream,
    clock: &MediaClock,
    stop: &AtomicBool,
    priority: u32,
) {
    stream.priority.store(raise(priority), Ordering::Relaxed);
    let interval = media.interval_nanos() as i64;
    let samples =
        vec![0.0f32; usize::from(media.channels()) * usize::from(media.samples_per_frame())];
    let mut frame = vec![0u8; ETHERNET_LEN + media.pdu_length()];
    let mut packetizer = Packetizer::new(media, stream.id);
    // The next frame's time, on the clock's line of this generation.
    let mut timeline: Option<(u32, i64)> = None;
    let mut reset = true;
    while !stop.load(Ordering::Relaxed) {
        let destination = stream.destination.load(Ordering::Relaxed);
        if destination == 0 {
            timeline = None;
            std::thread::sleep(IDLE);
            continue;
        }
        let generation = clock.generation();
        let due = match timeline {
            Some((was, due)) if was == generation => due,
            _ => {
                reset = true;
                first_due(clock.now())
            }
        };
        timeline = Some((generation, due));
        let wake = clock.monotonic_at(due);
        let now = monotonic();
        if wake > now {
            sleep_until(wake.min(now + LONGEST_SLEEP));
            continue;
        }
        let presentation = due + TRANSIT;
        if clock.now() > presentation - CROSSING {
            packetizer.skip(1);
            stream.late.fetch_add(1, Ordering::Relaxed);
        } else {
            write_header(&mut frame, address(destination), mac);
            packetizer.write(presentation, reset, &samples, &mut frame[ETHERNET_LEN..]);
            reset = false;
            match sender.send(&frame) {
                Ok(()) => stream.sent.fetch_add(1, Ordering::Relaxed),
                Err(_) => stream.refused.fetch_add(1, Ordering::Relaxed),
            };
        }
        timeline = Some((generation, due + interval));
    }
}

/// The blocks others hold, as last heard: who, the block's first address
/// and size, and when.
#[derive(Default)]
struct Heard {
    blocks: Vec<(MacAddress, u64, u16, Duration)>,
}

impl Heard {
    /// Notes what `message` from `source` says is held or wanted.
    fn note(&mut self, at: Duration, source: MacAddress, message: &Message) {
        let (start, count) = match message.message_type {
            MessageType::Defend => (message.conflict_start, message.conflict_count),
            MessageType::Probe | MessageType::Announce => {
                (message.requested_start, message.requested_count)
            }
        };
        let start = number(start);
        self.blocks.retain(|block| {
            at.saturating_sub(block.3) < HEARD_FOR && (block.0, block.1) != (source, start)
        });
        if count > 0 {
            self.blocks.push((source, start, count, at));
        }
    }

    /// A block heard of within [`HEARD_FOR`] of `at` that `count`
    /// addresses from `first` overlap, and who holds it.
    fn overlapping(
        &self,
        at: Duration,
        first: MacAddress,
        count: u16,
    ) -> Option<(MacAddress, u64, u16)> {
        let first = number(first);
        self.blocks
            .iter()
            .filter(|block| at.saturating_sub(block.3) < HEARD_FOR)
            .find(|block| {
                block.1 < first + u64::from(count) && first < block.1 + u64::from(block.2)
            })
            .map(|block| (block.0, block.1, block.2))
    }
}

/// What the claim makes of a MAAP PDU `source` sent: this computer's
/// own address is the daemon's, which never defends against this claim,
/// so its probe counts as a block held, and the claim gives way.
fn as_heard(source: MacAddress, mac: MacAddress, message: Message) -> Message {
    if source == mac && message.message_type == MessageType::Probe {
        Message {
            message_type: MessageType::Announce,
            ..message
        }
    } else {
        message
    }
}

/// Claims a MAAP block for `streams`, gives each its address, and keeps
/// the claim until `stop`.
fn claim(
    options: &Options,
    mac: MacAddress,
    streams: &[Arc<Stream>],
    stop: &AtomicBool,
) -> io::Result<()> {
    let socket = Socket::open(&options.interface, atdecc::ETHERTYPE_AVTP)?;
    socket.join_multicast(maap::DESTINATION)?;
    let _ =
        socket.keep_payloads_starting(atdecc::avtp::subtype::MAAP..=atdecc::avtp::subtype::MAAP);
    let started = std::time::Instant::now();
    let now = || Instant::from_nanos(started.elapsed().as_nanos() as u64);
    let mut heard = Heard::default();
    let mut buffer = [0u8; 1522];
    let mut receive = |heard: &mut Heard, wait: Duration| -> Option<(MacAddress, Message)> {
        match socket.receive(&mut buffer, Some(wait)) {
            Ok(Some(received)) => {
                let message = Message::decode(&buffer[..received.length])?;
                heard.note(started.elapsed(), received.source, &message);
                Some((received.source, message))
            }
            Ok(None) => None,
            Err(_) => {
                std::thread::sleep(wait);
                None
            }
        }
    };
    println!(
        "listening {} s for the MAAP blocks others hold",
        options.listen.as_secs()
    );
    while started.elapsed() < options.listen {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _ = receive(&mut heard, POLL);
    }
    let count = streams.len() as u16;
    let mut maap = Maap::new(yielding(mac), count);
    maap.start(now());
    // The claim gave a block up for one heard of before, and the loss it
    // reports for it is no news.
    let mut gave_up = false;
    while !stop.load(Ordering::Relaxed) {
        while let Some(event) = maap.poll_event() {
            match event {
                MaapEvent::Acquired(first) => {
                    if let Some((holder, start, size)) =
                        heard.overlapping(started.elapsed(), first, count)
                    {
                        println!(
                            "MAAP: {first} is in {holder}'s block of {size} from {}, claiming another",
                            address(start)
                        );
                        // Its announce, not sent, and the block's held.
                        while maap.poll_transmit().is_some() {}
                        let held = Message {
                            message_type: MessageType::Announce,
                            requested_start: address(start),
                            requested_count: size,
                            conflict_start: MacAddress([0; 6]),
                            conflict_count: 0,
                        };
                        maap.handle_frame(now(), holder, &held.encode());
                        gave_up = true;
                        continue;
                    }
                    for (place, stream) in streams.iter().enumerate() {
                        let to = address(number(first) + place as u64);
                        stream.go_to(Some(to));
                        println!("stream {place} {:016x} goes to {to}", stream.id);
                    }
                }
                MaapEvent::Lost => {
                    for stream in streams {
                        stream.go_to(None);
                    }
                    if !std::mem::take(&mut gave_up) {
                        println!(
                            "MAAP: another device took the block, so the streams stop while it claims another"
                        );
                    }
                }
            }
        }
        while let Some((to, pdu)) = maap.poll_transmit() {
            if let Err(error) = socket.send(to, &pdu) {
                eprintln!("MAAP: not sent: {error}");
            }
        }
        let wait = maap
            .poll_timeout()
            .map_or(POLL, |at| at.saturating_duration_since(now()))
            .min(POLL);
        if let Some((source, message)) = receive(&mut heard, wait) {
            let message = as_heard(source, mac, message);
            maap.handle_frame(now(), source, &message.encode());
        }
        maap.handle_timeout(now());
    }
    Ok(())
}

fn run(options: &Options, mac: MacAddress) -> io::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .map_err(io::Error::other)?;
    }
    let clock = Arc::new(MediaClock::gptpd(&options.interface));
    if !clock.locked() {
        eprintln!(
            "gptpd gives no gPTP time for {}: presentation times follow this computer's clock",
            options.interface
        );
    }
    let format = formats::aaf(48_000, 8, 32, formats::CLASS_A_FRAMES);
    let media =
        Media::of(format).ok_or_else(|| io::Error::other("no AAF layout for the format"))?;
    let streams: Vec<Arc<Stream>> = (0..options.streams)
        .map(|place| Arc::new(Stream::new(stream_id(mac, place))))
        .collect();
    let senders = streams
        .iter()
        .map(|_| FrameSender::open(&options.interface))
        .collect::<io::Result<Vec<_>>>()?;
    promise_time();
    let started = monotonic();
    let threads = senders
        .into_iter()
        .zip(&streams)
        .enumerate()
        .map(|(place, (sender, stream))| {
            let (stream, clock, stop) = (stream.clone(), clock.clone(), stop.clone());
            let priority = options.priority;
            std::thread::Builder::new()
                .name(format!("load {place}"))
                .spawn(move || pace(&sender, mac, media, &stream, &clock, &stop, priority))
        })
        .collect::<io::Result<Vec<_>>>();
    let claimed = match (threads.is_ok(), options.dest) {
        (false, _) => Ok(()),
        (true, Dest::Maap) => claim(options, mac, &streams, &stop),
        (true, Dest::Unicast(to)) => {
            for (place, stream) in streams.iter().enumerate() {
                stream.go_to(Some(to));
                println!("stream {place} {:016x} goes to {to}", stream.id);
            }
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
            }
            Ok(())
        }
    };
    stop.store(true, Ordering::Relaxed);
    let threads = threads?;
    for thread in threads {
        let _ = thread.join();
    }
    let ran = monotonic().saturating_sub(started);
    println!("stopped after {:.1} s", ran.as_secs_f64());
    for (place, stream) in streams.iter().enumerate() {
        let priority = stream.priority.load(Ordering::Relaxed);
        println!(
            "stream {place} {:016x}: sent {}, late {}, refused {}, {}",
            stream.id,
            stream.sent.load(Ordering::Relaxed),
            stream.late.load(Ordering::Relaxed),
            stream.refused.load(Ordering::Relaxed),
            if priority > 0 {
                format!("SCHED_RR {priority}")
            } else {
                "not real time".to_owned()
            }
        );
    }
    claimed
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // Raw sockets come from avb-netraw, as the daemon's do.
    let _ = avb_net::broker::use_helper("avb-netraw", &[Path::new("/usr/lib/avb-endpointd")]);
    let Some(mac) = avb_net::interfaces()
        .into_iter()
        .find(|found| found.name == options.interface)
        .map(|found| found.mac)
    else {
        eprintln!("no interface named {}", options.interface);
        return ExitCode::FAILURE;
    };
    match run(&options, mac) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("load_talker: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn options_take_counts_destinations_and_priorities_in_range() {
        let options = parse(&args("enp2s0")).unwrap();
        assert_eq!(
            options,
            Options {
                interface: "enp2s0".into(),
                streams: 1,
                dest: Dest::Maap,
                priority: 14,
                listen: LISTEN,
            }
        );
        let options = parse(&args(
            "--streams 3 enp2s0 --dest a0:10:a3:a5:17:26 --rr 10 --listen 0",
        ))
        .unwrap();
        assert_eq!(options.streams, 3);
        assert_eq!(
            options.dest,
            Dest::Unicast(MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x26]))
        );
        assert_eq!((options.priority, options.listen), (10, Duration::ZERO));
        assert_eq!(parse(&args("v0 --dest maap")).unwrap().dest, Dest::Maap);
        for wrong in [
            "",
            "v0 v1",
            "v0 --streams 0",
            "v0 --streams 4",
            "v0 --rr 15",
            "v0 --rr 0",
            "v0 --listen 121",
            "v0 --dest",
            "v0 --dest 91:e0:f0:00:77:40",
            "v0 --dest ff:ff:ff:ff:ff:ff",
            "v0 --dest 00:00:00:00:00:00",
            "v0 --dest nowhere",
            "v0 --fast",
        ] {
            assert!(parse(&args(wrong)).is_err(), "{wrong}");
        }
    }

    #[test]
    fn frames_carry_class_a_tags_and_ids_after_the_address() {
        let mac = MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x26]);
        assert_eq!(stream_id(mac, 0), 0xa010_a3a5_1726_ffe0);
        assert_eq!(stream_id(mac, 2), 0xa010_a3a5_1726_ffe2);
        let mut frame = [0u8; ETHERNET_LEN];
        let to = MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x64, 0x72]);
        write_header(&mut frame, to, mac);
        assert_eq!(&frame[..6], &to.0);
        assert_eq!(&frame[6..12], &mac.0);
        assert_eq!(frame[12..], [0x81, 0x00, 0x60, 0x02, 0x22, 0xf0]);
        assert_eq!(
            address(number(to) + 1),
            MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x64, 0x73])
        );
    }

    #[test]
    fn timelines_start_on_a_millisecond_past_the_lead() {
        assert_eq!(first_due(0), 1_000_000);
        assert_eq!(first_due(1), 2_000_000);
        assert_eq!(first_due(999_999), 2_000_000);
        assert_eq!(first_due(5_000_000), 6_000_000);
    }

    #[test]
    fn the_claim_gives_way_to_blocks_heard_of_and_to_this_computers_probes() {
        let mac = MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x26]);
        assert!(number(yielding(mac)) > number(MacAddress([0xfe, 0xff, 0xff, 0xff, 0xff, 0xff])));
        let block = |message_type, start: u64, count| Message {
            message_type,
            requested_start: address(start),
            requested_count: count,
            conflict_start: MacAddress([0; 6]),
            conflict_count: 0,
        };
        let daemons = 0x91e0_f000_7740;
        let mut heard = Heard::default();
        heard.note(
            Duration::from_secs(1),
            mac,
            &block(MessageType::Announce, daemons, 32),
        );
        let at = Duration::from_secs(40);
        assert!(heard.overlapping(at, address(daemons - 2), 3).is_some());
        assert!(heard.overlapping(at, address(daemons + 31), 1).is_some());
        assert!(heard.overlapping(at, address(daemons - 3), 3).is_none());
        assert!(heard.overlapping(at, address(daemons + 32), 3).is_none());
        // Forgotten once not heard for long.
        assert!(
            heard
                .overlapping(Duration::from_secs(72), address(daemons), 1)
                .is_none()
        );
        // A defend tells the part held.
        let mut heard = Heard::default();
        let other = MacAddress([0x00, 0x01, 0xf2, 0xfe, 0x01, 0x62]);
        let defend = Message {
            message_type: MessageType::Defend,
            requested_start: address(daemons),
            requested_count: 32,
            conflict_start: address(daemons + 4),
            conflict_count: 2,
        };
        heard.note(Duration::ZERO, other, &defend);
        assert!(
            heard
                .overlapping(Duration::ZERO, address(daemons), 4)
                .is_none()
        );
        assert_eq!(
            heard.overlapping(Duration::ZERO, address(daemons + 5), 1),
            Some((other, daemons + 4, 2))
        );
        // This computer's probe holds; another's waits on MAAP itself.
        let probe = block(MessageType::Probe, daemons, 32);
        assert_eq!(
            as_heard(mac, mac, probe).message_type,
            MessageType::Announce
        );
        assert_eq!(as_heard(other, mac, probe).message_type, MessageType::Probe);
    }

    /// The claim's state machine, given way: a block it acquired that
    /// one heard of overlaps is given up for another.
    #[test]
    fn an_acquired_block_heard_held_is_given_up() {
        let mac = MacAddress([0xa0, 0x10, 0xa3, 0xa5, 0x17, 0x26]);
        let mut maap = Maap::new(yielding(mac), 2);
        maap.start(Instant::ZERO);
        let mut at = Instant::ZERO;
        let first = loop {
            at += Duration::from_millis(100);
            maap.handle_timeout(at);
            if let Some(MaapEvent::Acquired(first)) = maap.poll_event() {
                break first;
            }
        };
        while maap.poll_transmit().is_some() {}
        let held = Message {
            message_type: MessageType::Announce,
            requested_start: first,
            requested_count: 1,
            conflict_start: MacAddress([0; 6]),
            conflict_count: 0,
        };
        maap.handle_frame(at, mac, &held.encode());
        assert_eq!(maap.poll_event(), Some(MaapEvent::Lost));
        let (to, pdu) = maap.poll_transmit().expect("a probe for another block");
        assert_eq!(to, maap::DESTINATION);
        let probe = Message::decode(&pdu).unwrap();
        assert_eq!(probe.message_type, MessageType::Probe);
        assert_ne!(probe.requested_start, first);
    }
}
