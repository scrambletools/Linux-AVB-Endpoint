//! Runs a Linux computer's AVB and Milan entities, so that any app can
//! play to and record from the network through PipeWire. One daemon per
//! interface, the user unit `avb-endpointd@<interface>`, runs every
//! entity on the computer: each an ATDECC entity whose streams any
//! controller can connect, with audio from and to a PipeWire device apps
//! play into or record from.
//!
//! Without features the crate is what apps share with the daemon, on
//! every platform: the entities to run ([`config`]) and the models they
//! show ([`model`]), what the daemon is doing ([`status`]), how loud its
//! streams are ([`meters`]), where those files are ([`store`]), the
//! daemon's user unit ([`unit`]), how an entity's ID is made
//! ([`entity_id`]), and the streams on other devices that get a device
//! of their own here ([`devices`]). The `ring` feature adds the app
//! rings' client ([`ring`]), for apps that take a stream's samples with
//! their presentation times; the `daemon` feature adds the daemon, for
//! Linux.

use atdecc::EntityId;
use avb_net::MacAddress;

pub mod config;
pub mod connections;
pub mod devices;
pub mod meters;
pub mod model;
pub mod status;
pub mod store;
pub mod unit;

#[cfg(feature = "daemon")]
pub mod discovery;
#[cfg(feature = "daemon")]
pub mod gptp;
#[cfg(feature = "daemon")]
pub mod lite;
#[cfg(feature = "daemon")]
pub mod port;
#[cfg(feature = "daemon")]
pub mod profile;
#[cfg(feature = "ring")]
pub mod ring;
#[cfg(feature = "daemon")]
pub mod runtime;
#[cfg(feature = "daemon")]
pub mod sharing;
#[cfg(feature = "daemon")]
pub mod signal;
#[cfg(feature = "daemon")]
mod watch;

/// The entity models of the daemon's entities, under Scramble Tools' MA-S
/// `8C-1F-64-36-C`: this, and a hash of their streams. triib 1.0's
/// talkers and listeners had `0x8c1f_6436_c000_0002` and `_0004`.
pub const DEVICE_MODELS: u64 = 0x8c1f_6436_c001_0000;

/// An entity ID from the interface's address and the entity's instance,
/// apart from triib's controller (FF-FE), triib-cli's (FF-FD) and plaa's
/// (FF-FC).
pub fn entity_id(mac: MacAddress, instance: u8) -> EntityId {
    let [a, b, c, d, e, f] = mac.0;
    EntityId(u64::from_be_bytes([a, b, c, 0xff, instance, d, e, f]))
}

/// Whether an entity model is one of the daemon's entities', telling them
/// apart before the status names them: the block's high bits and the bit
/// their hash always carries, which the apps' controllers lack.
pub fn is_device_model(model: atdecc::EntityModelId) -> bool {
    model.0 & 0xffff_ffff_ff01_0000 == DEVICE_MODELS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_id_takes_the_address_and_the_instance() {
        let mac = MacAddress([0xf0, 0xa7, 0x31, 0xf4, 0x0f, 0x14]);
        assert_eq!(entity_id(mac, 0), EntityId(0xf0a7_31ff_00f4_0f14));
        assert_eq!(entity_id(mac, 0x70), EntityId(0xf0a7_31ff_70f4_0f14));
        assert!(is_device_model(atdecc::EntityModelId(
            DEVICE_MODELS | 0x01_2345
        )));
        assert!(!is_device_model(atdecc::EntityModelId(
            0x8c1f_6436_c020_0005
        )));
    }
}
