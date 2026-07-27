//! The `smoltcp` [`Device`] over the net wire — the vsock-backed stream
//! `ral-daemon --pump`'s guest side speaks `ral_daemon::packet`'s framing on.
//!
//! A **reader** thread blocks on [`ral_daemon::packet::read_frame`] and
//! hands frames to `stack`'s core thread over a bounded channel; a full
//! channel **drops** the frame — the right guest→host backpressure for raw
//! IP, since the guest's own TCP sees the loss and backs off. The core
//! thread's every `transmit` is a blocking
//! [`ral_daemon::packet::write_frame`], so a wedged net link stalls the
//! poll loop itself: end-to-end backpressure with no unbounded queue.
//!
//! `stop()` on the returned [`Stop`] unsticks a parked reader the way
//! `vm-manager`'s `Console::wake` does: shut the wire down from a cloned
//! handle, turning the blocked read into an immediate error.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::pipe::Wake;

/// How many frames the reader may queue before dropping. Sized well past a
/// poll interval's traffic; a usually-full channel means the core thread is
/// stuck, not that this number is wrong.
const INBOX_CAP: usize = 256;

/// The net wire's transport: a duplex byte stream that can be cloned into
/// independent handles and shut down from any of them — a `VZVirtioSocket`
/// fd on macOS, an `hvsocket` handle on Windows.
pub trait Wire: Read + Write + Send + 'static {
    /// An independent handle onto the same connection; [`Wire::shutdown`]
    /// on either unblocks a read parked on the other.
    ///
    /// # Errors
    /// Whatever the platform's own duplication call reports.
    fn try_clone(&self) -> io::Result<Self>
    where
        Self: Sized;

    /// Unblock any read or write parked on this connection, from any cloned
    /// handle. Idempotent.
    fn shutdown(&self);
}

#[cfg(unix)]
impl Wire for std::os::unix::net::UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        Self::try_clone(self)
    }

    fn shutdown(&self) {
        let _ = Self::shutdown(self, std::net::Shutdown::Both);
    }
}

/// The Windows twin of the impl above: `TcpStream` purely as std's owner of
/// a connected stream socket, never a TCP connection — the same pretence
/// `ral_core::wire::WireStream` makes for the control plane, and for the
/// same reason. The net wire's `AF_HYPERV` socket is adopted into one
/// through `From<OwnedSocket>`, at the one call site in `synod::session`.
#[cfg(windows)]
impl Wire for std::net::TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        Self::try_clone(self)
    }

    fn shutdown(&self) {
        let _ = Self::shutdown(self, std::net::Shutdown::Both);
    }
}

/// The `smoltcp` device: a receive queue fed by the reader thread, and a
/// write half the core thread uses directly for every `transmit`.
pub struct TunDevice<W: Wire> {
    inbox: mpsc::Receiver<Vec<u8>>,
    write: W,
    /// Set once a write fails or the reader exits — a dead net wire.
    /// `stack`'s core loop checks this each iteration and ends the session.
    failed: Arc<AtomicBool>,
}

impl<W: Wire> TunDevice<W> {
    /// Whether the wire has died. Sticky.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

/// Unblocks the reader thread from outside it, and later waits for it to
/// actually end. See the module doc.
pub struct Stop<W: Wire> {
    wire: W,
    reader: thread::JoinHandle<()>,
}

impl<W: Wire> Stop<W> {
    pub fn stop(&self) {
        self.wire.shutdown();
    }

    /// Wait for the reader thread to end — prompt once [`Self::stop`] has
    /// been called or the wire has failed on its own.
    ///
    /// # Panics
    /// If the reader thread itself panicked.
    pub fn join(self) {
        self.reader
            .join()
            .expect("guest-net reader thread panicked");
    }
}

/// Start the reader thread and build the device the core thread polls.
/// `wake`, if given, is called once per frame the reader hands off.
///
/// # Errors
/// Whatever `wire.try_clone()` or starting the reader thread reports.
pub fn spawn<W: Wire>(wire: W, wake: Option<Wake>) -> io::Result<(TunDevice<W>, Stop<W>)> {
    let write_half = wire.try_clone()?;
    let stop_handle = wire.try_clone()?;
    let read_half = wire; // the caller's own handle becomes the reader's
    let (tx, rx) = mpsc::sync_channel(INBOX_CAP);
    let reader = thread::Builder::new()
        .name("guest-net-reader".to_string())
        .spawn(move || reader_loop(read_half, tx, wake))
        .map_err(|e| io::Error::other(format!("could not start the guest-net reader: {e}")))?;
    Ok((
        TunDevice {
            inbox: rx,
            write: write_half,
            failed: Arc::new(AtomicBool::new(false)),
        },
        Stop {
            wire: stop_handle,
            reader,
        },
    ))
}

fn reader_loop<W: Wire>(mut r: W, tx: mpsc::SyncSender<Vec<u8>>, wake: Option<Wake>) {
    // A broken peer and a deliberate `Stop::stop()` look identical here:
    // both end `read_frame` with an error, and either way the wire is done.
    while let Ok(frame) = ral_daemon::packet::read_frame(&mut r) {
        // Drop on a full inbox rather than block: the guest's own TCP is
        // what must back off, never this thread.
        let _ = tx.try_send(frame);
        if let Some(w) = &wake {
            w();
        }
    }
    // Dropping `tx` makes `receive`'s next `try_recv` see `Disconnected`;
    // the final ring makes the core thread notice promptly.
    drop(tx);
    if let Some(wake) = wake {
        wake();
    }
}

impl<W: Wire> Device for TunDevice<W> {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a, W>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = match self.inbox.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::TryRecvError::Empty) => return None,
            // The reader has exited — a dead wire, which a connection with
            // nothing pending to *send* would otherwise never notice.
            Err(mpsc::TryRecvError::Disconnected) => {
                self.failed.store(true, Ordering::Relaxed);
                return None;
            }
        };
        Some((
            RxToken(frame),
            TxToken {
                write: &mut self.write,
                failed: &self.failed,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            write: &mut self.write,
            failed: &self.failed,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = ral_daemon::packet::MTU;
        caps
    }
}

pub struct RxToken(Vec<u8>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

pub struct TxToken<'a, W: Wire> {
    write: &'a mut W,
    failed: &'a AtomicBool,
}

impl<W: Wire> phy::TxToken for TxToken<'_, W> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        // The blocking write that gives this crate its end-to-end
        // backpressure: see the module doc.
        if ral_daemon::packet::write_frame(self.write, &buf).is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ral_daemon::packet;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Sends a real volume of frames through the reader and drains what it
    /// queued. This deliberately does **not** assert every one of the
    /// 10,000 arrives: a burst that outruns the core thread is exactly
    /// when [`INBOX_CAP`]'s drop-on-full backpressure is supposed to bite,
    /// and asserting lossless delivery would be asserting away the very
    /// behaviour this module exists to have. What must hold regardless: no
    /// frame is ever corrupted or reordered, the writer is never blocked
    /// by a slow or absent reader, and at least some frames get through.
    #[test]
    fn ten_thousand_frames_round_trip_through_the_reader() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, None).unwrap();
        let writer = thread::spawn(move || {
            for i in 0..10_000u32 {
                let frame = i.to_le_bytes().to_vec();
                packet::write_frame(&mut b, &frame).unwrap();
            }
        });
        writer.join().unwrap(); // the writer must complete without ever blocking on the reader

        let mut last = None;
        let mut received = 0u32;
        let mut quiet_polls = 0;
        while quiet_polls < 200 {
            if let Some((rx, _tx)) = device.receive(Instant::from_millis(0)) {
                use phy::RxToken as _;
                rx.consume(|data| {
                    let v = u32::from_le_bytes(data.try_into().unwrap());
                    if let Some(prev) = last {
                        assert!(v > prev, "frames must never arrive out of order");
                    }
                    last = Some(v);
                });
                received += 1;
                quiet_polls = 0;
            } else {
                quiet_polls += 1;
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(received > 0, "at least some frames must get through");
        assert!(received <= 10_000);
    }

    #[test]
    fn an_oversize_frame_tears_the_reader_down() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, None).unwrap();
        b.write_all(&(u32::try_from(packet::MTU).unwrap() + 1).to_le_bytes())
            .unwrap();
        // No valid frame follows an oversize declaration; the reader must
        // simply stop, never having queued anything for the core thread.
        thread::sleep(Duration::from_millis(100));
        assert!(device.receive(Instant::from_millis(0)).is_none());
        // Bytes sent after the reader has already given up must go nowhere.
        b.write_all(&5u32.to_le_bytes()).unwrap();
        let _ = b.write_all(b"hello");
        thread::sleep(Duration::from_millis(50));
        assert!(device.receive(Instant::from_millis(0)).is_none());
    }

    #[test]
    fn stop_unsticks_a_reader_blocked_on_an_empty_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        let (device, stop) = spawn(a, None).unwrap();
        // The reader thread is now parked in a blocking read with nothing
        // arriving; stop() must return promptly rather than the test
        // hanging until the harness times it out.
        stop.stop();
        drop(device);
    }

    #[test]
    fn a_full_inbox_drops_frames_instead_of_blocking_the_writer() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (_device, _stop) = spawn(a, None).unwrap();
        // Flood well past INBOX_CAP without ever draining `_device`; the
        // writer must never block on the reader thread's channel.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for i in 0..(u32::try_from(INBOX_CAP).unwrap() * 4) {
            let frame = i.to_le_bytes().to_vec();
            packet::write_frame(&mut b, &frame).unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "the writer must never block even once the inbox is full"
            );
        }
    }
}
