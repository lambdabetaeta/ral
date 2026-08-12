//! Synod's [`exarch::agent::Hatchery`]: one accept-pump thread per
//! conversation over [`vm_manager::Machine::accept_agent`], sorting
//! siblings apart by the agent-port preamble's token.
//!
//! A helper's hatch answer and its dial cross independently — `hatch`'s
//! answer has to reach the guest and spawn a process before it can dial
//! home, while `agent-hatched` may be asked for immediately after — so a
//! dial may land before its awaiter registers, or after. [`Rendezvous`]
//! holds whichever side arrives first, keyed by token, and the other side
//! completes it.
//!
//! The pump never consumes a dial's preamble. It only *peeks* the sixteen
//! bytes — a non-destructive read, same syscall family as everything else
//! in this seam — to learn which hatch a dial answers; the untouched stream
//! crosses to `agent-hatched`, which reads and checks the preamble for
//! real ([`exarch::agent::Hatchery::await_dial`]'s contract: "preamble
//! bytes still on it"). A dial whose peeked bytes fail the magic is closed
//! and counted without ever reaching that check.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use exarch::agent::Hatchery;
use ral_core::wire::WireStream;
use vm_manager::{Machine, preamble};

/// How long the pump blocks in one `accept_agent` call before checking
/// whether it has been asked to stop — the pump's own polling grain, never
/// the patience a `` `hatch `` awaiter sees.
const POLL: Duration = Duration::from_millis(200);

/// How long the pump waits for a dial's sixteen preamble bytes to finish
/// arriving before giving up on it as a stray.
const PREAMBLE_GRACE: Duration = Duration::from_secs(2);

/// One token's rendezvous, whichever side got there first.
enum Slot {
    /// A dial arrived before `agent-hatched` asked for it.
    Dialed(WireStream),
    /// `agent-hatched` is already blocked in [`MachineHatchery::await_dial`],
    /// waiting on this end.
    Awaited(Sender<WireStream>),
}

/// `exarch::agent::Hatchery` over a real machine's agent port.
///
/// Owns the machine for as long as the conversation runs: [`Self::start`]
/// moves it into the pump thread, and [`Self::join`] stops the pump and
/// hands the machine back, the ordering `synod::session::Conversation::end`
/// needs (agent dropped, then the pump joined, then the machine shut down).
pub struct MachineHatchery {
    port: u32,
    rendezvous: Arc<Mutex<HashMap<u64, Slot>>>,
    stop: Arc<AtomicBool>,
    /// Dials whose preamble failed the magic: bounded noise the pump counts
    /// rather than parses further.
    refused: Arc<AtomicU64>,
    pump: Option<JoinHandle<Box<dyn Machine>>>,
}

impl MachineHatchery {
    /// Start the pump over `machine`'s agent port. `machine` is moved into
    /// the pump thread and recovered by [`Self::join`]; nothing else may
    /// touch it until then.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the pump thread at all — the same
    /// class of failure `std::thread::spawn` itself would propagate.
    #[must_use]
    pub fn start(machine: Box<dyn Machine>) -> Self {
        let rendezvous = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));

        let pump_rendezvous = Arc::clone(&rendezvous);
        let pump_stop = Arc::clone(&stop);
        let pump_refused = Arc::clone(&refused);
        let pump = std::thread::Builder::new()
            .name("synod-agent-pump".to_string())
            .spawn(move || pump_loop(machine, &pump_rendezvous, &pump_stop, &pump_refused))
            .expect("spawning the agent-port accept pump");

        Self {
            port: vm_manager::AGENT_PORT,
            rendezvous,
            stop,
            refused,
            pump: Some(pump),
        }
    }

    /// How many dials this pump has closed for failing the preamble's magic
    /// — a stray connection, never a hatched child.
    pub fn refused_dials(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Stop the pump and recover the machine it held.
    ///
    /// # Panics
    /// Panics if the pump thread itself panicked — a bug in the pump, not a
    /// condition this conversation can recover from.
    pub fn join(mut self) -> Box<dyn Machine> {
        self.stop.store(true, Ordering::Relaxed);
        self.pump
            .take()
            .expect("join called once, on the only handle")
            .join()
            .expect("the agent-port accept pump must not panic")
    }
}

impl Hatchery for MachineHatchery {
    fn await_dial(&self, token: u64, patience: Duration) -> Result<WireStream, String> {
        let already = lock(&self.rendezvous).remove(&token);
        if let Some(Slot::Dialed(stream)) = already {
            return Ok(stream);
        }

        let (tx, rx) = mpsc::channel();
        lock(&self.rendezvous).insert(token, Slot::Awaited(tx));

        if let Ok(stream) = rx.recv_timeout(patience) {
            return Ok(stream);
        }
        // Clear our own registration; a dial that races in at the very last
        // instant is simply dropped along with the sender — the same
        // honest timeout `agent-hatched` reports.
        lock(&self.rendezvous).remove(&token);
        Err(format!(
            "no dial bearing this token arrived within {patience:?}"
        ))
    }

    fn port(&self) -> u32 {
        self.port
    }
}

fn lock(m: &Mutex<HashMap<u64, Slot>>) -> std::sync::MutexGuard<'_, HashMap<u64, Slot>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The pump's body: accept dials until told to stop, sort each one by its
/// peeked preamble, and hand the machine back on the way out.
fn pump_loop(
    mut machine: Box<dyn Machine>,
    rendezvous: &Mutex<HashMap<u64, Slot>>,
    stop: &AtomicBool,
    refused: &AtomicU64,
) -> Box<dyn Machine> {
    while !stop.load(Ordering::Relaxed) {
        match machine.accept_agent(POLL) {
            Ok(dial) => route_dial(dial.into(), rendezvous, refused),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            // The machine's agent listener is gone, or this backend never
            // had one: either way nothing more will ever arrive here.
            Err(_) => break,
        }
    }
    machine
}

/// Peek `stream`'s preamble to learn its token, then either complete a
/// waiting `agent-hatched` or park the stream for one that hasn't asked
/// yet. A stray dial — bad magic, or one that never finishes arriving — is
/// closed and counted, never routed.
fn route_dial(stream: WireStream, rendezvous: &Mutex<HashMap<u64, Slot>>, refused: &AtomicU64) {
    let Ok(token) = peek_token(&stream) else {
        refused.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let removed = lock(rendezvous).remove(&token);
    let waiting = match removed {
        Some(Slot::Awaited(tx)) => Some(tx),
        Some(Slot::Dialed(_)) | None => None,
    };
    match waiting {
        Some(tx) => {
            // The awaiter may have timed out between our lookup and this
            // send; a dropped receiver just drops the stream, closing it.
            let _ = tx.send(stream);
        }
        None => {
            lock(rendezvous).insert(token, Slot::Dialed(stream));
        }
    }
}

/// Read `stream`'s sixteen preamble bytes without consuming them — repeated
/// non-destructive peeks, since one `recv(MSG_PEEK)` is not guaranteed to
/// see every byte the moment they start arriving.
///
/// # Errors
/// Returns a sentence if the preamble never finishes arriving within
/// [`PREAMBLE_GRACE`], or if it arrives but fails the magic.
fn peek_token(stream: &WireStream) -> Result<u64, String> {
    let mut buf = [0u8; preamble::LEN];
    let deadline = Instant::now() + PREAMBLE_GRACE;
    loop {
        let n = peek(stream, &mut buf)
            .map_err(|e| format!("agent-port dial: could not peek its preamble: {e}"))?;
        if n == buf.len() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("agent-port dial: its preamble never finished arriving".to_string());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    preamble::Preamble::decode(&buf)
        .map(|p| p.token)
        .map_err(|e| e.to_string())
}

/// Read from `stream` without consuming — one `recv(2)` with `MSG_PEEK` on
/// Unix, since `UnixStream::peek` is still an unstable library feature;
/// `TcpStream::peek` is stable, so Windows uses std's own.
///
/// # Errors
/// Returns the socket error.
#[cfg(unix)]
fn peek(stream: &WireStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: `buf` is a valid, uniquely-borrowed slice for the duration of
    // the call, and `MSG_PEEK` leaves the socket's receive buffer untouched.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_PEEK,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n.cast_unsigned())
    }
}

/// See the Unix twin above for the whole contract.
#[cfg(windows)]
fn peek(stream: &WireStream, buf: &mut [u8]) -> io::Result<usize> {
    stream.peek(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A fake `Machine` whose `accept_agent` serves dials from a channel —
    /// no real vsock listener needed to exercise the pump's own routing.
    struct FakeMachine {
        dials: std::sync::mpsc::Receiver<std::os::fd::OwnedFd>,
    }

    impl vm_manager::Machine for FakeMachine {
        fn workspace_path(&self) -> &std::path::Path {
            unimplemented!("not exercised by these tests")
        }
        fn take_wires(&mut self) -> vm_manager::Wires {
            unimplemented!("not exercised by these tests")
        }
        fn accept_agent(&mut self, patience: Duration) -> io::Result<vm_manager::AgentDial> {
            self.dials.recv_timeout(patience).map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "no dial within the patience given")
            })
        }
        fn shutdown(self: Box<Self>) -> Result<(), vm_manager::Error> {
            Ok(())
        }
    }

    /// Write a genuine preamble for `token` onto one end of a socketpair and
    /// hand the other to `dials` — the shape a real guest dial takes.
    fn dial(dials: &std::sync::mpsc::Sender<std::os::fd::OwnedFd>, token: u64) {
        use std::io::Write;
        use std::os::fd::OwnedFd;
        let (mut guest, host) = UnixStream::pair().expect("socketpair");
        guest
            .write_all(&preamble::Preamble::encode(token))
            .expect("write preamble");
        dials
            .send(OwnedFd::from(host))
            .expect("pump must still be receiving");
    }

    /// A dial that arrives before `await_dial` asks for it is still found —
    /// the pump parks it, keyed by its token, until an awaiter comes.
    #[test]
    fn a_dial_that_arrives_first_is_found_by_a_later_awaiter() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hatchery = MachineHatchery::start(Box::new(FakeMachine { dials: rx }));
        dial(&tx, 42);

        // Give the pump a moment to accept and route it before we ask.
        std::thread::sleep(Duration::from_millis(100));
        let mut stream = hatchery
            .await_dial(42, Duration::from_secs(2))
            .expect("the early dial must still be found");
        let mut buf = [0u8; preamble::LEN];
        std::io::Read::read_exact(&mut stream, &mut buf).expect("the preamble is still on it");
        assert_eq!(
            preamble::Preamble::decode(&buf)
                .expect("valid preamble")
                .token,
            42
        );

        drop(tx);
        let _ = hatchery.join();
    }

    /// An awaiter that registers first is completed once the matching dial
    /// lands — the other rendezvous order.
    #[test]
    fn an_awaiter_registered_first_is_completed_by_a_later_dial() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hatchery = Arc::new(MachineHatchery::start(Box::new(FakeMachine { dials: rx })));
        let waiter = {
            let hatchery = Arc::clone(&hatchery);
            std::thread::spawn(move || hatchery.await_dial(7, Duration::from_secs(2)))
        };
        std::thread::sleep(Duration::from_millis(50));
        dial(&tx, 7);

        let stream = waiter
            .join()
            .expect("the waiter thread must not panic")
            .expect("the dial must complete the wait");
        drop(stream);
        drop(tx);
        let hatchery = Arc::try_unwrap(hatchery).unwrap_or_else(|_| panic!("sole owner"));
        let _ = hatchery.join();
    }

    /// A dial whose first bytes are not the agent-port magic is closed and
    /// counted, and never satisfies any awaiter.
    #[test]
    fn a_stray_dial_is_closed_and_counted() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hatchery = MachineHatchery::start(Box::new(FakeMachine { dials: rx }));
        let (mut guest, host) = UnixStream::pair().expect("socketpair");
        std::io::Write::write_all(&mut guest, b"not an agent dial at all").expect("write junk");
        tx.send(std::os::fd::OwnedFd::from(host))
            .expect("pump receiving");

        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hatchery.refused_dials(), 1);

        drop(tx);
        let _ = hatchery.join();
    }
}
