//! The least of an app on the rings: what its entity's first input takes,
//! its first output sends back, for as long as asked. With
//! scripts/smoke/room.sh, the room then records what it played.
//!
//!   ring_echo <interface> <instance> <seconds>

use std::time::{Duration, Instant};

use avb_endpointd::ring::{App, Error, StreamRef};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [interface, instance, seconds] = args.as_slice() else {
        return Err("ring_echo <interface> <instance> <seconds>".into());
    };
    let stream = StreamRef {
        instance: instance.parse().map_err(|_| "an instance from 0 to 239")?,
        index: 0,
    };
    let seconds: u64 = seconds.parse().map_err(|_| "seconds, a whole number")?;
    // The daemon makes its socket and rings as it starts.
    let until = Instant::now() + Duration::from_secs(30);
    let (mut input, mut out) = loop {
        let attached = App::connect(interface)
            .map_err(Error::from)
            .and_then(|app| Ok((app.inbound(stream)?, app.outbound(stream)?)));
        match attached {
            Ok(rings) => break rings,
            Err(error) if Instant::now() > until => return Err(error.to_string()),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    input.wake_every(48);
    let mut samples = Vec::new();
    let started = Instant::now();
    let (mut echoed, mut overruns) = (0usize, 0u64);
    while started.elapsed() < Duration::from_secs(seconds) {
        input
            .wait(Some(Duration::from_millis(20)))
            .map_err(|error| error.to_string())?;
        loop {
            match input.next(&mut samples) {
                Ok(Some(received)) => {
                    out.write(&samples).map_err(|error| error.to_string())?;
                    echoed += received.frames;
                }
                Ok(None) => break,
                Err(Error::Overrun { lost_frames }) => overruns += lost_frames,
                Err(error) => return Err(error.to_string()),
            }
        }
        while out.sent().map_err(|error| error.to_string())?.is_some() {}
    }
    println!("ring_echo: {echoed} frames echoed, {overruns} lost to overruns");
    Ok(())
}
