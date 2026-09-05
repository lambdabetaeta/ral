//! The collector's own reader on a dead edge.  An external cannot be asked
//! whether it wrote, only heard: the bytes already pending were owed to a
//! reader that left, and the first byte after them earns the kill.

use super::collect::{Event, Slot};
use std::io::Read;
use std::sync::Arc;

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

/// Discard the bytes owed to the departed reader, then hear the next write.
/// The read end is shared with the [`HeldEdge`](super::route::HeldEdge), so the
/// fd closes once both the writer's filing and this thread have let go.
pub(super) fn listen(reader: Arc<os_pipe::PipeReader>, slot: Slot) {
    std::thread::spawn(move || {
        let mut r = &*reader;
        let owed = pending_bytes(&reader);
        if std::io::copy(&mut r.by_ref().take(owed), &mut std::io::sink()).is_err() {
            return;
        }
        // `UnexpectedEof` means no writer is left, so there is nothing to cut.
        if r.read_exact(&mut [0u8; 1]).is_ok() {
            slot.send(Event::Wrote(slot.ix));
        }
    });
}
