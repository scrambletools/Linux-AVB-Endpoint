//! `avb-endpointd crash`: saves the crash dump a device keeps in a
//! CRASH_DUMP memory object (IEEE 1722.1-2021, 7.2.10), read through
//! Address Access, so a device's crash needs no console cable to look
//! into; with `--erase`, erases it after (START_OPERATION, ERASE). An
//! ESP32 board keeps ESP-IDF's core dump image as stored, which
//! `esp-coredump` reads with the crashed firmware's ELF.
//!
//! It reads the dump's length afresh, then the dump one Address Access
//! READ at a time, as much as one response carries, trying each part
//! three times. Reading never clears a dump.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use atdecc::EntityId;
use atdecc::aecp::AaStatus;
use atdecc::controller::Outcome;
use atdecc::descriptor::{DescriptorType, MemoryObjectOperation, MemoryObjectType};
use atdecc::entity::MEMORY_SEGMENT;
use avb_endpointd::status::EXIT_USAGE;

use crate::tool_net::{self, Link, Wanted};

/// How long to look for the device asked for.
const FIND_FOR: Duration = Duration::from_secs(10);
/// How long a command may take, its retry included.
const ANSWER_WITHIN: Duration = Duration::from_secs(2);
/// Tries for each part of the dump.
const TRIES: u32 = 3;

struct Options {
    entity: String,
    output: Option<PathBuf>,
    erase: bool,
}

fn parse(words: &[String]) -> Result<Options, String> {
    let mut entity = None;
    let mut output = None;
    let mut erase = false;
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-o" | "--output" => {
                let file = words.next().ok_or("-o needs a file")?;
                output = Some(PathBuf::from(file));
            }
            "--erase" => erase = true,
            flag if flag.starts_with('-') => return Err(format!("no option {flag} for crash")),
            text if entity.is_none() => entity = Some(text.to_owned()),
            text => return Err(format!("crash takes one device, not {text} too")),
        }
    }
    let entity = entity.ok_or("crash needs a device: its entity ID or its name")?;
    Ok(Options {
        entity,
        output,
        erase,
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
    match crash(&options, interface) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("crash: {error}");
            ExitCode::FAILURE
        }
    }
}

/// What a device's CRASH_DUMP descriptor says: its index, where its
/// octets start and how many there are.
struct CrashDump {
    index: u16,
    start: u64,
    length: u64,
}

fn crash_dump(link: &Link, id: EntityId) -> Option<CrashDump> {
    link.controller
        .model(id)?
        .memory_objects()
        .find(|object| object.memory_object_type == MemoryObjectType::CRASH_DUMP)
        .map(|object| CrashDump {
            index: object.index,
            start: object.start_address,
            length: object.length,
        })
}

/// Reads the device's CRASH_DUMP descriptor again, for its length now.
fn refresh(link: &mut Link, id: EntityId, index: u16) -> Result<CrashDump, String> {
    let now = link.now();
    let command = link
        .controller
        .read_descriptor(now, id, DescriptorType::MEMORY_OBJECT, index);
    match link.wait(command, ANSWER_WITHIN)? {
        Outcome::Done => crash_dump(link, id).ok_or_else(|| "its crash dump went away".into()),
        outcome => Err(format!("its crash dump's length: {outcome:?}")),
    }
}

fn crash(options: &Options, interface: &str) -> Result<(), String> {
    let wanted = Wanted::of(&options.entity);
    let mut link = Link::open(interface)?;
    let id = link
        .find(&wanted, FIND_FOR)
        .map_err(|error| format!("{error} on {interface}"))?;
    let model = link.controller.model(id).ok_or("its model went away")?;
    let name = match model.entity_name() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => tool_net::describe(&Wanted::Id(id)),
    };
    let firmware = model
        .entity()
        .map(|entity| entity.firmware_version.to_owned())
        .unwrap_or_default();
    let Some(object) = crash_dump(&link, id) else {
        return Err(format!(
            "{name} keeps no crash dump (no CRASH_DUMP memory object)"
        ));
    };
    let object = refresh(&mut link, id, object.index)?;
    if object.length == 0 {
        println!("{name}: no crash dump");
        return Ok(());
    }
    let dump = read(&mut link, id, &object, &name)?;
    let path = options
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("crash-{:016x}.core", id.0)));
    std::fs::write(&path, &dump).map_err(|error| format!("{}: {error}", path.display()))?;
    println!(
        "{name}: crash dump of {} bytes saved to {}",
        dump.len(),
        path.display()
    );
    println!("{name} runs firmware {firmware} now; the dump names the ELF it crashed in");
    println!(
        "decode: esp-coredump info_corefile --core {} --core-format raw <that firmware's .elf>",
        path.display()
    );
    if options.erase {
        erase(&mut link, id, &object)?;
        println!("{name}: crash dump erased");
    }
    Ok(())
}

/// The dump's octets, read a part at a time.
fn read(link: &mut Link, id: EntityId, object: &CrashDump, name: &str) -> Result<Vec<u8>, String> {
    let total = usize::try_from(object.length).map_err(|_| "a dump too long to hold")?;
    let mut dump = Vec::with_capacity(total);
    let mut shown = 0;
    while dump.len() < total {
        let length = (total - dump.len()).min(usize::from(MEMORY_SEGMENT)) as u16;
        let address = object.start + dump.len() as u64;
        let mut last = String::new();
        let mut part = None;
        for _ in 0..TRIES {
            let now = link.now();
            let command = link.controller.read_memory(now, id, &[(address, length)]);
            match link.wait(command, ANSWER_WITHIN)? {
                Outcome::Done => {
                    let answer = link
                        .controller
                        .take_memory_read(command)
                        .ok_or("an answer went missing")?;
                    if answer.status != AaStatus::SUCCESS {
                        return Err(format!(
                            "{name} refused to give its dump at 0x{address:x}: {:?}",
                            answer.status
                        ));
                    }
                    match answer.tlvs.first() {
                        Some((at, octets))
                            if *at == address && octets.len() == usize::from(length) =>
                        {
                            part = Some(octets.clone());
                            break;
                        }
                        _ => last = "an answer of the wrong part".into(),
                    }
                }
                outcome => last = format!("{outcome:?}"),
            }
        }
        let part = part.ok_or_else(|| format!("reading at 0x{address:x}: {last}"))?;
        dump.extend_from_slice(&part);
        let tenth = dump.len() * 10 / total;
        if tenth > shown && dump.len() < total {
            shown = tenth;
            eprintln!("{name}: {} of {total} bytes read", dump.len());
        }
    }
    Ok(dump)
}

fn erase(link: &mut Link, id: EntityId, object: &CrashDump) -> Result<(), String> {
    let now = link.now();
    let command = link.controller.start_operation(
        now,
        id,
        DescriptorType::MEMORY_OBJECT,
        object.index,
        MemoryObjectOperation::ERASE.0,
        &[],
    );
    match link.wait(command, ANSWER_WITHIN)? {
        Outcome::Done => {}
        outcome => return Err(format!("erasing its crash dump: {outcome:?}")),
    }
    let after = refresh(link, id, object.index)?;
    if after.length != 0 {
        return Err(format!(
            "its crash dump still holds {} bytes after the erase",
            after.length
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn options_parse() {
        let options = parse(&words("esp-mic -o mic.core --erase")).unwrap();
        assert_eq!(options.entity, "esp-mic");
        assert_eq!(options.output, Some(PathBuf::from("mic.core")));
        assert!(options.erase);
        assert!(parse(&words("")).is_err());
        assert!(parse(&words("a b")).is_err());
        assert!(parse(&words("a --nope")).is_err());
        assert!(parse(&words("a -o")).is_err());
    }
}
