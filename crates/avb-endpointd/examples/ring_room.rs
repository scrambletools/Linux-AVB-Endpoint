//! A simulated room for an app's tests, in scripts/smoke/room.sh: the
//! second daemon's "Room" entity plays a WAV file into its output's ring,
//! which streams to the app's entity, and records what its input's ring
//! takes from the app's entity into another, until a stop file appears.
//!
//!   ring_room <sockets' folder> <play.wav> <record.wav> <stop file>
//!
//! The WAV played may be 16, 24 or 32-bit integer PCM, of any channels:
//! its first channels go to the ring's first, silence past them. It
//! should be at the stream's rate, as nothing here resamples. The one
//! recorded is 32-bit integer PCM of the ring's channels, every word as
//! it came; beside it, `<record.wav>.times` lists the first block's
//! presentation time and each block that came late, after a gap, or with
//! a restart.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use avb_endpointd::ring::{App, Error, Flags, Inbound, Outbound, StreamRef};

const ROOM: StreamRef = StreamRef {
    instance: 1,
    index: 0,
};

/// A WAV file's samples as 32-bit words, its channels and rate.
struct Wav {
    words: Vec<i32>,
    channels: usize,
    rate: u32,
}

fn read_wav(path: &Path) -> io::Result<Wav> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }
    let (mut format, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => format = Some(body.to_vec()),
            b"data" => data = Some(body.to_vec()),
            _ => {}
        }
        at += 8 + len + len % 2;
    }
    let format = format.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    let kind = u16::from_le_bytes([format[0], format[1]]);
    let channels = usize::from(u16::from_le_bytes([format[2], format[3]]));
    let rate = u32::from_le_bytes(format[4..8].try_into().unwrap());
    let bits = u16::from_le_bytes([format[14], format[15]]);
    // PCM, or WAVE_FORMAT_EXTENSIBLE holding PCM.
    if (kind != 1 && kind != 0xfffe) || channels == 0 {
        return Err(bad("not integer PCM"));
    }
    let words = match bits {
        16 => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| i32::from(i16::from_le_bytes(*sample)) << 16)
            .collect(),
        24 => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|[low, middle, high]| i32::from_le_bytes([0, *low, *middle, *high]))
            .collect(),
        32 => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| i32::from_le_bytes(*sample))
            .collect(),
        _ => return Err(bad("not 16, 24 or 32-bit samples")),
    };
    Ok(Wav {
        words,
        channels,
        rate,
    })
}

/// A 32-bit integer PCM WAV file being written, its lengths set as it
/// closes.
struct WavWriter {
    file: BufWriter<File>,
    frames: u32,
    channels: u16,
}

impl WavWriter {
    fn create(path: &Path, channels: u16, rate: u32) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let block = channels * 4;
        file.write_all(b"RIFF\0\0\0\0WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&(rate * u32::from(block)).to_le_bytes())?;
        file.write_all(&block.to_le_bytes())?;
        file.write_all(&32u16.to_le_bytes())?;
        file.write_all(b"data\0\0\0\0")?;
        Ok(WavWriter {
            file,
            frames: 0,
            channels,
        })
    }

    fn write(&mut self, words: &[i32]) -> io::Result<()> {
        for word in words {
            self.file.write_all(&word.to_le_bytes())?;
        }
        self.frames += (words.len() / usize::from(self.channels)) as u32;
        Ok(())
    }

    fn close(self) -> io::Result<()> {
        let data = self.frames * u32::from(self.channels) * 4;
        let mut file = self.file.into_inner().map_err(|error| error.into_error())?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&(36 + data).to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&data.to_le_bytes())?;
        Ok(())
    }
}

/// Tries `attach` until it works, 30 s at most, as the daemons start.
fn attached<T>(attach: impl Fn() -> Result<T, Error>) -> io::Result<T> {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        match attach() {
            Ok(ring) => return Ok(ring),
            Err(error) if Instant::now() > until => {
                return Err(io::Error::other(error.to_string()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let [folder, play, record, stop] = args.as_slice() else {
        eprintln!("ring_room <sockets' folder> <play.wav> <record.wav> <stop file>");
        std::process::exit(2);
    };
    let wav = read_wav(play)?;
    let socket = folder.join("v1.app");
    let mut out: Outbound = attached(|| App::connect_at(&socket)?.outbound(ROOM))?;
    let mut input: Inbound = attached(|| App::connect_at(&socket)?.inbound(ROOM))?;
    out.wake_every(48);
    input.wake_every(48);
    let channels = out.channels();
    if input.rate() != 0 && wav.rate != input.rate() {
        eprintln!(
            "ring_room: {} is at {} Hz, the stream at {}; nothing resamples",
            play.display(),
            wav.rate,
            input.rate()
        );
    }

    let mut recording = WavWriter::create(record, input.channels() as u16, wav.rate)?;
    let mut times = BufWriter::new(File::create(format!("{}.times", record.display()))?);
    writeln!(times, "# frame presented_ns flags missed")?;
    let mut samples = Vec::new();
    let mut first = true;
    let mut played = 0usize;
    let mut block = Vec::new();
    while !stop.exists() {
        // Play: 20 ms kept queued, the file's channels into the ring's.
        while played < wav.words.len() / wav.channels && out.queued() < 960 {
            block.clear();
            let end = (played + 96).min(wav.words.len() / wav.channels);
            for frame in played..end {
                let source = &wav.words[frame * wav.channels..(frame + 1) * wav.channels];
                for channel in 0..channels {
                    block.push(source.get(channel).copied().unwrap_or(0));
                }
            }
            out.write(&block)
                .map_err(|error| io::Error::other(error.to_string()))?;
            played = end;
            if played == wav.words.len() / wav.channels {
                out.finish();
            }
        }
        while out.sent().ok().flatten().is_some() {}
        // Record.
        input.wait(Some(Duration::from_millis(5)))?;
        loop {
            match input.next(&mut samples) {
                Ok(Some(received)) => {
                    let marked = Flags::LATE | Flags::GAP | Flags::RESTART | Flags::NO_TIME;
                    if first || received.flags.0 & marked.0 != 0 {
                        writeln!(
                            times,
                            "{} {} {:#06x} {}",
                            received.frame,
                            received
                                .presented
                                .map_or("-".into(), |time| time.to_string()),
                            received.flags.0,
                            received.missed
                        )?;
                        first = false;
                    }
                    recording.write(&samples)?;
                }
                Ok(None) => break,
                Err(Error::Overrun { lost_frames }) => {
                    writeln!(times, "# {lost_frames} frames lost to an overrun")?;
                }
                Err(error) => return Err(io::Error::other(error.to_string())),
            }
        }
    }
    recording.close()?;
    times.flush()?;
    println!(
        "ring_room: played {played} frames, recorded to {}",
        record.display()
    );
    Ok(())
}
