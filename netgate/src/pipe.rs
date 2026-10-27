//! Two bounded rings: workers block; the core never waits here.
//! A full ring leaves unaccepted bytes in the guest's TCP receive buffer.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use ral_core::sync::{CondvarExt as _, LockExt as _};

const RING_CAP: usize = 64 * 1024;

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
        let mut s = self.state.lock_ignore_poison();
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
        let mut s = self.state.lock_ignore_poison();
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
        let mut s = self.state.lock_ignore_poison();
        s.write_closed = true;
        drop(s);
        self.readable.notify_all();
    }

    fn close_read(&self) {
        let mut s = self.state.lock_ignore_poison();
        s.read_closed = true;
        drop(s);
        self.writable.notify_all();
    }

    fn is_write_closed_and_drained(&self) -> bool {
        let s = self.state.lock_ignore_poison();
        s.write_closed && s.buf.is_empty()
    }

    /// `Ok(0)` means EOF; absent `until` permits an unbounded wait.
    fn blocking_read(&self, out: &mut [u8], until: Option<Instant>) -> io::Result<usize> {
        let mut s = self.state.lock_ignore_poison();
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
                None => self.readable.wait_ignore_poison(s),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "read deadline elapsed",
                        ));
                    }
                    self.readable
                        .wait_timeout_ignore_poison(s, deadline - now)
                        .0
                }
            };
        }
    }

    /// Blocking write: waits for room, or reports
    /// [`io::ErrorKind::BrokenPipe`] once the reader is gone for good.
    fn blocking_write(&self, data: &[u8], wake: &Wake) -> io::Result<usize> {
        let mut s = self.state.lock_ignore_poison();
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
                wake();
                return Ok(n);
            }
            s = self.writable.wait_ignore_poison(s);
        }
    }
}

#[derive(Clone)]
pub(crate) struct PipeAbort {
    from_guest: Arc<Ring>,
    to_guest: Arc<Ring>,
}

impl PipeAbort {
    pub(crate) fn abort(&self) {
        self.to_guest.close_read();
        self.from_guest.close_write();
    }
}

/// The smoltcp poll loop must never block on this half.
pub struct CoreSide {
    from_guest: Arc<Ring>,
    to_guest: Arc<Ring>,
}

impl CoreSide {
    /// Leave unaccepted bytes in the smoltcp receive buffer; never discard them.
    pub fn push_from_guest(&self, data: &[u8]) -> usize {
        self.from_guest.try_write(data)
    }

    pub fn pull_for_guest(&self, out: &mut [u8]) -> usize {
        self.to_guest.try_read(out)
    }

    pub fn guest_closed(&self) {
        self.from_guest.close_write();
    }

    pub fn abandon(&self) {
        self.to_guest.close_read();
    }

    /// Output has ended and drained, independently of upload completion.
    #[must_use]
    pub fn output_finished(&self) -> bool {
        self.to_guest.is_write_closed_and_drained()
    }
}

impl Drop for CoreSide {
    /// Both rings must wake their blocked workers during teardown.
    fn drop(&mut self) {
        self.to_guest.close_read();
        self.from_guest.close_write();
    }
}

pub struct PipeReader {
    from_guest: Arc<Ring>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.from_guest.blocking_read(buf, None)
    }
}

impl PipeReader {
    /// # Errors
    /// Times out if the deadline passes without data or EOF.
    pub fn read_deadline(&self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.from_guest.blocking_read(buf, Some(deadline))
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.from_guest.close_read();
    }
}

pub struct PipeWriter {
    to_guest: Arc<Ring>,
    wake: Wake,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.to_guest.blocking_write(buf, &self.wake)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.to_guest.close_write();
        (self.wake)();
    }
}

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
    pub(crate) fn abort_handle(&self) -> PipeAbort {
        PipeAbort {
            from_guest: self.reader.from_guest.clone(),
            to_guest: self.writer.to_guest.clone(),
        }
    }

    /// # Errors
    /// Times out if the deadline passes without data or EOF.
    pub fn read_deadline(&self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.reader.read_deadline(buf, deadline)
    }

    /// Dropping either half closes only its own ring.
    #[must_use]
    pub fn split(self) -> (PipeReader, PipeWriter) {
        (self.reader, self.writer)
    }
}

#[must_use]
pub fn new_pipe(wake: Wake) -> (CoreSide, ProxyPipe) {
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
        let (core, mut worker) = new_pipe(Arc::new(|| {}));
        worker.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(core.pull_for_guest(&mut buf), 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn bytes_pushed_by_the_core_side_reach_the_worker() {
        let (core, mut worker) = new_pipe(Arc::new(|| {}));
        assert_eq!(core.push_from_guest(b"hi"), 2);
        let mut buf = [0u8; 2];
        assert_eq!(worker.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn a_full_ring_accepts_only_what_fits() {
        let (core, _worker) = new_pipe(Arc::new(|| {}));
        let data = vec![0u8; RING_CAP + 10];
        assert_eq!(core.push_from_guest(&data), RING_CAP);
        assert_eq!(core.push_from_guest(&data), 0);
    }

    #[test]
    fn guest_closed_then_drained_reads_as_eof() {
        let (core, mut worker) = new_pipe(Arc::new(|| {}));
        assert_eq!(core.push_from_guest(b"x"), 1);
        core.guest_closed();
        let mut buf = [0u8; 8];
        assert_eq!(worker.read(&mut buf).unwrap(), 1);
        assert_eq!(worker.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn dropping_the_worker_reports_broken_pipe_to_a_blocked_writer() {
        let (core, worker) = new_pipe(Arc::new(|| {}));
        drop(worker);
        assert!(core.output_finished());
    }

    #[test]
    fn a_write_blocked_on_a_full_ring_wakes_once_the_core_side_drains_it() {
        let (core, mut worker) = new_pipe(Arc::new(|| {}));
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
        let (_core, mut worker) = new_pipe(wake);
        worker.write_all(b"x").unwrap();
        assert!(rung.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn read_deadline_times_out_with_no_data() {
        let (_core, worker) = new_pipe(Arc::new(|| {}));
        let mut buf = [0u8; 8];
        let err = worker
            .read_deadline(&mut buf, Instant::now() + Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn read_deadline_returns_data_that_arrives_before_the_deadline() {
        let (core, worker) = new_pipe(Arc::new(|| {}));
        assert_eq!(core.push_from_guest(b"hi"), 2);
        let mut buf = [0u8; 8];
        let n = worker
            .read_deadline(&mut buf, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(&buf[..n], b"hi");
    }

    #[test]
    fn splitting_and_dropping_both_halves_closes_both_rings_like_an_undropped_pipe() {
        let (core, worker) = new_pipe(Arc::new(|| {}));
        let (reader, writer) = worker.split();
        drop(reader);
        drop(writer);
        assert!(core.output_finished());
    }

    #[test]
    fn dropping_the_core_side_wakes_a_worker_blocked_writing_on_a_full_ring() {
        let (core, mut worker) = new_pipe(Arc::new(|| {}));
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
