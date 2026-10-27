//! Frames use the `guest_abi::packet` codec spoken by `ral-guest`'s pump threads.
//! A full receive queue drops packets; transmit blocks for backpressure.
//! Shutdown through a cloned wire handle wakes a blocked reader.

use std::io;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use ral_core::protocol::channel::WireStream;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::pipe::Wake;

const INBOX_CAP: usize = 256;

pub struct TunDevice {
    inbox: mpsc::Receiver<Vec<u8>>,
    write: WireStream,
    /// Sticky: a failed write or exited reader ends the session.
    failed: Arc<AtomicBool>,
}

impl TunDevice {
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

pub struct Stop {
    wire: WireStream,
    reader: thread::JoinHandle<()>,
}

impl Stop {
    pub fn stop(&self) {
        let _ = self.wire.shutdown(Shutdown::Both);
    }

    /// # Panics
    /// If the reader thread panicked.
    pub fn join(self) {
        self.reader.join().expect("netgate reader thread panicked");
    }
}

/// # Errors
/// Reports wire cloning or reader-thread startup failure.
pub fn spawn(wire: WireStream, wake: Wake) -> io::Result<(TunDevice, Stop)> {
    let write_half = wire.try_clone()?;
    let stop_handle = wire.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(INBOX_CAP);
    let reader = thread::Builder::new()
        .name("netgate-reader".to_string())
        .spawn(move || reader_loop(wire, tx, &wake))
        .map_err(|e| io::Error::other(format!("could not start the netgate reader: {e}")))?;
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

fn reader_loop(mut r: WireStream, tx: mpsc::SyncSender<Vec<u8>>, wake: &Wake) {
    while let Ok(frame) = guest_abi::packet::read_frame(&mut r) {
        let _ = tx.try_send(frame);
        wake();
    }
    // Disconnect before waking the core so it observes the dead wire.
    drop(tx);
    wake();
}

impl Device for TunDevice {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = match self.inbox.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::TryRecvError::Empty) => return None,
            // Receive must detect a dead wire even when no output is pending.
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
        caps.max_transmission_unit = guest_abi::link::MTU;
        caps
    }
}

pub struct RxToken(Vec<u8>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

pub struct TxToken<'a> {
    write: &'a mut WireStream,
    failed: &'a AtomicBool,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        if guest_abi::packet::write_frame(self.write, &buf).is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use guest_abi::packet;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Loss is allowed; corruption, reordering and reader-induced blocking are not.
    #[test]
    fn ten_thousand_frames_round_trip_through_the_reader() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        let writer = thread::spawn(move || {
            for i in 0..10_000u32 {
                let frame = i.to_le_bytes().to_vec();
                packet::write_frame(&mut b, &frame).unwrap();
            }
        });
        writer.join().unwrap();

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
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        b.write_all(&(u32::try_from(guest_abi::link::MTU).unwrap() + 1).to_le_bytes())
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(device.receive(Instant::from_millis(0)).is_none());
        b.write_all(&5u32.to_le_bytes()).unwrap();
        let _ = b.write_all(b"hello");
        thread::sleep(Duration::from_millis(50));
        assert!(device.receive(Instant::from_millis(0)).is_none());
    }

    #[test]
    fn stop_unsticks_a_reader_blocked_on_an_empty_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        let (device, stop) = spawn(a, Arc::new(|| {})).unwrap();
        stop.stop();
        drop(device);
    }

    #[test]
    fn a_full_inbox_drops_frames_instead_of_blocking_the_writer() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (_device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
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
