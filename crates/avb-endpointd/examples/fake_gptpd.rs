//! A stand-in for gptpd in a namespace's tests, never on a network: it
//! writes the time record gptpd would for each interface named, saying
//! this system is the grandmaster, its time CLOCK_MONOTONIC's plus a fixed
//! offset. Every daemon in the namespace then keeps one gPTP time, and
//! their streams carry presentation times as on a network.
//!
//!   fake_gptpd <folder> <interface>...
//!
//! The folder is where the daemons look, /run/gptpd, on a tmpfs mounted
//! over it inside the namespace.

// The monotonic clock, as libc gives it.
#![allow(unsafe_code)]

use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::Duration;

use gptp::ClockIdentity;
use gptp::time::{Mapping, State};

/// gPTP time less CLOCK_MONOTONIC's, in nanoseconds: some years.
const OFFSET: i128 = 1_700_000_000_000_000_000;
/// gptpd writes at each Sync, eight times a second.
const EVERY: Duration = Duration::from_millis(125);

fn monotonic_nanos() -> i64 {
    // SAFETY: all zeros is a valid timespec, which clock_gettime fills.
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: a live timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let folder = PathBuf::from(args.next().expect("the records' folder"));
    let files = args
        .map(|interface| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(folder.join(format!("{interface}.time")))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    for sequence in 0u32.. {
        let now = monotonic_nanos();
        let mapping = Mapping {
            generation: 1,
            state: State::Grandmaster,
            grandmaster: ClockIdentity(0x8c1f_64ff_fe36_c0ff),
            updated: now,
            monotonic: now,
            at_monotonic: i128::from(now) + OFFSET,
            monotonic_rate: 1.0,
            local: i128::from(now),
            at_local: i128::from(now) + OFFSET,
            local_rate: 1.0,
            asymmetry_included: false,
            residual: Some(0),
            // No card follows CLOCK_TAI here, so talkers send when due.
            tai: None,
        };
        let record = mapping.encode(sequence);
        for file in &files {
            file.write_at(&record, 0)?;
        }
        std::thread::sleep(EVERY);
    }
    Ok(())
}
