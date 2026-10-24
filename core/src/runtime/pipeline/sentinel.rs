//! The collector's own reader on a dead edge.  An external cannot be asked
//! whether it wrote, only heard: the bytes already pending were owed to a
//! reader that left, and the first byte after them earns the kill.

use super::collect::{Event, Slot};
use std::io::Read;
use std::sync::Weak;

/// Bytes already in the edge's buffer when the reader ended.  A failed
/// snapshot counts as zero: nothing was proved pending.
#[cfg(unix)]
fn pending_bytes(reader: &os_pipe::PipeReader) -> u64 {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    // SAFETY: `reader` owns an open fd for the call's duration, and FIONREAD
    // writes one `c_int` through the pointer.
    let rc = unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &raw mut n) };
    if rc < 0 {
        0
    } else {
        u64::try_from(n).unwrap_or(0)
    }
}

#[cfg(windows)]
fn pending_bytes(reader: &os_pipe::PipeReader) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut avail: u32 = 0;
    // SAFETY: `reader` owns an open handle for the call's duration; the null
    // arguments are the documented "do not copy, do not report" form, and
    // `avail` is the only out-parameter written.
    let ok = unsafe {
        PeekNamedPipe(
            reader.as_raw_handle().cast(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &raw mut avail,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { 0 } else { u64::from(avail) }
}

/// The read end through its hold: once the hold is dropped, the next read is
/// end-of-file, and the fd closes with it.
struct WhileHeld(Weak<os_pipe::PipeReader>);

impl Read for WhileHeld {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.upgrade().map_or(Ok(0), |r| (&*r).read(buf))
    }
}

/// Discard the bytes owed to the departed reader, hear the next write, then
/// drain the rest, so no writer blocks on the edge — not even one the kill
/// does not reach, a shielded cleanup's command.  Only the
/// [`HeldEdge`](super::route::HeldEdge) keeps the read end open.
pub(super) fn listen(hold: Weak<os_pipe::PipeReader>, slot: Slot) {
    std::thread::spawn(move || {
        let owed = hold.upgrade().map_or(0, |r| pending_bytes(&r));
        let mut edge = WhileHeld(hold);
        if std::io::copy(&mut edge.by_ref().take(owed), &mut std::io::sink()).is_err() {
            return;
        }
        // `UnexpectedEof`: no writer or no hold is left, so nothing to cut.
        if edge.read_exact(&mut [0u8; 1]).is_ok() {
            slot.send(Event::Wrote(slot.ix));
            // Past the cue it reports nothing, so it keeps no sender.
            drop(slot);
            let _ = std::io::copy(&mut edge, &mut std::io::sink());
        }
    });
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::super::collect::{Event, Slot};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Past its cue the sentinel drains while the hold lives, so a writer a
    /// pipe's worth ahead lands; once the hold drops, the edge closes at the
    /// drain's next read.
    #[test]
    fn the_sentinel_drains_past_its_cue_until_the_hold_is_dropped() {
        let (r, w) = crate::process::cloexec_pipe().expect("a pipe");
        let hold = Arc::new(r);
        let (tx, rx) = std::sync::mpsc::channel();
        super::listen(Arc::downgrade(&hold), Slot { ix: 0, tx });
        // A stream, not one byte: whatever the snapshot counts as owed, a
        // byte follows it.
        let (done, landed) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _ = done.send((&w).write_all(&vec![0u8; 1 << 20]).is_ok());
            w
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Event::Wrote(0))
        ));
        assert_eq!(
            landed.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a write past the cue blocked"
        );
        let w = writer.join().expect("the writer");
        drop(hold);
        let deadline = Instant::now() + Duration::from_secs(5);
        let closed = loop {
            match (&w).write(b"y") {
                Ok(_) => assert!(Instant::now() < deadline, "the edge outlived its hold"),
                Err(e) => break e,
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(closed.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
