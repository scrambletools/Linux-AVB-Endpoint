//! `avb-endpointd log`: prints the log another device keeps in a
//! LOG_OBJECT memory object (IEEE 1722.1-2021, 7.2.10), read through
//! Address Access, so a device's log needs no console cable. A device is
//! asked nothing unless this runs. It has one read out per device at a
//! time: every 250 ms while the log grows, the header alone once a second
//! after it stops, and back to back while it is behind (see
//! [`atdecc::log`]).
//!
//! It reads as a controller of its own, `FF-FA` in its entity ID's
//! instance octets, beside the daemon's `FF-FB`. It reads only the models
//! of the devices it looks for: the one named by ID, or, for a name,
//! those that advertise Address Access.
//!
//! With `--stats`, it says on stderr every 10 s, and at the end, what
//! reading cost: its Address Access frames and octets each way, round
//! trips, retries, timeouts and refusals, and each line's latency from
//! when the device logged it to when this read it. A line's latency is
//! the device's milliseconds since boot at the read, from the log header,
//! less the line's own `(ms)`, plus half the round trip. `--stats-json
//! FILE` writes the same as JSON. `--poll MS` reads octets every MS
//! milliseconds, never the header alone: the most reading costs.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use atdecc::adp::EntityCapabilities;
use atdecc::aecp::{AaStatus, AddressAccessPdu, AecpHeader, AecpMessageType};
use atdecc::controller::{CommandId, Config, Controller, Event, Outcome};
use atdecc::descriptor::MemoryObjectType;
use atdecc::log::{LogPiece, LogTail};
use atdecc::{ADP_ACMP_MULTICAST, EntityId, Instant, MacAddress};
use avb_endpointd::status::EXIT_USAGE;
use avb_net::Socket;

/// The instance octet of this command's controller.
const CONTROLLER_INSTANCE: u8 = 0xfa;
/// How long to look for the devices asked for.
const FIND_FOR: Duration = Duration::from_secs(10);
const STATS_EVERY: Duration = Duration::from_secs(10);

struct Options {
    entities: Vec<String>,
    follow: bool,
    stats: bool,
    stats_json: Option<PathBuf>,
    poll: Option<Duration>,
}

fn parse(words: &[String]) -> Result<Options, String> {
    let mut options = Options {
        entities: Vec::new(),
        follow: false,
        stats: false,
        stats_json: None,
        poll: None,
    };
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--follow" | "-f" => options.follow = true,
            "--stats" => options.stats = true,
            "--stats-json" => {
                let file = words.next().ok_or("--stats-json needs a file")?;
                options.stats_json = Some(PathBuf::from(file));
            }
            "--poll" => {
                let millis: u64 = words
                    .next()
                    .and_then(|millis| millis.parse().ok())
                    .filter(|&millis| millis > 0)
                    .ok_or("--poll needs milliseconds, 1 or more")?;
                options.poll = Some(Duration::from_millis(millis));
            }
            flag if flag.starts_with('-') => return Err(format!("no option {flag} for log")),
            entity => options.entities.push(entity.to_owned()),
        }
    }
    if options.entities.is_empty() {
        return Err("log needs a device: its entity ID or its name".into());
    }
    Ok(options)
}

/// A device asked for: its entity ID, or its entity name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    Id(EntityId),
    Name(String),
}

impl Wanted {
    fn of(text: &str) -> Self {
        let digits = text.strip_prefix("0x").unwrap_or(text);
        match u64::from_str_radix(digits, 16) {
            Ok(id) if digits.len() == 16 => Wanted::Id(EntityId(id)),
            _ => Wanted::Name(text.to_owned()),
        }
    }
}

/// What reading a device's log cost, and how late its lines came.
#[derive(Debug, Default)]
struct Stats {
    reads: u64,
    frames_out: u64,
    octets_out: u64,
    frames_in: u64,
    octets_in: u64,
    timeouts: u64,
    refused: u64,
    round_trips: Vec<Duration>,
    log_octets: u64,
    lost: u64,
    dropped_lines: u64,
    restarts: u64,
    lines: u64,
    latencies_ms: Vec<u32>,
}

impl Stats {
    fn retries(&self) -> u64 {
        self.frames_out.saturating_sub(self.reads)
    }
}

/// A device whose log is followed.
struct Board {
    wanted: Wanted,
    entity_id: Option<EntityId>,
    mac: MacAddress,
    name: String,
    tail: Option<LogTail>,
    out: Option<CommandId>,
    /// When each Address Access command with this sequence ID went.
    sent: BTreeMap<u16, Instant>,
    /// The last round trip, for the latency of the lines it brought.
    last_round_trip: Duration,
    line: Vec<u8>,
    /// Octets up to the next newline belong to a line begun before them.
    skip_partial: bool,
    /// The device's milliseconds since boot when its log was first read
    /// this boot: lines logged before then were waiting, not late.
    reading_since_ms: Option<u32>,
    stats: Stats,
}

impl Board {
    fn label(&self) -> String {
        if self.name.is_empty() {
            self.entity_id
                .map_or_else(String::new, |id| format!("0x{:016x}", id.0))
        } else {
            self.name.clone()
        }
    }
}

pub fn run(words: &[String], interface: &str) -> ExitCode {
    let options = match parse(words) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{}", crate::tool::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // Raw sockets come from avb-netraw, as the daemon's do.
    #[cfg(target_os = "linux")]
    let _ = avb_net::broker::use_helper("avb-netraw", &[Path::new("/usr/lib/avb-endpointd")]);
    let Some(mac) = avb_net::interfaces()
        .into_iter()
        .find(|found| found.name == interface)
        .map(|found| found.mac)
    else {
        eprintln!("no interface named {interface}");
        return ExitCode::FAILURE;
    };
    match follow(&options, interface, mac) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("log: {error}");
            ExitCode::FAILURE
        }
    }
}

fn follow(options: &Options, interface: &str, mac: MacAddress) -> Result<(), String> {
    let socket =
        Socket::open(interface, atdecc::ETHERTYPE_AVTP).map_err(|error| error.to_string())?;
    socket
        .join_multicast(ADP_ACMP_MULTICAST)
        .map_err(|error| error.to_string())?;
    let _ = socket.keep_payloads_starting(atdecc::avtp::subtype::ADP..=atdecc::avtp::subtype::MAAP);
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed));
    }

    let started = std::time::Instant::now();
    let clock = || Instant::from_nanos(started.elapsed().as_nanos() as u64);
    let controller_id = avb_endpointd::entity_id(mac, CONTROLLER_INSTANCE);
    let mut config = Config::new(controller_id);
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
    let mut controller = Controller::new(config);

    let mut boards: Vec<Board> = options
        .entities
        .iter()
        .map(|text| Board {
            wanted: Wanted::of(text),
            entity_id: None,
            mac: MacAddress([0; 6]),
            name: String::new(),
            tail: None,
            out: None,
            sent: BTreeMap::new(),
            last_round_trip: Duration::ZERO,
            line: Vec::new(),
            skip_partial: false,
            reading_since_ms: None,
            stats: Stats::default(),
        })
        .collect();
    let by_name = boards
        .iter()
        .any(|board| matches!(board.wanted, Wanted::Name(_)));
    if by_name {
        controller.discover(None);
    } else {
        for board in &boards {
            if let Wanted::Id(id) = board.wanted {
                controller.discover(Some(id));
            }
        }
    }
    let prefix = boards.len() > 1;
    let find_until = clock() + FIND_FOR;
    let mut next_stats = clock() + STATS_EVERY;
    let mut buffer = vec![0u8; 1600];
    let mut stdout = std::io::stdout().lock();

    loop {
        let now = clock();
        if stop.load(Ordering::Relaxed) {
            break;
        }
        controller.handle_timeout(now);
        for board in boards.iter_mut() {
            if board.out.is_some() {
                continue;
            }
            let (Some(id), Some(tail)) = (board.entity_id, board.tail.as_mut()) else {
                continue;
            };
            if let Some(reads) = tail.poll_read(now) {
                board.out = Some(controller.read_memory(now, id, &reads));
                board.stats.reads += 1;
            }
        }
        while let Ok(Some(transmit)) = controller.poll_transmit(&mut buffer) {
            let frame = &buffer[..transmit.length];
            let _ = socket.send(transmit.destination, frame);
            if let Some(board) = boards
                .iter_mut()
                .find(|board| board.entity_id.is_some() && board.mac == transmit.destination)
                && let Some(sequence) = address_access(frame, controller_id, false)
            {
                board.stats.frames_out += 1;
                board.stats.octets_out += wire_octets(frame.len());
                board.sent.insert(sequence, now);
            }
        }
        while let Some(event) = controller.poll_event() {
            handle_event(
                &mut controller,
                &mut boards,
                event,
                now,
                options,
                &mut stdout,
            )?;
        }
        let _ = stdout.flush();

        let finding = boards.iter().any(|board| board.tail.is_none());
        if finding && now >= find_until {
            let missing: Vec<&str> = boards
                .iter()
                .zip(&options.entities)
                .filter(|(board, _)| board.tail.is_none())
                .map(|(_, text)| text.as_str())
                .collect();
            return Err(format!(
                "no device {} found on {interface} with a log",
                missing.join(", ")
            ));
        }
        if !options.follow
            && !finding
            && boards
                .iter()
                .all(|board| board.tail.as_ref().is_some_and(LogTail::caught_up))
        {
            break;
        }
        if now >= next_stats {
            next_stats = now + STATS_EVERY;
            if options.stats {
                for board in &boards {
                    eprintln!("{}", stats_line(board, started.elapsed()));
                }
            }
            if let Some(path) = &options.stats_json {
                write_json(path, &boards, started.elapsed());
            }
        }

        let wake = [
            controller.poll_timeout(),
            Some(next_stats),
            finding.then_some(find_until),
        ]
        .into_iter()
        .chain(boards.iter().map(|board| {
            board
                .tail
                .as_ref()
                .filter(|_| board.out.is_none())
                .and_then(LogTail::poll_timeout)
        }))
        .flatten()
        .min();
        let wait = wake
            .map_or(Duration::from_millis(200), |wake| {
                wake.saturating_duration_since(now)
            })
            .min(Duration::from_millis(200));
        match socket.receive(&mut buffer, Some(wait)) {
            Ok(Some(received)) => {
                let now = clock();
                let frame = &buffer[..received.length];
                if let Some(board) = boards
                    .iter_mut()
                    .find(|board| board.entity_id.is_some() && board.mac == received.source)
                    && let Some(sequence) = address_access(frame, controller_id, true)
                {
                    board.stats.frames_in += 1;
                    board.stats.octets_in += wire_octets(frame.len());
                    if let Some(sent) = board.sent.remove(&sequence) {
                        let round_trip = now.saturating_duration_since(sent);
                        board.last_round_trip = round_trip;
                        board.stats.round_trips.push(round_trip);
                    }
                }
                let _ = controller.handle_frame(now, received.source, frame);
            }
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    for board in &mut boards {
        flush_line(board, prefix, &mut stdout);
    }
    let _ = stdout.flush();
    if options.stats {
        for board in &boards {
            eprintln!("{}", stats_line(board, started.elapsed()));
        }
    }
    if let Some(path) = &options.stats_json {
        write_json(path, &boards, started.elapsed());
    }
    Ok(())
}

fn handle_event(
    controller: &mut Controller,
    boards: &mut [Board],
    event: Event,
    now: Instant,
    options: &Options,
    out: &mut impl Write,
) -> Result<(), String> {
    let prefix = boards.len() > 1;
    match event {
        Event::EntityOnline(id) => {
            let Some(entity) = controller.entity(id) else {
                return Ok(());
            };
            let mac = entity.mac;
            let address_access = entity
                .adp
                .entity_capabilities
                .contains(EntityCapabilities::ADDRESS_ACCESS_SUPPORTED);
            let mut read = false;
            for board in boards.iter_mut() {
                match &board.wanted {
                    Wanted::Id(wanted) if *wanted == id => read = true,
                    Wanted::Name(_) if board.entity_id.is_none() && address_access => read = true,
                    _ => {}
                }
                if board.entity_id == Some(id) {
                    board.mac = mac;
                    if let Some(tail) = &mut board.tail {
                        tail.resume(now);
                        let _ = writeln!(out, "[{} is back]", board.label());
                    }
                }
            }
            if read && controller.model(id).is_none() {
                controller.enumerate(now, id);
            }
        }
        Event::EntityOffline(id, _) => {
            for board in boards
                .iter_mut()
                .filter(|board| board.entity_id == Some(id))
            {
                if let Some(tail) = &mut board.tail {
                    tail.pause();
                    board.out = None;
                    let _ = writeln!(out, "[{} went away]", board.label());
                }
            }
        }
        Event::EntityEnumerated(id) => {
            let Some(model) = controller.model(id) else {
                return Ok(());
            };
            let name = model.entity_name().unwrap_or_default().to_owned();
            let log = model
                .memory_objects()
                .find(|object| object.memory_object_type == MemoryObjectType::LOG_OBJECT)
                .map(|object| object.start_address);
            let mac = controller.entity(id).map(|entity| entity.mac);
            let taken: Vec<EntityId> = boards.iter().filter_map(|board| board.entity_id).collect();
            for board in boards.iter_mut().filter(|board| board.entity_id.is_none()) {
                let wanted = match &board.wanted {
                    Wanted::Id(wanted) => *wanted == id,
                    Wanted::Name(wanted) => *wanted == name && !taken.contains(&id),
                };
                if !wanted {
                    continue;
                }
                let Some(base) = log else {
                    return Err(format!(
                        "{} keeps no log (no LOG_OBJECT memory object)",
                        label_of(&name, id)
                    ));
                };
                board.entity_id = Some(id);
                board.mac = mac.unwrap_or(board.mac);
                board.name = name.clone();
                let mut tail = LogTail::new(base, now);
                if let Some(poll) = options.poll {
                    tail.set_intervals(poll, poll);
                    tail.set_always_read(true);
                }
                board.tail = Some(tail);
                break;
            }
        }
        Event::CommandFinished(command, outcome) => {
            let Some(board) = boards.iter_mut().find(|board| board.out == Some(command)) else {
                return Ok(());
            };
            board.out = None;
            let Some(tail) = &mut board.tail else {
                return Ok(());
            };
            let answer = (outcome == Outcome::Done)
                .then(|| controller.take_memory_read(command))
                .flatten();
            match answer {
                Some(answer) if answer.status == AaStatus::SUCCESS => {
                    let tlvs: Vec<(u64, &[u8])> = answer
                        .tlvs
                        .iter()
                        .map(|(address, octets)| (*address, &octets[..]))
                        .collect();
                    let mut pieces = Vec::new();
                    tail.handle_read(now, &tlvs, &mut pieces);
                    for piece in pieces {
                        print_piece(board, piece, prefix, out);
                    }
                }
                Some(_) => {
                    board.stats.refused += 1;
                    tail.handle_failure(now);
                }
                None => {
                    if outcome == Outcome::NoResponse {
                        board.stats.timeouts += 1;
                    }
                    tail.handle_failure(now);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn label_of(name: &str, id: EntityId) -> String {
    if name.is_empty() {
        format!("0x{:016x}", id.0)
    } else {
        name.to_owned()
    }
}

/// The sequence ID of an Address Access command (or response) between
/// this controller and a device.
fn address_access(frame: &[u8], controller: EntityId, response: bool) -> Option<u16> {
    let (header, _) = AecpHeader::decode(frame).ok()?;
    let wanted = if response {
        AecpMessageType::ADDRESS_ACCESS_RESPONSE
    } else {
        AecpMessageType::ADDRESS_ACCESS_COMMAND
    };
    (header.message_type == wanted && header.controller_entity_id == controller)
        .then(|| AddressAccessPdu::decode(frame).ok())
        .flatten()
        .map(|pdu| pdu.header.sequence_id)
}

/// Octets a frame of this payload takes on the wire: the Ethernet header
/// and the payload, padded to the least frame, without preamble or FCS.
fn wire_octets(payload: usize) -> u64 {
    (14 + payload).max(60) as u64
}

fn print_piece(board: &mut Board, piece: LogPiece, prefix: bool, out: &mut impl Write) {
    let label = board.label();
    match piece {
        LogPiece::Text {
            octets,
            board_ms,
            after_gap,
        } => {
            board.stats.log_octets += octets.len() as u64;
            let since = *board.reading_since_ms.get_or_insert(board_ms);
            if after_gap {
                board.line.clear();
                board.skip_partial = true;
            }
            for &octet in &octets {
                if board.skip_partial {
                    board.skip_partial = octet != b'\n';
                    continue;
                }
                board.line.push(octet);
                if octet == b'\n' {
                    board.stats.lines += 1;
                    if let Some(logged) = line_ms(&board.line)
                        && logged >= since
                        && let Some(age) = board_ms.checked_sub(logged)
                    {
                        let latency = age + (board.last_round_trip / 2).as_millis() as u32;
                        board.stats.latencies_ms.push(latency);
                    }
                    let text = String::from_utf8_lossy(&board.line);
                    let text = text.trim_end_matches(['\n', '\r']);
                    let _ = if prefix {
                        writeln!(out, "{label}: {text}")
                    } else {
                        writeln!(out, "{text}")
                    };
                    board.line.clear();
                }
            }
        }
        LogPiece::Lost(octets) => {
            board.stats.lost += octets;
            let _ = writeln!(
                out,
                "[{label}: {octets} bytes lost, written faster than read]"
            );
        }
        LogPiece::Restarted => {
            board.stats.restarts += 1;
            board.line.clear();
            board.skip_partial = false;
            board.reading_since_ms = None;
            let _ = writeln!(out, "[{label} restarted]");
        }
        LogPiece::Dropped(lines) => {
            board.stats.dropped_lines += u64::from(lines);
            let _ = writeln!(out, "[{label} dropped {lines} lines]");
        }
    }
}

/// Prints a line left without its newline at the end.
fn flush_line(board: &mut Board, prefix: bool, out: &mut impl Write) {
    if board.line.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(&board.line).into_owned();
    let _ = if prefix {
        writeln!(out, "{}: {text}", board.label())
    } else {
        writeln!(out, "{text}")
    };
    board.line.clear();
}

/// A line's milliseconds since boot: ESP-IDF's `I (12345) tag: text`,
/// the first parenthesized number.
fn line_ms(line: &[u8]) -> Option<u32> {
    let open = line.iter().position(|&octet| octet == b'(')?;
    let rest = &line[open + 1..];
    let close = rest.iter().position(|&octet| octet == b')')?;
    std::str::from_utf8(&rest[..close]).ok()?.parse().ok()
}

fn percentile(sorted: &[u32], share: f64) -> Option<u32> {
    let last = sorted.len().checked_sub(1)?;
    Some(sorted[((last as f64) * share).round() as usize])
}

fn stats_line(board: &Board, elapsed: Duration) -> String {
    let stats = &board.stats;
    let seconds = elapsed.as_secs_f64().max(0.001);
    let round_trips = &stats.round_trips;
    let rtt_avg = if round_trips.is_empty() {
        0.0
    } else {
        round_trips.iter().sum::<Duration>().as_secs_f64() * 1000.0 / round_trips.len() as f64
    };
    let rtt_max = round_trips
        .iter()
        .max()
        .copied()
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0;
    let mut latencies = stats.latencies_ms.clone();
    latencies.sort_unstable();
    let latency = match (
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.95),
        latencies.last(),
    ) {
        (Some(median), Some(p95), Some(max)) => {
            format!("latency {median}/{p95}/{max} ms (median/p95/max)")
        }
        _ => "latency n/a".into(),
    };
    format!(
        "{}: {:.0} s, {:.2} reads/s, out {:.2} frames/s {:.0} B/s, in {:.2} frames/s {:.0} B/s, \
         rtt {rtt_avg:.2}/{rtt_max:.2} ms (avg/max), {latency}, {} retries, {} timeouts, \
         {} refused, {} lines, {} B read, {} B lost, {} lines dropped, {} restarts",
        board.label(),
        seconds,
        stats.reads as f64 / seconds,
        stats.frames_out as f64 / seconds,
        stats.octets_out as f64 / seconds,
        stats.frames_in as f64 / seconds,
        stats.octets_in as f64 / seconds,
        stats.retries(),
        stats.timeouts,
        stats.refused,
        stats.lines,
        stats.log_octets,
        stats.lost,
        stats.dropped_lines,
        stats.restarts,
    )
}

fn write_json(path: &Path, boards: &[Board], elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    let entries: Vec<String> = boards
        .iter()
        .map(|board| {
            let stats = &board.stats;
            let mut latencies = stats.latencies_ms.clone();
            latencies.sort_unstable();
            let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
            let rtt_avg = if stats.round_trips.is_empty() {
                0.0
            } else {
                ms(stats.round_trips.iter().sum::<Duration>()) / stats.round_trips.len() as f64
            };
            let rtt_max = ms(stats.round_trips.iter().max().copied().unwrap_or_default());
            let number =
                |value: Option<u32>| value.map_or("null".into(), |value| value.to_string());
            format!(
                "{{\"entity\":\"{}\",\"name\":\"{}\",\"seconds\":{seconds:.1},\"reads\":{},\
                 \"frames_out\":{},\"octets_out\":{},\"frames_in\":{},\"octets_in\":{},\
                 \"retries\":{},\"timeouts\":{},\"refused\":{},\"rtt_ms_avg\":{rtt_avg:.3},\
                 \"rtt_ms_max\":{rtt_max:.3},\"lines\":{},\"log_octets\":{},\"lost_octets\":{},\
                 \"dropped_lines\":{},\"restarts\":{},\"latency_ms_median\":{},\
                 \"latency_ms_p95\":{},\"latency_ms_max\":{}}}",
                board
                    .entity_id
                    .map_or_else(String::new, |id| format!("0x{:016x}", id.0)),
                json_text(&board.name),
                stats.reads,
                stats.frames_out,
                stats.octets_out,
                stats.frames_in,
                stats.octets_in,
                stats.retries(),
                stats.timeouts,
                stats.refused,
                stats.lines,
                stats.log_octets,
                stats.lost,
                stats.dropped_lines,
                stats.restarts,
                number(percentile(&latencies, 0.5)),
                number(percentile(&latencies, 0.95)),
                number(latencies.last().copied()),
            )
        })
        .collect();
    let json = format!("{{\"boards\":[{}]}}\n", entries.join(","));
    let temporary = path.with_extension("json.part");
    if std::fs::write(&temporary, json).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

fn json_text(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)))
            }
            other => out.push(other),
        }
    }
    out
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
    }

    #[test]
    fn options_parse() {
        let words: Vec<String> = [
            "esp-mic",
            "--follow",
            "--stats",
            "--poll",
            "10",
            "esp-synth",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        let options = parse(&words).unwrap();
        assert_eq!(options.entities, ["esp-mic", "esp-synth"]);
        assert!(options.follow && options.stats);
        assert_eq!(options.poll, Some(Duration::from_millis(10)));
        assert!(parse(&[]).is_err());
        assert!(parse(&["x".into(), "--poll".into(), "0".into()]).is_err());
        assert!(parse(&["x".into(), "--nope".into()]).is_err());
    }

    #[test]
    fn a_lines_milliseconds_are_its_first_parenthesized_number() {
        assert_eq!(line_ms(b"I (12345) avb: hello (7)\n"), Some(12345));
        assert_eq!(line_ms(b"no time here\n"), None);
        assert_eq!(line_ms(b"W (abc) x\n"), None);
    }

    #[test]
    fn a_line_after_a_gap_starts_at_the_next_newline() {
        let mut board = Board {
            wanted: Wanted::Name("b".into()),
            entity_id: None,
            mac: MacAddress([0; 6]),
            name: "b".into(),
            tail: None,
            out: None,
            sent: BTreeMap::new(),
            last_round_trip: Duration::from_millis(2),
            line: Vec::new(),
            skip_partial: false,
            reading_since_ms: Some(90),
            stats: Stats::default(),
        };
        let mut out = Vec::new();
        print_piece(
            &mut board,
            LogPiece::Text {
                octets: b"tail of a line\nI (100) a: whole\nI (1".to_vec(),
                board_ms: 150,
                after_gap: true,
            },
            false,
            &mut out,
        );
        print_piece(
            &mut board,
            LogPiece::Text {
                octets: b"20) a: split\nI (80) a: waiting\n".to_vec(),
                board_ms: 160,
                after_gap: false,
            },
            false,
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "I (100) a: whole\nI (120) a: split\nI (80) a: waiting\n"
        );
        // 150 - 100 and 160 - 120, each plus half of a 2 ms round trip;
        // none for the line logged before reading began.
        assert_eq!(board.stats.latencies_ms, [51, 41]);
        assert_eq!(board.stats.lines, 3);
    }
}
