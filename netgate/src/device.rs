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

    fn wait_failed(device: &mut TunDevice) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !device.failed() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            let _ = device.receive(Instant::from_millis(0));
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// Below `INBOX_CAP`, nothing is dropped.
    #[test]
    fn frames_arrive_whole_and_in_order() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        let sent: Vec<u32> = (0..200).collect();
        for i in &sent {
            packet::write_frame(&mut b, &i.to_le_bytes()).unwrap();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < sent.len() {
            assert!(std::time::Instant::now() < deadline, "frames went missing");
            match device.receive(Instant::from_millis(0)) {
                Some((rx, _tx)) => {
                    use phy::RxToken as _;
                    rx.consume(|data| got.push(u32::from_le_bytes(data.try_into().unwrap())));
                }
                None => thread::sleep(Duration::from_millis(1)),
            }
        }
        assert_eq!(got, sent);
    }

    #[test]
    fn an_oversize_frame_fails_the_device() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        b.write_all(&(u32::try_from(guest_abi::link::MTU).unwrap() + 1).to_le_bytes())
            .unwrap();
        assert!(wait_failed(&mut device));
    }

    #[test]
    fn a_closed_wire_fails_the_device() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        drop(b);
        assert!(wait_failed(&mut device));
    }

    #[test]
    fn a_failed_transmit_fails_the_device() {
        use phy::TxToken as _;
        let (a, b) = UnixStream::pair().unwrap();
        let (mut device, _stop) = spawn(a, Arc::new(|| {})).unwrap();
        drop(b);
        assert!(!device.failed());
        device
            .transmit(Instant::from_millis(0))
            .unwrap()
            .consume(4, |buf| buf.fill(1));
        assert!(device.failed());
    }

    #[test]
    fn stop_unsticks_a_reader_blocked_on_an_empty_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        let (_device, stop) = spawn(a, Arc::new(|| {})).unwrap();
        stop.stop();
        let (done, exited) = mpsc::channel();
        thread::spawn(move || {
            stop.join();
            let _ = done.send(());
        });
        exited
            .recv_timeout(Duration::from_secs(5))
            .expect("the reader must exit once stopped");
    }
}
