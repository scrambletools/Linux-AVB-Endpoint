//! Apps' binds end to end, for scripts/smoke/bind.sh: through the first
//! daemon's app socket, binds its entity's ring input to the second's
//! talker and the second's listener to its ring output, as an app with no
//! controller of its own does, and checks that media comes each way, that
//! each binding reads back, that the bad ones are refused as they should
//! be, and that unbinding stops it all.
//!
//!   ring_bind <sockets' folder> <the second's entity ID> <configurations' folder>
//!
//! On v0, instance 1 has an output, "Say", and an input, "Hear"; on v1,
//! instance 1 an input, "Speaker", and an output, "Mic", each the first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use avb_endpointd::ring::{App, BindError, Error, Inbound, Remote, StreamRef};

const FIRST: StreamRef = StreamRef {
    instance: 1,
    index: 0,
};
/// 2 channels of AAF, 32 bits at 48 kHz: class A, 6 samples a frame, as
/// the streams start; class B, 12.
const CLASS_A: u64 = 0x0205_0220_0080_6000;
const CLASS_B: u64 = 0x0205_0220_0080_c000;

fn main() {
    let mut args = std::env::args().skip(1);
    let folder = PathBuf::from(args.next().expect("the sockets' folder"));
    let room = args.next().expect("the second's entity ID");
    let room = u64::from_str_radix(room.trim_start_matches("0x"), 16).expect("an entity ID");
    let config = PathBuf::from(args.next().expect("the configurations' folder"));
    if let Err(message) = check(&folder, room, &config.join("v0.toml")) {
        println!("FAIL: {message}");
        std::process::exit(1);
    }
}

/// Tries `connect` until it works, 20 s at most, as the daemons start.
fn connected(path: &Path) -> Result<App, String> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match App::connect_at(path) {
            Ok(app) => return Ok(app),
            Err(error) if Instant::now() > until => return Err(error.to_string()),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Waits up to 10 s for a block with a presentation time on `input`.
fn hears(input: &mut Inbound) -> bool {
    let mut samples = Vec::new();
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        input.wait(Some(Duration::from_millis(50))).ok();
        loop {
            match input.next(&mut samples) {
                Ok(Some(block)) if block.presented.is_some() => return true,
                Ok(Some(_)) | Err(Error::Overrun { .. }) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
    false
}

/// No block for 300 ms on end, within 3 s, as the stream stops.
fn silent(input: &mut Inbound) -> bool {
    let mut samples = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    let mut last = Instant::now();
    while Instant::now() < until {
        match input.next(&mut samples) {
            Ok(Some(_)) => last = Instant::now(),
            _ if last.elapsed() >= Duration::from_millis(300) => return true,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    false
}

/// Asks `bound` until it says `wanted`, 5 s at most: an output's listener
/// reads back once the controller has read it again.
fn reads(app: &App, out: bool, wanted: Option<Remote>) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let bound = app.bound(out, FIRST).map_err(|error| error.to_string())?;
        if bound == wanted {
            return Ok(());
        }
        if Instant::now() > until {
            return Err(format!("bound({out}) says {bound:?}, not {wanted:?}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Waits up to 5 s for the configuration at `path` to have `line`, or
/// not to.
fn kept(path: &Path, line: &str, wanted: bool) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        if text.lines().any(|kept| kept.trim() == line) == wanted {
            return Ok(());
        }
        if Instant::now() > until {
            let has = if wanted { "lacks" } else { "still has" };
            return Err(format!("{} {has} {line}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn check(folder: &Path, room: u64, config: &Path) -> Result<(), String> {
    let app = connected(&folder.join("v0.app"))?;
    let room_app = connected(&folder.join("v1.app"))?;
    let mic = Remote {
        entity: room,
        index: 0,
    };
    let speaker = Remote {
        entity: room,
        index: 0,
    };
    let failed = |what: &str, error: BindError| format!("{what}: {error}");

    // Nothing bound at start, and the controller not running.
    reads(&app, false, None)?;
    reads(&app, true, None)?;

    // The bad ones.
    let nowhere = StreamRef {
        instance: 9,
        index: 0,
    };
    match app.bind(false, nowhere, mic, None) {
        Err(BindError::NoStream) => println!("ok: a stream the daemon lacks is refused"),
        other => return Err(format!("binding a stream it lacks gave {other:?}")),
    }
    match app.bind(false, FIRST, mic, Some(0x0123_4567_89ab_cdef)) {
        Err(BindError::Format) => println!("ok: a format the stream lacks is refused"),
        other => return Err(format!("binding in a format it lacks gave {other:?}")),
    }

    // The input, to the room's talker: first in class B, which it offers
    // though configured in class A, then, unbound, in class A, the
    // talker's.
    let mut hear = app.inbound(FIRST).map_err(|error| error.to_string())?;
    app.bind(false, FIRST, mic, Some(CLASS_B))
        .map_err(|error| failed("binding in class B", error))?;
    reads(&app, false, Some(mic))?;
    println!("ok: the input binds to the talker, in class B");
    app.unbind(false, FIRST, mic)
        .map_err(|error| failed("unbinding from class B", error))?;
    app.bind(false, FIRST, mic, Some(CLASS_A))
        .map_err(|error| failed("binding in class A", error))?;
    if !hears(&mut hear) {
        return Err("the input bound to the talker heard nothing in 10 s".into());
    }
    println!("ok: the input hears the talker it was bound to");
    let binding = format!("bound = \"0x{room:016x}:0\"");
    kept(config, &binding, true)?;
    println!("ok: the configuration keeps the input's binding");

    // The output, to the room's listener: the controller starts, finds the
    // listener and binds it.
    let mut speaker_ring = room_app.inbound(FIRST).map_err(|error| error.to_string())?;
    let asked = Instant::now();
    app.bind(true, FIRST, speaker, None)
        .map_err(|error| failed("binding the listener", error))?;
    println!(
        "ok: the listener binds to the output, answered in {} ms",
        asked.elapsed().as_millis()
    );
    if !hears(&mut speaker_ring) {
        return Err("the listener bound to the output heard nothing in 10 s".into());
    }
    println!("ok: the listener hears the output");
    reads(&app, true, Some(speaker))?;
    println!("ok: both bindings read back");
    // A listener not on the network: no answer, in about 3 s.
    let asked = Instant::now();
    let missing = Remote {
        entity: 0x0011_2233_4455_6677,
        index: 0,
    };
    match app.bind(true, FIRST, missing, None) {
        Err(BindError::NoAnswer) => println!(
            "ok: a listener not on the network is no answer, in {} ms",
            asked.elapsed().as_millis()
        ),
        other => return Err(format!("binding a listener not there gave {other:?}")),
    }

    // Unbinding stops each.
    app.unbind(true, FIRST, speaker)
        .map_err(|error| failed("unbinding the listener", error))?;
    reads(&app, true, None)?;
    if !silent(&mut speaker_ring) {
        return Err("the listener still hears the output, unbound".into());
    }
    app.unbind(false, FIRST, mic)
        .map_err(|error| failed("unbinding the input", error))?;
    reads(&app, false, None)?;
    kept(config, &binding, false)?;
    if !silent(&mut hear) {
        return Err("the input still hears the talker, unbound".into());
    }
    // Again: nothing to do.
    app.unbind(false, FIRST, mic)
        .map_err(|error| failed("unbinding the input again", error))?;
    println!("ok: unbound, both are silent");
    Ok(())
}
