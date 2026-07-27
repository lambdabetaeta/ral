//! The byte-level seam between the smoltcp world and a connection's worker
//! thread.
//!
//! `stack`'s core thread owns every [`smoltcp::socket::tcp::Socket`]; no
//! other thread ever touches one. Everything downstream of the accept gate
//! — TLS, HTTP, the intercepting proxy proper, all of it `proxy.rs`'s
//! business, not this module's — talks only to a [`ProxyPipe`]: two
//! independent 64 KiB byte rings, one per direction, so a worker thread can
//! use ordinary blocking [`Read`]/[`Write`] (which is what `rustls` and
//! `reqwest` both want) while the core thread never blocks on anything but
//! its own outbound net-wire writes.
//!
//! Bounded, not unbounded: a full ring *is* the backpressure signal, not a
//! problem to route around. When [`CoreSide::push_from_guest`] finds the
//! guest→worker ring full, the caller simply leaves the unaccepted bytes in
//! the smoltcp socket's own receive buffer for the next poll — the guest's
//! own TCP flow control does the rest, with no queue anywhere growing past
//! its cap. When a worker's [`ProxyPipe::write`] finds the worker→guest ring
//! full, it blocks, which costs nothing because a worker is a dedicated
//! thread with nothing else to do while it waits.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};

/// Each ring's capacity. 64 KiB is a healthy TCP window's worth: generous
/// enough that a worker's blocking read/write rarely stalls a live
/// connection, small enough that a wedged one cannot hoard more than a
/// moment's data in either direction.
const RING_CAP: usize = 64 * 1024;

/// Something to ring when bytes land in the worker→guest direction.
///
/// So the core thread's poll loop — parked between smoltcp events — wakes
/// promptly instead of waiting out its own timeout. Supplied by `stack`,
/// which owns the one doorbell every connection and the frame reader
/// share.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

struct RingState {
    buf: VecDeque<u8>,
    /// The writer is done: normal end of this direction, or its side of
    /// the connection was dropped. Once set and the buffer has drained,
    /// every further read reports end-of-file.
    write_closed: bool,
    /// The reader is gone. A writer, blocked now or in the future, must
    /// stop waiting for space that will never be taken.
    read_closed: bool,
}

struct Ring {
    state: Mutex<RingState>,
    readable: Condvar,
    writable: Condvar,
}

impl Ring {
    fn new() -> Self {
        Self {
            state: Mutex::new(RingState {
                buf: VecDeque::with_capacity(RING_CAP),
                write_closed: false,
                read_closed: false,
            }),
            readable: Condvar::new(),
            writable: Condvar::new(),
        }
    }

    /// Non-blocking: take up to `out.len()` queued bytes. Returns `0` if
    /// the ring is simply empty right now, indistinguishable here from
    /// end-of-file — callers on the non-blocking side track EOF themselves
    /// via [`Self::is_write_closed`].
    fn try_read(&self, out: &mut [u8]) -> usize {
        let mut s = self.state.lock().expect("ring poisoned");
        let n = s.buf.len().min(out.len());
        for slot in &mut out[..n] {
            *slot = s.buf.pop_front().expect("n bounded by buf.len()");
        }
        drop(s);
        if n > 0 {
            self.writable.notify_one();
        }
        n
    }

    /// Non-blocking: push as many of `data`'s bytes as currently fit.
    /// Returns how many were accepted; the caller keeps the rest — that
    /// partial acceptance *is* the guest→worker backpressure.
    fn try_write(&self, data: &[u8], wake: Option<&Wake>) -> usize {
        let mut s = self.state.lock().expect("ring poisoned");
        if s.write_closed || s.read_closed {
            return 0;
        }
        let room = RING_CAP - s.buf.len();
        let n = room.min(data.len());
        s.buf.extend(&data[..n]);
        drop(s);
        if n > 0 {
            self.readable.notify_one();
            if let Some(wake) = wake {
                wake();
            }
        }
        n
    }

    fn close_write(&self) {
        let mut s = self.state.lock().expect("ring poisoned");
        s.write_closed = true;
        drop(s);
        self.readable.notify_all();
    }

    fn close_read(&self) {
        let mut s = self.state.lock().expect("ring poisoned");
        s.read_closed = true;
        drop(s);
        self.writable.notify_all();
    }

    fn is_write_closed_and_drained(&self) -> bool {
        let s = self.state.lock().expect("ring poisoned");
        s.write_closed && s.buf.is_empty()
    }

    /// Blocking read for the worker side: waits for at least one byte, for
    /// end-of-write (returns `Ok(0)`), or wakes on a spurious signal and
    /// retries — never returns early with nothing to show for it.
    fn blocking_read(&self, out: &mut [u8]) -> io::Result<usize> {
        let mut s = self.state.lock().expect("ring poisoned");
        loop {
            if !s.buf.is_empty() {
                let n = s.buf.len().min(out.len());
                for slot in &mut out[..n] {
                    *slot = s.buf.pop_front().expect("n bounded by buf.len()");
                }
                drop(s);
                self.writable.notify_one();
                return Ok(n);
            }
            if s.write_closed {
                return Ok(0);
            }
            s = self.readable.wait(s).expect("ring poisoned");
        }
    }

    /// Blocking write for the worker side: waits for room, or reports
    /// [`io::ErrorKind::BrokenPipe`] once the reader is gone for good.
    fn blocking_write(&self, data: &[u8], wake: Option<&Wake>) -> io::Result<usize> {
        let mut s = self.state.lock().expect("ring poisoned");
        loop {
            if s.read_closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the guest side of this connection is gone",
                ));
            }
            let room = RING_CAP - s.buf.len();
            if room > 0 {
                let n = room.min(data.len());
                s.buf.extend(&data[..n]);
                drop(s);
                self.readable.notify_one();
                if let Some(wake) = wake {
                    wake();
                }
                return Ok(n);
            }
            s = self.writable.wait(s).expect("ring poisoned");
        }
    }
}

/// The core thread's handle on one connection's pipe — non-blocking only,
/// called from inside the smoltcp poll loop, where nothing may ever wait.
pub struct CoreSide {
    from_guest: Arc<Ring>,
    to_guest: Arc<Ring>,
}

impl CoreSide {
    /// Hand the core thread's own read of the guest's TCP socket to the
    /// worker. Returns how many bytes were accepted — less than
    /// `data.len()` (including zero) means the ring is full, and the
    /// caller must leave the remainder in the smoltcp socket's own receive
    /// buffer rather than discard it: that undrained buffer is the
    /// guest→worker backpressure.
    pub fn push_from_guest(&self, data: &[u8]) -> usize {
        self.from_guest.try_write(data, None)
    }

    /// Take bytes the worker has queued for the guest, to hand to the
    /// smoltcp socket's send buffer. Non-blocking; an empty return means
    /// nothing is waiting right now.
    pub fn pull_for_guest(&self, out: &mut [u8]) -> usize {
        self.to_guest.try_read(out)
    }

    /// The guest's half of the TCP connection closed (a FIN arrived, fully
    /// drained). No more bytes will ever come from it; the worker's next
    /// read, once it catches up, sees end-of-file.
    pub fn guest_closed(&self) {
        self.from_guest.close_write();
    }

    /// The connection is being torn down from the smoltcp side (RST, or the
    /// socket is being reclaimed) — nothing the worker writes from here on
    /// will ever reach the guest.
    pub fn abandon(&self) {
        self.to_guest.close_read();
    }

    /// The worker is gone and has nothing left queued for the guest — the
    /// core thread's cue to close its half of the TCP connection.
    #[must_use]
    pub fn worker_finished(&self) -> bool {
        self.to_guest.is_write_closed_and_drained()
    }
}

/// A connection worker's handle: ordinary blocking [`Read`]/[`Write`], so
/// TLS and HTTP code on this thread need not know smoltcp exists.
pub struct ProxyPipe {
    from_guest: Arc<Ring>,
    to_guest: Arc<Ring>,
    wake: Option<Wake>,
}

impl Read for ProxyPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.from_guest.blocking_read(buf)
    }
}

impl Write for ProxyPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.to_guest.blocking_write(buf, self.wake.as_ref())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ProxyPipe {
    fn drop(&mut self) {
        // The worker is gone: nothing further will arrive for the guest,
        // and nothing further it sends is worth reading.
        self.to_guest.close_write();
        self.from_guest.close_read();
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

/// A fresh connection's pipe, split into the core thread's non-blocking
/// half and the worker's blocking half.
///
/// `wake`, if given, is rung whenever the worker queues bytes for the
/// guest or drops its end, so the core thread's poll loop need not
/// busy-poll every pipe on a fixed tick.
#[must_use]
pub fn new_pipe(wake: Option<Wake>) -> (CoreSide, ProxyPipe) {
    let from_guest = Arc::new(Ring::new());
    let to_guest = Arc::new(Ring::new());
    (
        CoreSide {
            from_guest: from_guest.clone(),
            to_guest: to_guest.clone(),
        },
        ProxyPipe {
            from_guest,
            to_guest,
            wake,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn bytes_written_by_the_worker_reach_the_core_side() {
        let (core, mut worker) = new_pipe(None);
        worker.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        // try_read may need a moment to see what write_all just queued —
        // the two run on one thread here, so no real race, just proving
        // the non-blocking side observes what the blocking side wrote.
        assert_eq!(core.pull_for_guest(&mut buf), 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn bytes_pushed_by_the_core_side_reach_the_worker() {
        let (core, mut worker) = new_pipe(None);
        assert_eq!(core.push_from_guest(b"hi"), 2);
        let mut buf = [0u8; 2];
        assert_eq!(worker.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn a_full_ring_accepts_only_what_fits() {
        let (core, _worker) = new_pipe(None);
        let data = vec![0u8; RING_CAP + 10];
        assert_eq!(core.push_from_guest(&data), RING_CAP);
        assert_eq!(core.push_from_guest(&data), 0);
    }

    #[test]
    fn guest_closed_then_drained_reads_as_eof() {
        let (core, mut worker) = new_pipe(None);
        assert_eq!(core.push_from_guest(b"x"), 1);
        core.guest_closed();
        let mut buf = [0u8; 8];
        assert_eq!(worker.read(&mut buf).unwrap(), 1); // the queued byte first
        assert_eq!(worker.read(&mut buf).unwrap(), 0); // then EOF
    }

    #[test]
    fn dropping_the_worker_reports_broken_pipe_to_a_blocked_writer() {
        let (core, worker) = new_pipe(None);
        drop(worker);
        assert!(core.worker_finished());
    }

    #[test]
    fn a_write_blocked_on_a_full_ring_wakes_once_the_core_side_drains_it() {
        let (core, mut worker) = new_pipe(None);
        let filler = vec![0u8; RING_CAP];
        worker.write_all(&filler).unwrap();
        let handle = thread::spawn(move || worker.write_all(b"more"));
        thread::sleep(Duration::from_millis(50));
        let mut sink = vec![0u8; RING_CAP];
        assert_eq!(core.pull_for_guest(&mut sink), RING_CAP);
        handle.join().unwrap().unwrap();
        let mut tail = [0u8; 4];
        assert_eq!(core.pull_for_guest(&mut tail), 4);
        assert_eq!(&tail, b"more");
    }

    #[test]
    fn the_wake_callback_fires_when_the_worker_writes() {
        let rung = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rung2 = rung.clone();
        let wake: Wake = Arc::new(move || rung2.store(true, std::sync::atomic::Ordering::SeqCst));
        let (_core, mut worker) = new_pipe(Some(wake));
        worker.write_all(b"x").unwrap();
        assert!(rung.load(std::sync::atomic::Ordering::SeqCst));
    }
}
