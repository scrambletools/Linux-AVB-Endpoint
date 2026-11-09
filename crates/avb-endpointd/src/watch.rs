//! Watches the configuration file's folder with inotify, so the daemon
//! reads a change at once, even one written within the same tick of the
//! file's modification time as the one before, which a look at that time
//! misses. The daemon still looks every two seconds, where the folder
//! cannot be watched.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

/// How long the thread waits for an event before it looks whether to
/// stop, in milliseconds.
const LOOK: i32 = 500;

/// The watch on a file, ended when dropped.
pub struct Watch {
    stop: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sends what `input` makes each time the file at `path` is written,
/// replaced, made or removed, until the watch is dropped or nothing
/// receives: `None` where its folder cannot be watched.
pub fn watch<T: Send + 'static>(path: &Path, sender: Sender<T>, input: fn() -> T) -> Option<Watch> {
    let folder = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())?;
    let name = path.file_name()?.to_os_string();
    let events = open(folder)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 4096];
            while !stopped.load(Ordering::Relaxed) {
                let Some(read) = wait_and_read(&events, &mut buffer) else {
                    continue;
                };
                if names(&buffer[..read]).any(|named| named == name.as_bytes())
                    && sender.send(input()).is_err()
                {
                    return;
                }
            }
        })
        .ok()?;
    Some(Watch { stop })
}

/// An inotify descriptor watching `folder` for files written, moved in or
/// out, made or removed.
#[allow(unsafe_code)]
fn open(folder: &Path) -> Option<OwnedFd> {
    let folder = CString::new(folder.as_os_str().as_bytes()).ok()?;
    // SAFETY: inotify_init1 takes flags alone and returns a new descriptor
    // or -1.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a descriptor just opened, owned by nothing else.
    let events = unsafe { OwnedFd::from_raw_fd(fd) };
    let mask = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_CREATE
        | libc::IN_DELETE;
    // SAFETY: the descriptor is open and the path a valid C string that
    // outlives the call.
    let watched = unsafe { libc::inotify_add_watch(events.as_raw_fd(), folder.as_ptr(), mask) };
    (watched >= 0).then_some(events)
}

/// Waits up to [`LOOK`] for events and reads them into `buffer`: how many
/// bytes, or `None` where none came.
#[allow(unsafe_code)]
fn wait_and_read(events: &OwnedFd, buffer: &mut [u8]) -> Option<usize> {
    let mut poll = libc::pollfd {
        fd: events.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, valid for the call.
    let ready = unsafe { libc::poll(&mut poll, 1, LOOK) };
    if ready <= 0 {
        return None;
    }
    // SAFETY: the buffer is valid for writes of its whole length.
    let read = unsafe { libc::read(events.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
    usize::try_from(read).ok().filter(|read| *read > 0)
}

/// The names in a read of inotify events, each a header of four 32-bit
/// fields, the last the length of the name after it, padded with NULs.
fn names(events: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let header = events.get(at..at + 16)?;
        let length = u32::from_ne_bytes(header[12..16].try_into().ok()?) as usize;
        let name = events.get(at + 16..at + 16 + length)?;
        at += 16 + length;
        Some(
            &name[..name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name.len())],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn event(name: &str, padded: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; 12];
        bytes.extend_from_slice(&(padded as u32).to_ne_bytes());
        let mut field = name.as_bytes().to_vec();
        field.resize(padded, 0);
        bytes.extend_from_slice(&field);
        bytes
    }

    #[test]
    fn events_name_their_files() {
        let mut bytes = event("enp2s0.toml", 16);
        bytes.extend(event(".enp2s0.toml.tmp", 32));
        bytes.extend(event("", 0));
        let named: Vec<&[u8]> = names(&bytes).collect();
        assert_eq!(
            named,
            [&b"enp2s0.toml"[..], &b".enp2s0.toml.tmp"[..], &b""[..]]
        );
        // A read cut short ends the names rather than misreading them.
        assert_eq!(names(&bytes[..20]).count(), 0);
    }

    /// Replacing the file, as every write does, is heard at once; another
    /// file in the folder is not.
    #[test]
    fn a_replaced_file_is_heard() {
        let dir = std::env::temp_dir().join(format!("avb-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("enp2s0.toml");
        let (sender, heard) = mpsc::channel();
        let watch = watch(&path, sender, || ()).expect("watched");
        std::fs::write(dir.join("enp6s0.toml"), "other").unwrap();
        assert!(heard.recv_timeout(Duration::from_millis(300)).is_err());
        crate::store::write(&path, b"[[endpoint]]\nname = \"A\"\n").unwrap();
        assert!(heard.recv_timeout(Duration::from_secs(2)).is_ok());
        drop(watch);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
