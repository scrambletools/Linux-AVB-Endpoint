//! `avb-endpointd firmware`: uploads a firmware image to a device as IEEE
//! 1722.1-2021 Annex D has it, as Hive does. The image goes to the
//! device's FIRMWARE_IMAGE memory object: START_OPERATION's UPLOAD with its
//! length, Address Access WRITEs of its octets in order, one at a time, and
//! START_OPERATION's STORE_AND_REBOOT, or STORE with `--no-reboot`. The
//! device checks the image, says so in OPERATION_STATUS and restarts in
//! it. This then reads the device's firmware_version again and says
//! whether it runs the new image, or rolled back to the old: an ESP32
//! board keeps a new image only once it is on the network again, and
//! otherwise restarts in the old within a minute.
//!
//! The image is an ESP-IDF application image, as idf.py builds it. Its
//! header names the chip and its esp_app_desc_t the project, version and
//! ELF SHA-256, whose first nine hex digits the board's firmware_version
//! ends with. A signed image ends in a 4 KiB sector of Secure Boot V2
//! signature blocks; an image without one is uploaded with a warning, as
//! a board that checks signatures refuses it.
//!
//! A device whose streams run refuses an upload (STREAM_IS_RUNNING).
//! `--stop-streams` locks it first (LOCK_ENTITY), which lets it stop
//! them for the upload without saving that; the restart, or the lock's
//! end, starts them again.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use atdecc::aecp::{AaStatus, AemStatus};
use atdecc::controller::{Event, Outcome, Refusal};
use atdecc::descriptor::{DescriptorType, MemoryObjectOperation, MemoryObjectType};
use atdecc::entity::MEMORY_SEGMENT;
use atdecc::{EntityId, Instant};
use avb_endpointd::status::EXIT_USAGE;

use crate::tool_net::{self, Link, Wanted};

/// How long to look for the device asked for.
const FIND_FOR: Duration = Duration::from_secs(10);
/// How long a command may take, its retry included.
const ANSWER_WITHIN: Duration = Duration::from_secs(2);
/// Tries for each part of the image.
const TRIES: u32 = 3;
/// How often a lock is taken again, well inside Milan's 60 s.
const LOCK_AGAIN: Duration = Duration::from_secs(20);
/// How long checking and keeping the image may take.
const STORE_WITHIN: Duration = Duration::from_secs(60);
/// How long the device may take to come back after its restart.
const BACK_WITHIN: Duration = Duration::from_secs(120);
/// How long to watch a device back in its new image for a restart in the
/// old: an ESP32 board keeps a new image within 60 s.
const SETTLE: Duration = Duration::from_secs(90);

struct Options {
    entity: String,
    image: PathBuf,
    stop_streams: bool,
    no_reboot: bool,
    settle: Duration,
}

fn parse(words: &[String]) -> Result<Options, String> {
    let mut positional = Vec::new();
    let mut stop_streams = false;
    let mut no_reboot = false;
    let mut settle = SETTLE;
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--stop-streams" => stop_streams = true,
            "--no-reboot" => no_reboot = true,
            "--settle" => {
                let seconds: u64 = words
                    .next()
                    .and_then(|seconds| seconds.parse().ok())
                    .ok_or("--settle needs seconds")?;
                settle = Duration::from_secs(seconds);
            }
            flag if flag.starts_with('-') => return Err(format!("no option {flag} for firmware")),
            text => positional.push(text.to_owned()),
        }
    }
    let [entity, image] = <[String; 2]>::try_from(positional)
        .map_err(|_| "firmware needs a device and an image file")?;
    Ok(Options {
        entity,
        image: PathBuf::from(image),
        stop_streams,
        no_reboot,
        settle,
    })
}

pub fn run(words: &[String], interface: &str) -> ExitCode {
    let options = match parse(words) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{}", crate::tool::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match firmware(&options, interface) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("firmware: {error}");
            ExitCode::FAILURE
        }
    }
}

/// What an ESP-IDF application image says of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspImage {
    pub chip_id: u16,
    pub version: String,
    pub project: String,
    pub elf_sha256: [u8; 32],
    /// Secure Boot V2 signature blocks in its last 4 KiB.
    pub signatures: usize,
}

impl EspImage {
    /// The first nine hex digits of its ELF's SHA-256, as a board's
    /// firmware_version ends with.
    pub fn elf_sha9(&self) -> String {
        let hex: String = self
            .elf_sha256
            .iter()
            .map(|octet| format!("{octet:02x}"))
            .collect();
        hex[..9].to_owned()
    }

    pub fn chip(&self) -> String {
        let name = match self.chip_id {
            0 => "ESP32",
            2 => "ESP32-S2",
            5 => "ESP32-C3",
            9 => "ESP32-S3",
            12 => "ESP32-C2",
            13 => "ESP32-C6",
            16 => "ESP32-H2",
            18 => "ESP32-P4",
            23 => "ESP32-C5",
            _ => return format!("chip {}", self.chip_id),
        };
        name.to_owned()
    }
}

/// An ESP-IDF application image's header and app description: magic 0xE9
/// first, the chip ID at 12, esp_app_desc_t at 32.
pub fn parse_image(image: &[u8]) -> Result<EspImage, String> {
    const DESC: usize = 32;
    if image.len() < DESC + 208 || image[0] != 0xe9 {
        return Err("not an ESP-IDF application image (no 0xE9 header)".into());
    }
    let word = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap_or([0; 4]));
    if word(DESC) != 0xabcd_5432 {
        return Err("not an ESP-IDF application image (no app description)".into());
    }
    let text = |at: usize, length: usize| {
        let field = &image[at..at + length];
        let end = field.iter().position(|&octet| octet == 0).unwrap_or(length);
        String::from_utf8_lossy(&field[..end]).into_owned()
    };
    let mut elf_sha256 = [0; 32];
    elf_sha256.copy_from_slice(&image[DESC + 144..DESC + 176]);
    let signatures = if image.len().is_multiple_of(4096) {
        let sector = &image[image.len() - 4096..];
        (0..3)
            .take_while(|block| sector[block * 1216..block * 1216 + 2] == [0xe7, 0x02])
            .count()
    } else {
        0
    };
    Ok(EspImage {
        chip_id: u16::from_le_bytes([image[12], image[13]]),
        version: text(DESC + 16, 32),
        project: text(DESC + 48, 32),
        elf_sha256,
        signatures,
    })
}

/// The nine hex digits a board's firmware_version ends with, after " - ".
fn version_sha9(firmware_version: &str) -> Option<String> {
    let (_, rest) = firmware_version.rsplit_once(" - ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    (digits.len() >= 9).then(|| digits[..9].to_lowercase())
}

/// The device's FIRMWARE_IMAGE: its index, where its octets go and how
/// many it takes.
struct Target {
    index: u16,
    start: u64,
    maximum: u64,
}

/// What the upload holds while it goes on: the lock, if taken, and when
/// to take it again.
struct Session<'a> {
    link: &'a mut Link,
    id: EntityId,
    name: String,
    locked: bool,
    lock_at: Instant,
}

impl Session<'_> {
    /// Takes the lock again when due.
    fn keep_lock(&mut self) -> Result<(), String> {
        if !self.locked || self.link.now() < self.lock_at {
            return Ok(());
        }
        self.lock(false)
    }

    fn lock(&mut self, unlock: bool) -> Result<(), String> {
        let now = self.link.now();
        let command = self.link.controller.lock_entity(now, self.id, unlock);
        match self.link.wait(command, ANSWER_WITHIN)? {
            Outcome::Done => {
                self.locked = !unlock;
                self.lock_at = self.link.now() + LOCK_AGAIN;
                Ok(())
            }
            outcome => Err(format!(
                "{} {}: {}",
                if unlock { "unlocking" } else { "locking" },
                self.name,
                describe(outcome)
            )),
        }
    }

    /// Lets the lock go, if taken, after a failure.
    fn release(&mut self) {
        if self.locked {
            let _ = self.lock(true);
        }
    }

    fn operation(
        &mut self,
        target: &Target,
        operation: MemoryObjectOperation,
        values: &[u8],
    ) -> Result<u16, Outcome> {
        let now = self.link.now();
        let command = self.link.controller.start_operation(
            now,
            self.id,
            DescriptorType::MEMORY_OBJECT,
            target.index,
            operation.0,
            values,
        );
        match self.link.wait(command, ANSWER_WITHIN) {
            Ok(Outcome::Done) => self
                .link
                .controller
                .take_operation_id(command)
                .ok_or(Outcome::NoResponse),
            Ok(outcome) => Err(outcome),
            Err(_) => Err(Outcome::NoResponse),
        }
    }

    fn abort(&mut self, target: &Target, upload: u16) {
        let now = self.link.now();
        let command = self.link.controller.abort_operation(
            now,
            self.id,
            DescriptorType::MEMORY_OBJECT,
            target.index,
            upload,
        );
        let _ = self.link.wait(command, ANSWER_WITHIN);
    }
}

fn describe(outcome: Outcome) -> String {
    match outcome {
        Outcome::Done => "done".into(),
        Outcome::Refused(Refusal::Aem(status)) => format!("refused, {status:?}"),
        Outcome::Refused(refusal) => format!("refused, {refusal:?}"),
        Outcome::NoResponse => "no answer".into(),
        Outcome::NotPossible => "not possible".into(),
    }
}

fn firmware(options: &Options, interface: &str) -> Result<(), String> {
    let image = std::fs::read(&options.image)
        .map_err(|error| format!("{}: {error}", options.image.display()))?;
    let described =
        parse_image(&image).map_err(|error| format!("{}: {error}", options.image.display()))?;
    println!(
        "{}: {} {} for {}, {} bytes, ELF {}",
        options.image.display(),
        described.project,
        described.version,
        described.chip(),
        image.len(),
        described.elf_sha9()
    );
    match described.signatures {
        0 => eprintln!(
            "warning: {} is not signed; a device that checks signatures refuses it",
            options.image.display()
        ),
        1 => println!("signed with one key"),
        keys => println!("signed with {keys} keys"),
    }

    let wanted = Wanted::of(&options.entity);
    let mut link = Link::open_registering(interface, true)?;
    let id = link
        .find(&wanted, FIND_FOR)
        .map_err(|error| format!("{error} on {interface}"))?;
    let model = link.controller.model(id).ok_or("its model went away")?;
    let name = match model.entity_name() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => tool_net::describe(&Wanted::Id(id)),
    };
    let before = model
        .entity()
        .map(|entity| entity.firmware_version.to_owned())
        .unwrap_or_default();
    let target = model
        .memory_objects()
        .find(|object| object.memory_object_type == MemoryObjectType::FIRMWARE_IMAGE)
        .map(|object| Target {
            index: object.index,
            start: object.start_address,
            maximum: object.maximum_length,
        })
        .ok_or_else(|| format!("{name} takes no firmware (no FIRMWARE_IMAGE memory object)"))?;
    if image.len() as u64 > target.maximum {
        return Err(format!(
            "the image is {} bytes; {name} takes {} at most",
            image.len(),
            target.maximum
        ));
    }
    println!("{name} runs {before}");
    if version_sha9(&before).as_deref() == Some(&described.elf_sha9()[..]) {
        eprintln!("note: {name} runs this image already");
    }

    let mut session = Session {
        link: &mut link,
        id,
        name: name.clone(),
        locked: false,
        lock_at: Instant::ZERO,
    };
    if options.stop_streams {
        session.lock(false)?;
    }
    let upload = match session.operation(
        &target,
        MemoryObjectOperation::UPLOAD,
        &(image.len() as u64).to_be_bytes(),
    ) {
        Ok(upload) => upload,
        Err(Outcome::Refused(Refusal::Aem(AemStatus::STREAM_IS_RUNNING))) => {
            session.release();
            return Err(format!(
                "{name}'s streams run; --stop-streams lets it stop them for the upload"
            ));
        }
        Err(outcome) => {
            session.release();
            return Err(format!(
                "{name} would not start the upload: {}",
                describe(outcome)
            ));
        }
    };

    if let Err(error) = send(&mut session, &target, &image) {
        session.abort(&target, upload);
        session.release();
        return Err(error);
    }
    let store_type = if options.no_reboot {
        MemoryObjectOperation::STORE
    } else {
        MemoryObjectOperation::STORE_AND_REBOOT
    };
    let store = match session.operation(&target, store_type, &[]) {
        Ok(store) => store,
        Err(outcome) => {
            session.abort(&target, upload);
            session.release();
            return Err(format!(
                "{name} would not store the image: {}",
                describe(outcome)
            ));
        }
    };
    match stored(&mut session, &target, store)? {
        Stored::Kept => {}
        Stored::Refused => {
            session.release();
            return Err(format!(
                "{name} refused the image: it failed {name}'s check (signature, chip or project)"
            ));
        }
        Stored::Restarting => {}
    }
    if options.no_reboot {
        session.release();
        println!("{name}: image stored; it runs from {name}'s next restart");
        return Ok(());
    }
    println!("{name}: image stored; restarting in it");
    drop(session);
    let after = back(&mut link, id)?;
    let new = described.elf_sha9();
    let old = version_sha9(&before);
    let verdict = |running: &str| match version_sha9(running) {
        Some(sha9) if sha9 == new => Verdict::New,
        Some(sha9) if Some(&sha9) == old.as_ref() => Verdict::Old,
        _ => Verdict::Other,
    };
    match verdict(&after) {
        Verdict::New => {}
        Verdict::Old => return Err(format!("{name} rolled back to {after}")),
        Verdict::Other => return Err(format!("{name} came back running {after}")),
    }
    println!(
        "{name} runs {after}; watching {} s for a rollback",
        options.settle.as_secs()
    );
    if let Some(later) = settle(&mut link, id, options.settle)? {
        return match verdict(&later) {
            Verdict::New => {
                println!("{name} restarted and still runs {later}");
                Ok(())
            }
            Verdict::Old => Err(format!("{name} rolled back to {later}")),
            Verdict::Other => Err(format!("{name} came back running {later}")),
        };
    }
    println!("{name} now runs {after}");
    Ok(())
}

enum Verdict {
    New,
    Old,
    Other,
}

/// Writes the image, a part at a time, keeping the lock.
fn send(session: &mut Session<'_>, target: &Target, image: &[u8]) -> Result<(), String> {
    let mut sent = 0;
    let mut shown = 0;
    let started = std::time::Instant::now();
    while sent < image.len() {
        session.keep_lock()?;
        let length = (image.len() - sent).min(usize::from(MEMORY_SEGMENT));
        let address = target.start + sent as u64;
        let part = &image[sent..sent + length];
        let mut last = String::new();
        let mut written = false;
        for _ in 0..TRIES {
            let now = session.link.now();
            let command = session
                .link
                .controller
                .write_memory(now, session.id, address, part);
            match session.link.wait(command, ANSWER_WITHIN)? {
                Outcome::Done => {
                    let answer = session
                        .link
                        .controller
                        .take_memory_read(command)
                        .ok_or("an answer went missing")?;
                    if answer.status != AaStatus::SUCCESS {
                        return Err(format!(
                            "{} refused the image at byte {sent}: {:?}",
                            session.name, answer.status
                        ));
                    }
                    written = true;
                    break;
                }
                outcome => last = describe(outcome),
            }
        }
        if !written {
            return Err(format!("writing at byte {sent}: {last}"));
        }
        sent += length;
        let tenth = sent * 10 / image.len();
        if tenth > shown && sent < image.len() {
            shown = tenth;
            eprintln!("{}: {sent} of {} bytes sent", session.name, image.len());
        }
    }
    let seconds = started.elapsed().as_secs_f64().max(0.001);
    println!(
        "{}: {} bytes sent in {seconds:.1} s ({:.0} KB/s)",
        session.name,
        image.len(),
        image.len() as f64 / seconds / 1000.0
    );
    Ok(())
}

enum Stored {
    Kept,
    Refused,
    /// The device went away to restart before its 1000 was heard.
    Restarting,
}

/// Waits for the store's OPERATION_STATUS: 1000 kept, 0 or another value
/// over 1000 but 0xFFFF refused.
fn stored(session: &mut Session<'_>, target: &Target, store: u16) -> Result<Stored, String> {
    let until = session.link.now() + STORE_WITHIN;
    let mut shown = 0;
    loop {
        session.keep_lock()?;
        let events = session
            .link
            .events(until.min(session.link.now() + LOCK_AGAIN))?;
        while let Some(status) = session.link.controller.poll_operation_status() {
            if status.entity_id != session.id
                || status.index != target.index
                || status.operation_id != store
            {
                continue;
            }
            match status.percent_complete {
                1000 => return Ok(Stored::Kept),
                0xffff => {}
                percent @ 1..=999 => {
                    if percent / 100 > shown {
                        shown = percent / 100;
                        eprintln!("{}: storing, {}%", session.name, percent / 10);
                    }
                }
                _ => return Ok(Stored::Refused),
            }
        }
        if events.iter().any(|event| {
            matches!(event, Event::EntityOffline(id, _) | Event::EntityRestarted(id) if *id == session.id)
        }) {
            return Ok(Stored::Restarting);
        }
        if session.link.now() >= until {
            return Err(format!(
                "{} said nothing of the image's check",
                session.name
            ));
        }
    }
}

/// Waits for the device to come back after its restart and reads its
/// firmware_version: online again after going, or advertising as just
/// started.
fn back(link: &mut Link, id: EntityId) -> Result<String, String> {
    let until = link.now() + BACK_WITHIN;
    loop {
        for event in link.events(until)? {
            if matches!(event, Event::EntityOnline(back) | Event::EntityRestarted(back) if back == id)
            {
                return firmware_version(link, id);
            }
        }
        if link.now() >= until {
            return Err("it did not come back".into());
        }
    }
}

/// Watches the device for `within`: its firmware_version if it restarts.
fn settle(link: &mut Link, id: EntityId, within: Duration) -> Result<Option<String>, String> {
    let until = link.now() + within;
    let mut gone = false;
    while link.now() < until {
        for event in link.events(until)? {
            match event {
                Event::EntityOffline(gone_id, _) if gone_id == id => gone = true,
                Event::EntityOnline(back) | Event::EntityRestarted(back) if back == id => {
                    return firmware_version(link, id).map(Some);
                }
                _ => {}
            }
        }
    }
    if gone {
        return back(link, id).map(Some);
    }
    Ok(None)
}

/// Reads the device's ENTITY descriptor afresh for its firmware_version.
fn firmware_version(link: &mut Link, id: EntityId) -> Result<String, String> {
    let now = link.now();
    link.controller.enumerate(now, id);
    let until = link.now() + Duration::from_secs(10);
    'enumerated: loop {
        for event in link.events(until)? {
            if matches!(event, Event::EntityEnumerated(done) if done == id) {
                break 'enumerated;
            }
        }
        if link.now() >= until {
            break;
        }
    }
    let now = link.now();
    let command = link
        .controller
        .read_descriptor(now, id, DescriptorType::ENTITY, 0);
    match link.wait(command, ANSWER_WITHIN)? {
        Outcome::Done => {}
        outcome => return Err(format!("reading its version again: {}", describe(outcome))),
    }
    link.controller
        .model(id)
        .and_then(|model| model.entity())
        .map(|entity| entity.firmware_version.to_owned())
        .ok_or_else(|| "its version went missing".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image as idf.py lays one out, `length` octets.
    fn image(length: usize, signatures: usize) -> Vec<u8> {
        let mut image = vec![0u8; length];
        image[0] = 0xe9;
        image[12] = 18;
        image[32..36].copy_from_slice(&0xabcd_5432u32.to_le_bytes());
        image[48..53].copy_from_slice(b"1.2.3");
        image[80..92].copy_from_slice(b"avb_endpoint");
        for (at, octet) in image[176..208].iter_mut().enumerate() {
            *octet = 0xd0 + at as u8;
        }
        if signatures > 0 {
            let sector = length - 4096;
            for block in 0..signatures {
                image[sector + block * 1216] = 0xe7;
                image[sector + block * 1216 + 1] = 0x02;
            }
        }
        image
    }

    #[test]
    fn an_esp_images_header_and_description_are_read() {
        let described = parse_image(&image(8192, 2)).unwrap();
        assert_eq!(described.version, "1.2.3");
        assert_eq!(described.project, "avb_endpoint");
        assert_eq!(described.chip(), "ESP32-P4");
        assert_eq!(described.elf_sha9(), "d0d1d2d3d");
        assert_eq!(described.signatures, 2);
        assert_eq!(parse_image(&image(8000, 0)).unwrap().signatures, 0);
        let mut bad = image(8192, 0);
        bad[0] = 0;
        assert!(parse_image(&bad).is_err());
        let mut bad = image(8192, 0);
        bad[32] = 0;
        assert!(parse_image(&bad).is_err());
    }

    #[test]
    fn a_boards_version_ends_with_its_elf_sha9() {
        assert_eq!(
            version_sha9("1.0.0 (esp_avb 2.45.0 - dccd7ef2f)").as_deref(),
            Some("dccd7ef2f")
        );
        assert_eq!(version_sha9("0.3.2"), None);
    }

    #[test]
    fn options_parse() {
        let words: Vec<String> = ["esp-mic", "new.bin", "--stop-streams", "--settle", "5"]
            .iter()
            .map(|word| word.to_string())
            .collect();
        let options = parse(&words).unwrap();
        assert_eq!(options.entity, "esp-mic");
        assert_eq!(options.image, PathBuf::from("new.bin"));
        assert!(options.stop_streams && !options.no_reboot);
        assert_eq!(options.settle, Duration::from_secs(5));
        assert!(parse(&["esp-mic".into()]).is_err());
        assert!(parse(&["a".into(), "b".into(), "c".into()]).is_err());
    }
}
