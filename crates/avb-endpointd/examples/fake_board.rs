//! A stand-in for an ESP32 board in a namespace's tests, never on a
//! network: an entity with no streams that keeps a log as the boards do,
//! a LOG_OBJECT memory object read through Address Access (see
//! `atdecc::log`), so `avb-endpointd log` can be tried without one. It
//! writes lines as ESP-IDF prints them, `I (ms) tag: text`.
//!
//!   fake_board --interface NAME [--name NAME] [--instance N] [--ring OCTETS]
//!              [--rate LINES_PER_SECOND] [--line OCTETS] [--burst OCTETS]
//!              [--restart-after SECONDS] [--drop-every N] [--for SECONDS]
//!              [--crash-dump OCTETS] [--crash-max OCTETS] [--crash-file PATH]
//!              [--firmware-max OCTETS] [--version TEXT] [--streams-running]
//!              [--reject-images] [--rollback-after SECONDS]
//!   fake_board --make-image FILE [--version TEXT] [--size OCTETS]
//!              [--project NAME] [--signed KEYS]
//!
//! `--burst` writes that many octets of lines at once at the start, as a
//! boot does. `--restart-after` starts the log over in a new boot that
//! long after the start, once. `--drop-every` counts every Nth line as
//! dropped instead of writing it. It writes each line on stdout too, to
//! compare with what was read.
//!
//! `--crash-dump` keeps a crash dump of that many octets, as a board's
//! CRASH_DUMP memory object, which START_OPERATION erases; `--crash-max`
//! alone keeps an empty one, up to that size (256 KiB by default).
//! `--crash-file` writes the dump's octets there, to compare with what
//! was read.
//!
//! `--firmware-max` takes firmware images of up to that many octets, as
//! IEEE 1722.1-2021 Annex D uploads them: a FIRMWARE_IMAGE memory object.
//! An image is kept when it is an ESP-IDF application image of its
//! project, `fake_board`, unless `--reject-images`; the board then
//! restarts in it, its firmware_version `<version> (fake - <sha9>)` as an
//! ESP32 board's, `--version` the first one's. `--streams-running` has it
//! refuse an upload unless locked, and `--rollback-after` has a new image
//! restart in the old that long after it started. `--make-image` writes
//! such an image, `--signed` with that many signature blocks after it.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use atdecc::entity::{ClockQuality, EndpointModel, Entity};
use atdecc::{ADP_ACMP_MULTICAST, ClockIdentity, EntityModelId, Instant};
use avb_net::Socket;

struct Options {
    interface: String,
    name: String,
    instance: u8,
    ring: u32,
    rate: f64,
    line: usize,
    burst: usize,
    restart_after: Option<Duration>,
    drop_every: Option<u64>,
    run_for: Option<Duration>,
    crash_dump: Option<usize>,
    crash_max: Option<u64>,
    crash_file: Option<String>,
    firmware_max: Option<u64>,
    version: String,
    streams_running: bool,
    reject_images: bool,
    rollback_after: Option<Duration>,
    make_image: Option<String>,
    size: usize,
    project: String,
    signed: usize,
}

const USAGE: &str = "usage: fake_board --interface NAME [--name NAME] [--instance N] \
                     [--ring OCTETS] [--rate LINES_PER_SECOND] [--line OCTETS] \
                     [--burst OCTETS] [--restart-after SECONDS] [--drop-every N] \
                     [--for SECONDS] [--crash-dump OCTETS] [--crash-max OCTETS] \
                     [--crash-file PATH] [--firmware-max OCTETS] [--version TEXT] \
                     [--streams-running] [--reject-images] [--rollback-after SECONDS]\n\
                     \x20      fake_board --make-image FILE [--version TEXT] [--size OCTETS] \
                     [--project NAME] [--signed KEYS]";

/// The project its images are of.
const PROJECT: &str = "fake_board";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        interface: String::new(),
        name: "fake-board".into(),
        instance: 0x30,
        ring: 32 * 1024,
        rate: 2.0,
        line: 60,
        burst: 0,
        restart_after: None,
        drop_every: None,
        run_for: None,
        crash_dump: None,
        crash_max: None,
        crash_file: None,
        firmware_max: None,
        version: "0.0.1".into(),
        streams_running: false,
        reject_images: false,
        rollback_after: None,
        make_image: None,
        size: 65536,
        project: PROJECT.into(),
        signed: 0,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        let number = |text: &String| {
            text.parse::<f64>()
                .map_err(|_| format!("{arg}: not a number"))
        };
        match arg.as_str() {
            "--interface" => options.interface = value()?.clone(),
            "--name" => options.name = value()?.clone(),
            "--instance" => options.instance = number(value()?)? as u8,
            "--ring" => options.ring = number(value()?)? as u32,
            "--rate" => options.rate = number(value()?)?,
            "--line" => options.line = number(value()?)? as usize,
            "--burst" => options.burst = number(value()?)? as usize,
            "--restart-after" => {
                options.restart_after = Some(Duration::from_secs_f64(number(value()?)?));
            }
            "--drop-every" => options.drop_every = Some(number(value()?)? as u64),
            "--for" => options.run_for = Some(Duration::from_secs_f64(number(value()?)?)),
            "--crash-dump" => options.crash_dump = Some(number(value()?)? as usize),
            "--crash-max" => options.crash_max = Some(number(value()?)? as u64),
            "--crash-file" => options.crash_file = Some(value()?.clone()),
            "--firmware-max" => options.firmware_max = Some(number(value()?)? as u64),
            "--version" => options.version = value()?.clone(),
            "--streams-running" => options.streams_running = true,
            "--reject-images" => options.reject_images = true,
            "--rollback-after" => {
                options.rollback_after = Some(Duration::from_secs_f64(number(value()?)?));
            }
            "--make-image" => options.make_image = Some(value()?.clone()),
            "--size" => options.size = number(value()?)? as usize,
            "--project" => options.project = value()?.clone(),
            "--signed" => options.signed = number(value()?)? as usize,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if options.interface.is_empty() && options.make_image.is_none() {
        return Err("--interface is needed".into());
    }
    Ok(options)
}

fn model(options: &Options, mac: atdecc::MacAddress, firmware_version: &str) -> EndpointModel {
    let entity_id = avb_endpointd::entity_id(mac, options.instance);
    let [a, b, c, d, e, f] = mac.0;
    EndpointModel {
        entity_id,
        entity_model_id: EntityModelId(0x8c1f_6436_c030_0002),
        entity_name: options.name.clone(),
        group_name: String::new(),
        vendor_name: "Scramble Tools".into(),
        model_name: "Fake board".into(),
        firmware_version: firmware_version.into(),
        serial_number: String::new(),
        mac,
        interface_name: options.interface.clone(),
        clock_identity: ClockIdentity(u64::from_be_bytes([a, b, c, 0xff, 0xfe, d, e, f])),
        clock_quality: ClockQuality::grandmaster_capable(248),
        sampling_rates: vec![48_000],
        current_sampling_rate: 48_000,
        outputs: Vec::new(),
        inputs: Vec::new(),
        clock_source: 0,
        clock_source_inputs: Vec::new(),
        logo: None,
        vendor_logo: None,
    }
}

/// A line of `length` octets, newline included, logged at `ms`.
fn line(ms: u64, number: u64, length: usize) -> String {
    let mut text = format!("I ({ms}) fake: line {number} ");
    while text.len() + 1 < length {
        text.push(char::from(b'a' + (text.len() % 26) as u8));
    }
    text.push('\n');
    text
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Some(path) = &options.make_image {
        let image = make_image(
            &options.version,
            &options.project,
            options.size,
            options.signed,
        );
        if let Err(error) = std::fs::write(path, image) {
            eprintln!("fake_board: {path}: {error}");
            std::process::exit(1);
        }
        return;
    }
    let _ = avb_net::broker::use_helper("avb-netraw", &[Path::new("/usr/lib/avb-endpointd")]);
    let Some(mac) = avb_net::interfaces()
        .into_iter()
        .find(|found| found.name == options.interface)
        .map(|found| found.mac)
    else {
        eprintln!("no interface named {}", options.interface);
        std::process::exit(1);
    };
    if let Err(error) = run(&options, mac) {
        eprintln!("fake_board: {error}");
        std::process::exit(1);
    }
}

fn run(options: &Options, mac: atdecc::MacAddress) -> std::io::Result<()> {
    let socket = Socket::open(&options.interface, atdecc::ETHERTYPE_AVTP)?;
    socket.join_multicast(ADP_ACMP_MULTICAST)?;
    let started = std::time::Instant::now();
    let clock = || Instant::from_nanos(started.elapsed().as_nanos() as u64);
    let mut boot = 1;
    // Lines count milliseconds from their boot, as a board's do.
    let mut booted = clock();
    let mut dump = (options.crash_dump.is_some() || options.crash_max.is_some())
        .then(|| crash_dump(options.crash_dump.unwrap_or(0)));
    if let (Some(dump), Some(path)) = (&dump, &options.crash_file) {
        std::fs::write(path, dump)?;
    }
    let mut running = format!(
        "{} (fake - {})",
        options.version,
        sha9(&elf_sha256(&options.version, &options.project, 0))
    );
    let mut entity = board(options, mac, &running, booted, boot, dump.clone());
    // A new image kept and its boot due, and the image it replaced with
    // when to go back to it.
    let mut staged: Option<String> = None;
    let mut reboot_at: Option<Instant> = None;
    let mut rollback: Option<(String, Instant)> = None;
    let mut stdout = std::io::stdout().lock();
    let mut number = 0;
    let mut write_line = |entity: &mut Entity, number: &mut u64, booted: Instant, now: Instant| {
        let ms = now.saturating_duration_since(booted).as_millis() as u64;
        *number += 1;
        if options
            .drop_every
            .is_some_and(|every| every > 0 && number.is_multiple_of(every))
        {
            entity.log_dropped_line();
            return;
        }
        let text = line(ms, *number, options.line);
        entity.log(text.as_bytes());
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    };
    // A boot's burst: lines enough for the octets asked, at once.
    while (number as usize) * options.line < options.burst {
        write_line(&mut entity, &mut number, booted, clock());
    }
    let every = Duration::from_secs_f64(1.0 / options.rate.max(0.001));
    let mut next_line = clock() + every;
    let mut restarted = false;
    let mut buffer = vec![0u8; 1600];
    loop {
        let now = clock();
        if options
            .run_for
            .is_some_and(|run_for| now.since_start() >= run_for)
        {
            entity.depart();
            while let Some((destination, frame)) = entity.poll_transmit() {
                let _ = socket.send(destination, &frame);
            }
            return Ok(());
        }
        if !restarted
            && let Some(after) = options.restart_after
            && now.since_start() >= after
        {
            restarted = true;
            boot += 1;
            // A new boot: a fresh log, from its own zero.
            entity.keep_log(now, options.ring, boot);
            booted = now;
            number = 0;
        }
        if let Some(stored) = entity.take_stored_firmware() {
            let version = image_version(&stored.image).filter(|_| !options.reject_images);
            entity.finish_firmware(version.is_some());
            if let Some(version) = version {
                staged = Some(version);
                if stored.reboot {
                    reboot_at = Some(now + Duration::from_millis(300));
                }
            }
        }
        let back = rollback
            .as_ref()
            .filter(|(_, at)| now >= *at)
            .map(|(old, _)| old.clone());
        if reboot_at.is_some_and(|at| now >= at) || back.is_some() {
            entity.depart();
            while let Some((destination, frame)) = entity.poll_transmit() {
                let _ = socket.send(destination, &frame);
            }
            dump = entity.crash_dump().map(<[u8]>::to_vec);
            if let Some(old) = back {
                running = old;
                rollback = None;
            } else if let Some(new) = staged.take() {
                if let Some(after) = options.rollback_after {
                    rollback = Some((running.clone(), now + after));
                }
                running = new;
            }
            reboot_at = None;
            boot += 1;
            booted = now;
            number = 0;
            entity = board(options, mac, &running, now, boot, dump.clone());
        }
        entity.handle_timeout(now);
        if now >= next_line {
            next_line += every;
            if options.rate > 0.0 {
                write_line(&mut entity, &mut number, booted, now);
            }
        }
        while let Some((destination, frame)) = entity.poll_transmit() {
            let _ = socket.send(destination, &frame);
        }
        let wake = [entity.poll_timeout(), Some(next_line)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + Duration::from_millis(100));
        let wait = wake
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        if let Ok(Some(received)) = socket.receive(&mut buffer, Some(wait)) {
            entity.handle_frame(clock(), received.source, &buffer[..received.length]);
        }
    }
}

/// A stand-in for a core dump of `length` octets: a name, then a pattern
/// that shows any octet out of place.
fn crash_dump(length: usize) -> Vec<u8> {
    let mut dump = b"FAKE CORE DUMP\n".to_vec();
    dump.extend((0..length).map(|at| (at.wrapping_mul(31) ^ (at >> 8)) as u8));
    dump.truncate(length);
    dump
}

/// The board in one boot: its log from `now`, its crash dump, and the
/// firmware it takes.
fn board(
    options: &Options,
    mac: atdecc::MacAddress,
    version: &str,
    now: Instant,
    boot: u32,
    dump: Option<Vec<u8>>,
) -> Entity {
    let mut entity = Entity::new(model(options, mac, version), &[]);
    entity.keep_log(now, options.ring, boot);
    if let Some(dump) = dump {
        entity.keep_crash_dump(options.crash_max.unwrap_or(256 * 1024), dump);
    }
    if let Some(maximum) = options.firmware_max {
        entity.keep_firmware(maximum);
        entity.set_streams_running(options.streams_running);
    }
    entity.start(now);
    entity
}

/// A stand-in for an ELF's SHA-256: FNV-1a of what tells images apart,
/// four ways.
fn elf_sha256(version: &str, project: &str, size: usize) -> [u8; 32] {
    let mut sha = [0u8; 32];
    for (part, chunk) in sha.chunks_mut(8).enumerate() {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ part as u64;
        for octet in version
            .bytes()
            .chain(project.bytes())
            .chain(size.to_le_bytes())
        {
            hash = (hash ^ u64::from(octet)).wrapping_mul(0x0100_0000_01b3);
        }
        chunk.copy_from_slice(&hash.to_be_bytes());
    }
    sha
}

fn sha9(sha: &[u8; 32]) -> String {
    let hex: String = sha.iter().map(|octet| format!("{octet:02x}")).collect();
    hex[..9].to_owned()
}

/// An ESP-IDF application image's layout: the 0xE9 header, the chip
/// (ESP32-P4) at 12, esp_app_desc_t at 32 with its version, project and
/// ELF SHA-256, then `signed` signature blocks in a 4 KiB sector after.
fn make_image(version: &str, project: &str, size: usize, signed: usize) -> Vec<u8> {
    let size = size.max(256);
    let mut image: Vec<u8> = (0..size).map(|at| (at % 251) as u8).collect();
    image[..32].fill(0);
    image[0] = 0xe9;
    image[12..14].copy_from_slice(&18u16.to_le_bytes());
    image[32..208].fill(0);
    image[32..36].copy_from_slice(&0xabcd_5432u32.to_le_bytes());
    let field = |image: &mut Vec<u8>, at: usize, text: &str| {
        let text = &text.as_bytes()[..text.len().min(31)];
        image[at..at + text.len()].copy_from_slice(text);
    };
    field(&mut image, 48, version);
    field(&mut image, 80, project);
    image[176..208].copy_from_slice(&elf_sha256(version, project, size));
    if signed > 0 {
        image.resize(image.len().next_multiple_of(4096), 0xff);
        let sector = image.len();
        image.resize(sector + 4096, 0xff);
        for block in 0..signed.min(3) {
            image[sector + block * 1216] = 0xe7;
            image[sector + block * 1216 + 1] = 0x02;
        }
    }
    image
}

/// The firmware_version an image of its project runs as, if it is one.
fn image_version(image: &[u8]) -> Option<String> {
    if image.len() < 208 || image[0] != 0xe9 {
        return None;
    }
    if image[32..36] != 0xabcd_5432u32.to_le_bytes() {
        return None;
    }
    let text = |at: usize| {
        let field = &image[at..at + 32];
        let end = field.iter().position(|&octet| octet == 0).unwrap_or(32);
        String::from_utf8_lossy(&field[..end]).into_owned()
    };
    if text(80) != PROJECT {
        return None;
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&image[176..208]);
    Some(format!("{} (fake - {})", text(48), sha9(&sha)))
}
