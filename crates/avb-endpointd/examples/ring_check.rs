//! The app rings end to end, for scripts/smoke/ring.sh: queues a counted
//! pattern into one daemon's outbound ring, and reads it from the other's
//! inbound ring, which a stream between them carries. It checks that
//! every word arrives as it went, that each block arrives with the
//! presentation time it went out with, that a flush silences the stream
//! from the next block on, and that attaching again finds the streams
//! going on as they were.
//!
//!   ring_check <sockets' folder>
//!
//! The talker's ring is instance 1's first output on v0, the listener's
//! instance 1's first input on v1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use avb_endpointd::ring::{App, Error, Flags, Inbound, Outbound, Received, Sent, StreamRef};

const ROOM: StreamRef = StreamRef {
    instance: 1,
    index: 0,
};
const RATE: i64 = 48_000;
/// How long the pattern plays.
const PLAYING: Duration = Duration::from_secs(3);

/// The first channel's word for pattern frame `count`: every bit used.
fn word(count: u32) -> i32 {
    count.wrapping_mul(0x9e37_79b1) as i32
}

fn main() {
    let folder = PathBuf::from(std::env::args().nth(1).expect("the sockets' folder"));
    if let Err(message) = check(&folder) {
        println!("FAIL: {message}");
        std::process::exit(1);
    }
}

/// Tries `attach` until it works, 20 s at most, as the daemons start.
fn attached<T>(attach: impl Fn() -> Result<T, Error>) -> Result<T, String> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match attach() {
            Ok(ring) => return Ok(ring),
            Err(error) if Instant::now() > until => return Err(error.to_string()),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn rings(folder: &Path) -> Result<(Outbound, Inbound), String> {
    let out = attached(|| App::connect_at(&folder.join("v0.app"))?.outbound(ROOM))?;
    let input = attached(|| App::connect_at(&folder.join("v1.app"))?.inbound(ROOM))?;
    Ok((out, input))
}

/// What came in: each block, and its samples.
#[derive(Default)]
struct Taken {
    blocks: Vec<(Received, Vec<i32>)>,
    overruns: u64,
}

impl Taken {
    fn drain(&mut self, input: &mut Inbound) -> Result<(), String> {
        let mut samples = Vec::new();
        loop {
            match input.next(&mut samples) {
                Ok(Some(received)) => self.blocks.push((received, samples.clone())),
                Ok(None) => return Ok(()),
                Err(Error::Overrun { lost_frames }) => self.overruns += lost_frames,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

fn drain_sent(out: &mut Outbound, sent: &mut Vec<Sent>) -> Result<(), String> {
    while let Some(block) = out.sent().map_err(|error| error.to_string())? {
        sent.push(block);
    }
    Ok(())
}

fn check(folder: &Path) -> Result<(), String> {
    let (mut out, mut input) = rings(folder)?;
    out.set_lead(Duration::from_millis(3));
    out.wake_every(48);
    input.wake_every(48);

    // The streams connect, and blocks come with presentation times.
    let mut taken = Taken::default();
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        input.wait(Some(Duration::from_millis(50))).ok();
        taken.drain(&mut input)?;
        if taken
            .blocks
            .iter()
            .any(|(block, _)| block.presented.is_some())
        {
            break;
        }
        if Instant::now() > until {
            return Err(format!(
                "no block with a presentation time came in 20 s ({} without)",
                taken.blocks.len()
            ));
        }
    }
    println!(
        "ok: the stream runs, rate {} Hz, {} channels each way",
        input.rate(),
        input.channels()
    );
    taken = Taken::default();
    let mut sent = Vec::new();
    drain_sent(&mut out, &mut sent)?;
    sent.clear();
    let channels = out.channels();

    // The pattern, 10 ms kept queued, the ring frame of each write's
    // first count noted.
    let mut placed: BTreeMap<u32, u64> = BTreeMap::new();
    let mut count = 1u32;
    let mut block = Vec::new();
    let started = Instant::now();
    while started.elapsed() < PLAYING {
        while out.queued() < 480 {
            block.clear();
            for frame in 0..48 {
                let value = count + frame;
                block.push(word(value));
                block.push(value as i32);
                block.extend(std::iter::repeat_n(0, channels.saturating_sub(2)));
            }
            let at = out.write(&block).map_err(|error| error.to_string())?;
            placed.insert(count, at.frame);
            count += 48;
        }
        out.wait(Some(Duration::from_millis(5))).ok();
        drain_sent(&mut out, &mut sent)?;
        taken.drain(&mut input)?;
    }
    let cut = out.flush();
    let flushed = Instant::now();
    while flushed.elapsed() < Duration::from_millis(300) {
        out.wait(Some(Duration::from_millis(5))).ok();
        drain_sent(&mut out, &mut sent)?;
        taken.drain(&mut input)?;
    }
    let generation = input.generation();
    let out_stats = out.take_stats();
    let in_stats = input.take_stats();

    // Every word as it went: the counts follow on, each with its word,
    // but where the talker left frames out or the listener missed some.
    let late_out: u64 = sent
        .iter()
        .filter(|block| block.flags.contains(Flags::LATE))
        .map(|block| block.frames as u64)
        .sum();
    let mut last: Option<u32> = None;
    let (mut frames, mut jumps, mut wrong) = (0u64, 0u64, 0u64);
    for (received, samples) in &taken.blocks {
        for frame in samples.chunks_exact(channels) {
            let value = frame[1] as u32;
            if value == 0 {
                continue;
            }
            frames += 1;
            if frame[0] != word(value) {
                wrong += 1;
            }
            if let Some(last) = last
                && value != last + 1
                && !received.flags.contains(Flags::GAP)
                && late_out == 0
            {
                jumps += 1;
            }
            last = Some(value);
        }
    }
    if frames == 0 {
        return Err("none of the pattern came in".into());
    }
    if wrong > 0 {
        return Err(format!("{wrong} of {frames} frames came in changed"));
    }
    if jumps > 0 {
        return Err(format!(
            "the pattern jumped {jumps} times with no frames missed or left out"
        ));
    }
    println!(
        "ok: {frames} frames came in word for word ({} left out late, {} missed, {} lost to overruns)",
        late_out, in_stats.missed, taken.overruns
    );

    // Each block's presentation time as it went out: the pattern frame
    // at a block's start was presented, by the talker's records, then.
    let by_frame: BTreeMap<u64, &Sent> = sent.iter().map(|block| (block.frame, block)).collect();
    let presented_out = |frame: u64| {
        let (_, block) = by_frame.range(..=frame).next_back()?;
        if frame >= block.frame + block.frames as u64 {
            return None;
        }
        block
            .presented
            .map(|presented| presented + (frame - block.frame) as i64 * 1_000_000_000 / RATE)
    };
    let ring_frame = |value: u32| {
        let (first, frame) = placed.range(..=value).next_back()?;
        (value < first + 48).then(|| frame + u64::from(value - first))
    };
    let (mut matched, mut differ) = (0u64, 0u64);
    for (received, samples) in &taken.blocks {
        let value = samples[1] as u32;
        let (Some(presented), Some(frame)) = (received.presented, ring_frame(value)) else {
            continue;
        };
        if value == 0 {
            continue;
        }
        match presented_out(frame) {
            Some(went) if went == presented => matched += 1,
            Some(_) => differ += 1,
            None => {}
        }
    }
    if matched == 0 || differ > 0 {
        return Err(format!(
            "{matched} blocks came presented as they went, {differ} otherwise"
        ));
    }
    println!("ok: {matched} blocks came presented when they went out to be");

    // The flush: silence from the block after the cursor then.
    let cut_at = presented_out(cut.frame + 6).ok_or("no record of the block after the flush")?;
    let after: u64 = taken
        .blocks
        .iter()
        .filter(|(received, _)| {
            received
                .presented
                .is_some_and(|presented| presented >= cut_at)
        })
        .map(|(_, samples)| {
            samples
                .chunks_exact(channels)
                .filter(|frame| frame[1] != 0)
                .count() as u64
        })
        .sum();
    if after > 0 {
        return Err(format!("{after} pattern frames came in after the flush"));
    }
    let idle = sent
        .iter()
        .filter(|block| block.frame >= cut.frame + 6 && !block.flags.contains(Flags::LATE))
        .all(|block| block.flags.contains(Flags::IDLE) && !block.flags.contains(Flags::DRY));
    if !idle {
        return Err("the talker's records after the flush are not idle".into());
    }
    println!("ok: the flush silenced the stream from the next block on");
    println!(
        "ok: least margin in {:?} us, least queued out {:?} frames, {} dry runs, {} late in",
        in_stats.least_margin.map(|margin| margin / 1000),
        out_stats.least_queued,
        out_stats.dry_runs,
        in_stats.late
    );

    // Attaching again: the same rings, the streams going on as they were.
    drop((out, input));
    let (mut out, mut input) = rings(folder)?;
    let mut taken = Taken::default();
    let again = Instant::now();
    while again.elapsed() < Duration::from_millis(300) {
        input.wait(Some(Duration::from_millis(20))).ok();
        taken.drain(&mut input)?;
    }
    let mut sent = Vec::new();
    drain_sent(&mut out, &mut sent)?;
    if taken.blocks.is_empty() || input.generation() != generation {
        return Err(format!(
            "after attaching again: {} blocks, generation {} then {}",
            taken.blocks.len(),
            generation,
            input.generation()
        ));
    }
    println!(
        "ok: attached again, {} blocks in 300 ms, generation {generation} as it was",
        taken.blocks.len()
    );
    Ok(())
}
