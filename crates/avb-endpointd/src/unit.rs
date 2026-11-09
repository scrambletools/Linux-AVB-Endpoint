//! The daemon's user unit, `avb-endpointd@<interface>.service`, as the
//! apps run it with `systemctl --user`: started and enabled when a person
//! puts this computer on the network on an interface, through any app, so
//! it runs at each login after, with or without the app, its default
//! entity and any others with it; stopped and disabled when they take it
//! off.

use std::io;
use std::process::{Command, Stdio};

use crate::status::{EXIT_BAD_CONFIG, EXIT_NO_INTERFACE, EXIT_REFUSED};

/// Whether a unit can be named for `interface` as it is: letters, digits,
/// dots, dashes and underscores, which the unit passes on unescaped.
pub fn nameable(interface: &str) -> bool {
    !interface.is_empty()
        && interface
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The daemon's unit on `interface`: `None` where none can be named.
pub fn name(interface: &str) -> Option<String> {
    nameable(interface).then(|| format!("avb-endpointd@{interface}.service"))
}

/// What the daemon's unit on an interface is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// It runs, or is starting.
    Active,
    /// It is not running, and did not fail.
    Inactive,
    /// Another daemon holds the interface, this person's or another's or
    /// root's, and it will not start again until started.
    Refused,
    /// No such interface.
    NoInterface,
    /// The configuration file does not parse.
    BadConfig,
    /// It failed otherwise, or crashed past its restarts.
    Failed,
}

/// Starts the daemon on `interface` and enables it, so it starts at each
/// login: its failures forgotten first, as a refusal stops its restarts.
/// One running already reads the file's changes itself.
pub fn start(interface: &str) -> io::Result<()> {
    let unit = unit(interface)?;
    let _ = systemctl(&["reset-failed", &unit]);
    systemctl(&["enable", "--now", &unit])
}

/// Stops the daemon on `interface`, taking every entity it runs there off
/// the network, and its starting at login.
pub fn disable(interface: &str) -> io::Result<()> {
    systemctl(&["disable", "--now", &unit(interface)?])
}

/// What the daemon's unit on `interface` is doing, from systemd.
pub fn state(interface: &str) -> io::Result<UnitState> {
    let unit = unit(interface)?;
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=ActiveState",
            "--property=ExecMainStatus",
            &unit,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!("systemctl show {unit} failed")));
    }
    Ok(parse_state(&String::from_utf8_lossy(&output.stdout)))
}

/// A unit's state from `systemctl show`'s ActiveState and ExecMainStatus.
fn parse_state(shown: &str) -> UnitState {
    let value = |key: &str| {
        shown
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
    };
    let status: u8 = value("ExecMainStatus").parse().unwrap_or(0);
    match value("ActiveState") {
        "active" | "activating" | "reloading" => UnitState::Active,
        "failed" => match status {
            EXIT_REFUSED => UnitState::Refused,
            EXIT_NO_INTERFACE => UnitState::NoInterface,
            EXIT_BAD_CONFIG => UnitState::BadConfig,
            _ => UnitState::Failed,
        },
        _ => UnitState::Inactive,
    }
}

fn unit(interface: &str) -> io::Result<String> {
    name(interface).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no unit can be named for the interface {interface:?}"),
        )
    })
}

fn systemctl(arguments: &[&str]) -> io::Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "systemctl --user {} failed: {status}",
            arguments.join(" ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_is_named_for_the_interface_as_it_is() {
        assert_eq!(
            name("enp2s0").as_deref(),
            Some("avb-endpointd@enp2s0.service")
        );
        assert_eq!(
            name("br-lan.2").as_deref(),
            Some("avb-endpointd@br-lan.2.service")
        );
        assert_eq!(name("enp2s0 x"), None);
        assert_eq!(name("../x"), None);
        assert_eq!(name(""), None);
    }

    /// A failed unit tells a refusal, a missing interface and a file that
    /// doesn't parse from other failures by its exit status.
    #[test]
    fn a_units_state_tells_why_it_stopped() {
        let shown = |state: &str, status: u8| {
            parse_state(&format!("ActiveState={state}\nExecMainStatus={status}\n"))
        };
        assert_eq!(shown("active", 0), UnitState::Active);
        assert_eq!(shown("activating", 0), UnitState::Active);
        assert_eq!(shown("inactive", 0), UnitState::Inactive);
        assert_eq!(shown("failed", EXIT_REFUSED), UnitState::Refused);
        assert_eq!(shown("failed", EXIT_NO_INTERFACE), UnitState::NoInterface);
        assert_eq!(shown("failed", EXIT_BAD_CONFIG), UnitState::BadConfig);
        assert_eq!(shown("failed", 1), UnitState::Failed);
        assert_eq!(parse_state(""), UnitState::Inactive);
    }
}
