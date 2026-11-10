//! The neutral tool, for a person with no AVB app: `avb-endpointd on`
//! and `off` start or stop the daemon's unit on an interface; `list`
//! shows the streams on other devices it found; `pin`, `unpin`, `hide`,
//! `show` and `mode` change the interface's file under its lock, as an
//! app does with the library.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use avb_endpointd::config::{self, Config, Shown};
use avb_endpointd::devices::{DeviceState, RemoteStream, Way};
use avb_endpointd::status::{self, DeviceStatus, EXIT_USAGE};
use avb_endpointd::unit;

pub const COMMANDS: [&str; 8] = ["on", "off", "list", "pin", "unpin", "hide", "show", "mode"];

pub const USAGE: &str = "\
usage: avb-endpointd --interface NAME [--config FILE]
       avb-endpointd on|off|list --interface NAME
       avb-endpointd pin|unpin|hide|show STREAM --interface NAME
       avb-endpointd mode destinations|sources all|pinned --interface NAME
STREAM is a device's name as list shows it, or <entity id>:in<n> for a
listener input, <entity id>:out<n> for a talker output.";

/// Runs `command` with its `words` on `interface`'s file, or `path`.
pub fn run(command: &str, words: &[String], interface: &str, path: Option<PathBuf>) -> ExitCode {
    let Some(path) = path.or_else(|| config::path(interface)) else {
        eprintln!("no configuration folder for {interface}.toml; give a file with --config");
        return ExitCode::FAILURE;
    };
    let outcome = match (command, words) {
        ("on", []) => unit::start(interface)
            .map(|()| format!("this computer is on the network on {interface}"))
            .map_err(|error| error.to_string()),
        ("off", []) => unit::disable(interface)
            .map(|()| format!("this computer is off the network on {interface}"))
            .map_err(|error| error.to_string()),
        ("list", []) => Ok(list(interface, &path)),
        ("pin" | "unpin" | "hide" | "show", [stream]) => {
            change_stream(command, stream, interface, &path)
        }
        ("mode", [way, shown]) => set_mode(way, shown, &path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match outcome {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// The streams the daemon found and those the file lists, one a line.
fn list(interface: &str, path: &Path) -> String {
    let config = config::load(path).unwrap_or_default();
    let found = status::read(interface).map(|status| status.devices);
    let mut lines = vec![format!(
        "destinations: {}, sources: {}",
        shown_text(config.destinations),
        shown_text(config.sources)
    )];
    let devices = found.clone().unwrap_or_default();
    for device in &devices {
        lines.push(device_line(device));
    }
    // Those the file lists that the daemon has not reported.
    for (entry, listed) in config
        .pin
        .iter()
        .map(|entry| (entry, "pinned"))
        .chain(config.hide.iter().map(|entry| (entry, "hidden")))
    {
        let Some(stream) = entry.stream() else {
            continue;
        };
        if devices.iter().any(|device| device.stream() == Some(stream)) {
            continue;
        }
        lines.push(format!("{stream}  {}  {listed}, not found", entry.name));
    }
    if found.is_none() {
        lines.push(format!("the daemon does not run on {interface}"));
    } else if lines.len() == 1 {
        lines.push("no streams on other devices found yet".into());
    }
    lines.join("\n")
}

fn device_line(device: &DeviceStatus) -> String {
    let stream = device
        .stream()
        .map_or_else(|| device.entity_id.clone(), |stream| stream.to_string());
    let mut marks = Vec::new();
    if device.pinned {
        marks.push(if device.remembered {
            "remembered"
        } else {
            "pinned"
        });
    }
    if device.hidden {
        marks.push("hidden");
    }
    if device.shown {
        marks.push("shown");
    }
    let channels = if device.live_channels != device.channels && device.live_channels > 0 {
        format!("{} of {} channels", device.live_channels, device.channels)
    } else {
        format!("{} channels", device.channels)
    };
    format!(
        "{stream}  {}  {}, {channels}{}{}",
        device.name,
        state_text(device.state),
        if marks.is_empty() { "" } else { ", " },
        marks.join(", ")
    )
}

fn state_text(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Idle => "idle",
        DeviceState::Connected => "connected",
        DeviceState::Busy => "busy",
        DeviceState::Offline => "offline",
        DeviceState::NoTalker => "no talker free",
        DeviceState::NoBandwidth => "no bandwidth",
        DeviceState::NoFormat => "no format it takes",
        _ => "other",
    }
}

fn shown_text(shown: Shown) -> &'static str {
    match shown {
        Shown::All => "all",
        Shown::Pinned => "pinned",
    }
}

/// The stream `text` names, and its name where the daemon found it:
/// `<entity id>:in<n>` or `:out<n>`, or a name the daemon reported, as
/// long as only one stream has it.
fn resolve(text: &str, interface: &str) -> Result<(RemoteStream, String), String> {
    let devices = status::read(interface)
        .map(|status| status.devices)
        .unwrap_or_default();
    if let Ok(stream) = text.parse::<RemoteStream>() {
        let name = devices
            .iter()
            .find(|device| device.stream() == Some(stream))
            .map(|device| device.name.clone())
            .unwrap_or_default();
        return Ok((stream, name));
    }
    let named: Vec<&DeviceStatus> = devices
        .iter()
        .filter(|device| device.name.eq_ignore_ascii_case(text))
        .collect();
    match named.as_slice() {
        [device] => device
            .stream()
            .map(|stream| (stream, device.name.clone()))
            .ok_or_else(|| format!("{text}: the daemon reported no entity ID for it")),
        [] => Err(format!(
            "{text}: no stream on another device has that name; avb-endpointd list --interface {interface} shows them"
        )),
        _ => Err(format!(
            "{text}: more than one stream has that name; give its <entity id>:in<n> or :out<n>"
        )),
    }
}

fn change_stream(
    command: &str,
    text: &str,
    interface: &str,
    path: &Path,
) -> Result<String, String> {
    let (stream, name) = resolve(text, interface)?;
    let shown = if name.is_empty() {
        stream.to_string()
    } else {
        format!("{name} ({stream})")
    };
    let changed = config::edit_at(path, |config: &mut Config| match command {
        "pin" => config.pin(stream, &name, false),
        "unpin" => config.unpin(&stream),
        "hide" => config.hide(stream, &name),
        _ => config.show(&stream),
    })
    .map_err(|error| format!("{}: {error}", path.display()))?;
    let done = match (command, changed) {
        ("pin", true) => "pinned",
        ("pin", false) => "pinned already",
        ("unpin", true) => "unpinned",
        ("unpin", false) => "was not pinned",
        ("hide", true) => "hidden",
        ("hide", false) => "hidden already",
        (_, true) => "shown again",
        (_, false) => "was not hidden",
    };
    Ok(format!("{shown}: {done}"))
}

fn set_mode(way: &str, shown: &str, path: &Path) -> Result<String, String> {
    let way = match way {
        "destinations" => Way::Destination,
        "sources" => Way::Source,
        _ => return Err(format!("{way}: not destinations or sources\n{USAGE}")),
    };
    let shown = match shown {
        "all" => Shown::All,
        "pinned" => Shown::Pinned,
        _ => return Err(format!("{shown}: not all or pinned\n{USAGE}")),
    };
    config::edit_at(path, |config: &mut Config| config.set_shown(way, shown))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let way = match way {
        Way::Destination => "destinations",
        Way::Source => "sources",
    };
    Ok(format!("{way}: {}", shown_text(shown)))
}
