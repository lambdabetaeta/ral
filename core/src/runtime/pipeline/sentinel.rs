//! The collector's own reader on a dead edge.
//!
//! An external stage cannot be asked whether it wrote; it can only be heard.
//! So the parent reads the edge itself: the bytes already pending were owed
//! to a reader that left, and the first byte after them is a write to a dead
//! edge — which the collector answers with the kill.

use super::collect::Event;
use std::io::Read;
use std::sync::mpsc::Sender;

/// Bytes already in the edge's buffer when the reader ended.  A failed
/// snapshot counts as zero: nothing was proved pending.
#[cfg(unix)]
fn pending_bytes(reader: &os_pipe::PipeReader) -> usize {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    // SAFETY: `reader` owns an open fd for the call's duration, and FIONREAD
    // writes one `c_int` through the pointer.
    let rc = unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &raw mut n) };
    if rc < 0 {
        0
    } else {
        usize::try_from(n).unwrap_or(0)
    }
}

#[cfg(windows)]
fn pending_bytes(reader: &os_pipe::PipeReader) -> usize {
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
    if ok == 0 { 0 } else { avail as usize }
}

/// Discard the bytes owed to the departed reader, then hear the next write.
/// The reader travels back to the collector with the news, so it is released
/// at the writer's filing and not before.
pub(super) fn listen(reader: os_pipe::PipeReader, ix: usize, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8 * 1024];
        let mut owed = pending_bytes(&reader);
        while owed > 0 {
            let want = owed.min(buf.len());
            match reader.read(&mut buf[..want]) {
                Ok(0) | Err(_) => return,
                Ok(n) => owed -= n,
            }
        }
        // EOF here means no writer is left, so there is nothing to cut.
        if let Ok(n) = reader.read(&mut buf)
            && n > 0
        {
            let _ = tx.send(Event::Wrote(ix, reader));
        }
    });
}
