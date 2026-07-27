//! The byte-level seam between the smoltcp world and a worker thread.
//!
//! Two 64 KiB rings, one per direction: the worker gets ordinary blocking
//! [`Read`]/[`Write`] while the core thread never blocks here.
//!
//! A full ring *is* the backpressure: [`CoreSide::push_from_guest`] leaves
//! unaccepted bytes in the smoltcp socket's own receive buffer (the guest's
//! TCP flow control does the rest), and a worker's [`ProxyPipe::write`]
//! simply blocks its own dedicated thread.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Each ring's capacity: a healthy TCP window's worth, small enough that a
/// wedged connection cannot hoard more than a moment's data.
const RING_CAP: usize = 64 * 1024;

/// Rung when bytes land in the worker→guest direction, so the core thread's
/// poll loop wakes promptly. `stack` owns the one doorbell every connection
/// and the frame reader share.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

struct RingState {
    buf: VecDeque<u8>,
    /// The writer is done: once drained, every further read is end-of-file.
    write_closed: bool,
    /// The reader is gone: a blocked writer must stop waiting for space.
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

    /// Non-blocking: take up to `out.len()` queued bytes. `0` means empty
    /// right now — EOF is [`Self::is_write_closed_and_drained`]'s to tell.
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

    /// Non-blocking: push as many of `data`'s bytes as currently fit; the
    /// caller keeps the rest — partial acceptance *is* the backpressure.
    fn try_write(&self, data: &[u8]) -> usize {
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

    /// Take what is queued, else wait; `Ok(0)` means end-of-write. `until`
    /// is `None` for an unbounded wait, `Some(deadline)` to time out with
    /// [`io::ErrorKind::TimedOut`].
    fn blocking_read(&self, out: &mut [u8], until: Option<Instant>) -> io::Result<usize> {
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
            s = match until {
                None => self.readable.wait(s).expect("ring poisoned"),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "read deadline elapsed",
                        ));
                    }
                    self.readable
                        .wait_timeout(s, deadline - now)
                        .expect("ring poisoned")
                        .0
                }
            };
        }
    }

    /// Blocking write: waits for room, or reports
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
    /// Hand guest bytes to the worker. A short (or zero) return means the
    /// ring is full: leave the remainder in the smoltcp socket's own
    /// receive buffer, never discard it.
    pub fn push_from_guest(&self, data: &[u8]) -> usize {
        self.from_guest.try_write(data)
    }

    /// Take bytes the worker has queued for the guest. Non-blocking.
    pub fn pull_for_guest(&self, out: &mut [u8]) -> usize {
        self.to_guest.try_read(out)
    }

    /// The guest's half of the TCP connection closed; the worker's next
    /// read sees end-of-file.
    pub fn guest_closed(&self) {
        self.from_guest.close_write();
    }

    /// Teardown from the smoltcp side — nothing the worker writes from
    /// here on will reach the guest.
    pub fn abandon(&self) {
        self.to_guest.close_read();
    }

    /// The worker is gone with nothing left queued — the core thread's cue
    /// to close its half of the TCP connection.
    #[must_use]
    pub fn worker_finished(&self) -> bool {
        self.to_guest.is_write_closed_and_drained()
    }
}

impl Drop for CoreSide {
    /// Close both rings, so a worker parked on either wakes rather than
    /// sleeps forever on a condvar nobody will signal again.
    fn drop(&mut self) {
        self.to_guest.close_read();
        self.from_guest.close_write();
    }
}

/// The worker's read half, after [`ProxyPipe::split`].
pub struct PipeReader {
    from_guest: Arc<Ring>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.from_guest.blocking_read(buf, None)
    }
}

impl PipeReader {
    /// A read bounded by `deadline` — used only while the CONNECT head is
    /// incomplete.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] if `deadline` passes with no data and no
    /// end-of-file.
    pub fn read_deadline(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.from_guest.blocking_read(buf, Some(deadline))
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.from_guest.close_read();
    }
}

/// The worker's write half, after [`ProxyPipe::split`].
pub struct PipeWriter {
    to_guest: Arc<Ring>,
    wake: Option<Wake>,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.to_guest.blocking_write(buf, self.wake.as_ref())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.to_guest.close_write();
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

/// The worker's handle: blocking [`Read`]/[`Write`] over both directions,
/// for the head-reading phase before [`Self::split`].
pub struct ProxyPipe {
    reader: PipeReader,
    writer: PipeWriter,
}

impl Read for ProxyPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for ProxyPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl ProxyPipe {
    /// See [`PipeReader::read_deadline`].
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] if `deadline` passes with no data and no
    /// end-of-file.
    pub fn read_deadline(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.reader.read_deadline(buf, deadline)
    }

    /// Split into independent halves so each direction can be copied
    /// concurrently. Dropping either half closes its own ring.
    #[must_use]
    pub fn split(self) -> (PipeReader, PipeWriter) {
        (self.reader, self.writer)
    }
}

/// A fresh connection's pipe: the core thread's non-blocking half and the
/// worker's blocking half. `wake`, if given, is rung whenever the worker
/// queues bytes for the guest or drops its end.
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
            reader: PipeReader { from_guest },
            writer: PipeWriter { to_guest, wake },
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
        assert_eq!(worker.read(&mut buf).unwrap(), 1);
        assert_eq!(worker.read(&mut buf).unwrap(), 0);
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

    #[test]
    fn read_deadline_times_out_with_no_data() {
        let (_core, mut worker) = new_pipe(None);
        let mut buf = [0u8; 8];
        let err = worker
            .read_deadline(&mut buf, Instant::now() + Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn read_deadline_returns_data_that_arrives_before_the_deadline() {
        let (core, mut worker) = new_pipe(None);
        assert_eq!(core.push_from_guest(b"hi"), 2);
        let mut buf = [0u8; 8];
        let n = worker
            .read_deadline(&mut buf, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(&buf[..n], b"hi");
    }

    #[test]
    fn splitting_and_dropping_both_halves_closes_both_rings_like_an_undropped_pipe() {
        let (core, worker) = new_pipe(None);
        let (reader, writer) = worker.split();
        drop(reader);
        drop(writer);
        assert!(core.worker_finished());
    }

    #[test]
    fn dropping_the_core_side_wakes_a_worker_blocked_writing_on_a_full_ring() {
        let (core, mut worker) = new_pipe(None);
        let filler = vec![0u8; RING_CAP];
        worker.write_all(&filler).unwrap();
        let handle = thread::spawn(move || worker.write_all(b"more"));
        thread::sleep(Duration::from_millis(50));
        drop(core);
        let result = handle.join().unwrap();
        assert!(
            result.is_err(),
            "a writer blocked on a full ring must wake to an error, not hang forever"
        );
    }
}
