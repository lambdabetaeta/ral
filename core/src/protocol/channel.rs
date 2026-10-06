//! Duplex framed channel: one connection is one session, carrying
//! length-prefixed JSON frames ([`crate::frame`]) in either direction, and
//! the one liveness law both ends of it keep.
//!
//! [`WireStream`] is `UnixStream` on Unix and `TcpStream` on Windows, yet
//! `vm-manager` hands back `AF_VSOCK` and `AF_HYPERV` sockets: the types are
//! borrowed only as std's owner of a *connected stream socket*, every operation
//! used here being one syscall in any family.  Nothing here or above asks such
//! a stream for an address — the one question that would expose the pretence.

use super::Frame;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

/// The connected stream socket a [`WireChannel`] frames over: std's owner
/// type, not a statement about the address family.
#[cfg(unix)]
pub type WireStream = std::os::unix::net::UnixStream;

/// The connected stream socket a [`WireChannel`] frames over: std's owner
/// type, not a statement about the address family.
#[cfg(windows)]
pub type WireStream = std::net::TcpStream;

pub struct WireChannel {
    stream: WireStream,
}

impl WireChannel {
    /// Two ends of one connection, for a front-end and an engine that share a
    /// process tree.
    ///
    /// # Errors
    /// Returns the socket error if the pair cannot be made.
    #[cfg(all(unix, test))]
    pub(crate) fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = crate::process::cloexec_socketpair()?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Two ends of one connection, for a front-end and an engine that share a
    /// process tree.
    ///
    /// Windows has no `socketpair(2)`, so the pair is a loopback connection
    /// made and accepted here — genuinely TCP, hence the one place Nagle is
    /// worth turning off: a frame protocol wants each frame on the wire now.
    ///
    /// # Errors
    /// Returns the socket error if the loopback pair cannot be established.
    #[cfg(all(windows, test))]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:wire-pair-windows] the Windows twin of `process::cloexec_socketpair` ([silent:cloexec-socketpair]): the same one-connection-for-a-process-tree, spelled as a loopback bind-connect-accept because Windows has no socketpair(2). No outside name is reached (the port is ephemeral and the peer is this process), so it is silent for the same reason its Unix hemisphere is."
    )]
    pub(crate) fn pair() -> io::Result<(Self, Self)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let a = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (b, _peer) = listener.accept()?;
        a.set_nodelay(true)?;
        b.set_nodelay(true)?;
        Ok((Self { stream: a }, Self { stream: b }))
    }

    /// Frame over an already-connected stream: one end of a [`Self::pair`], or
    /// the virtual-socket connection into a guest VM — a backend's own
    /// `OwnedFd` or `OwnedSocket` converts straight in.
    pub(crate) fn from_stream(stream: impl Into<WireStream>) -> Self {
        Self {
            stream: stream.into(),
        }
    }

    /// Read one frame.
    ///
    /// # Errors
    /// Returns the read or decode error.  `Ok(None)` is a clean EOF: the peer
    /// closed between frames.
    pub(crate) fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        crate::frame::read_frame(&mut self.stream)
    }

    /// Write one frame.
    ///
    /// # Errors
    /// Returns the encode or write error.
    pub(crate) fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        crate::frame::write_frame(&mut self.stream, frame)
    }

    /// Duplicate the socket so one channel reads while another writes the same
    /// connection.  The duplicate is uninheritable on both platforms, so no
    /// process a run spawns receives it.
    ///
    /// # Errors
    /// Returns the duplication error.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Wait until a frame is readable, or `timeout` passes; `None` waits
    /// indefinitely.  A hangup counts as readable, so `true` promises
    /// `read_frame` returns promptly — frame, `None`, or error.  This is how
    /// `engine_session` notices a silent front-end; the deadline governs frame
    /// *arrival*, a peer dying mid-frame erring the read rather than stalling.
    ///
    /// # Errors
    /// Returns the `poll(2)` error, with `EINTR` retried against the
    /// caller's own timeout.
    #[cfg(unix)]
    pub(crate) fn poll_readable(&self, timeout: Option<Duration>) -> io::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        use rustix::io::Errno;
        use std::time::Instant;
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // An unrepresentable remainder waits indefinitely, as `None` does.
            let left = deadline
                .and_then(|d| Timespec::try_from(d.saturating_duration_since(Instant::now())).ok());
            let mut fds = [PollFd::new(&self.stream, PollFlags::IN)];
            match poll(&mut fds, left.as_ref()) {
                Ok(ready) => return Ok(ready > 0),
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Shut the socket down both ways, waking any thread parked in
    /// `read_frame` or `poll_readable` — clones included, since they
    /// share the one underlying connection.
    pub(crate) fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Bound how long a single `write_frame` may block on this socket.
    ///
    /// `SO_SNDTIMEO` lives on the shared file description, not the file
    /// descriptor, so every clone of this channel inherits the same deadline —
    /// one call at construction reaches every writer — and it governs writes
    /// alone: `read_frame` and `poll_readable` are untouched. A write
    /// that stalls past `d` becomes an `io::Error`, which the framing law at
    /// the call sites above converts into connection death, the same as a
    /// severed pipe. The invariant this buys: no `write_frame` blocks past the
    /// peer's patience deadline.
    ///
    /// # Errors
    /// Returns the socket error if the timeout cannot be set.
    pub(crate) fn set_write_deadline(&self, d: Duration) -> io::Result<()> {
        self.stream.set_write_timeout(Some(d))
    }
}

/// The one write door under the severance law: write `frame`, and if the
/// write fails, record it through `record` and shut the channel down before
/// the lock is released.
///
/// Both ends of the protocol pass through here — the front-end's
/// `write_through` records a `Severed` cause, the engine's `engine_write`
/// raises its fault flag — so the ordering the law states is enforced once
/// rather than restated at each door. What it buys: nothing can append a
/// fresh frame after a truncated one, and no window exists in which the
/// record still calls an already-shut socket healthy.
///
/// Deliberately outside [`LockExt`](crate::sync::LockExt)'s recover-poison
/// policy: a panic between `write_frame` and `shutdown` can leave a partial
/// frame on the socket that neither `record` nor a shutdown has sealed off,
/// and letting the next writer resume into that torn stream is worse than
/// the panic propagating; a poisoned channel must stay poisoned.
///
/// # Errors
/// Returns the write failure, so a caller that must react further — the
/// heartbeat's `Ping` arm breaking its loop — can, without severing again.
#[allow(
    clippy::disallowed_methods,
    reason = "the wire writer: a panic between write_frame and shutdown leaves a partial frame on the socket that neither the severance record nor shutdown has sealed off, so a recovered guard would append the next frame onto a torn one"
)]
pub(crate) fn write_or_sever(
    ch: &Mutex<WireChannel>,
    frame: &Frame,
    record: impl FnOnce(&io::Error),
) -> io::Result<()> {
    let mut guard = ch.lock().unwrap();
    let outcome = guard.write_frame(frame);
    if let Err(e) = &outcome {
        record(e);
        guard.shutdown();
    }
    outcome
}

// ── Liveness ──────────────────────────────────────────────────────────

/// How often a front-end pings by default.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// The armed silence the engine reads as the front-end's death: six default
/// ping intervals, so no scheduling jitter can fake one.
#[cfg(unix)]
pub(crate) const HOST_SILENCE_DEADLINE: Duration = PING_INTERVAL.saturating_mul(6);

/// How briskly a front-end manufactures traffic to keep an idle engine
/// visibly alive, and how much silence it tolerates before declaring the
/// peer dead.
///
/// The same `deadline` also arms [`WireChannel::set_write_deadline`],
/// so it doubles as the bound on how long a single stalled write may block
/// before that, too, is read as the peer's death.
///
/// Since liveness is *any* received frame (the law on [`Frame::Ping`]), the
/// interval must be comfortably shorter than the deadline: several exchanges
/// fall inside one window, so a single dropped one never condemns a live peer.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    pub interval: Duration,
    pub deadline: Duration,
}

/// How many whole `tick`s `span` is worth, never none.
pub(crate) fn ticks(span: Duration, tick: Duration) -> u32 {
    let count = span.as_nanos().checked_div(tick.as_nanos()).unwrap_or(1);
    u32::try_from(count).unwrap_or(u32::MAX).max(1)
}

impl Liveness {
    /// How many unanswered `Ping`s amount to the deadline.
    ///
    /// The deadline is *configured* as a span but *judged* as this count: a
    /// clock measures the host's absence as readily as the peer's silence, so
    /// a suspended laptop would wake to find its engine long dead of a silence
    /// nobody was running to observe.  A probe is evidence because this
    /// process authored it; a duration is evidence of nothing.
    pub(crate) fn probes(self) -> u32 {
        ticks(self.deadline, self.interval)
    }
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            interval: PING_INTERVAL,
            deadline: Duration::from_secs(25),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Control, DispatchId};

    #[test]
    fn round_trip_control() {
        let (mut a, mut b) = WireChannel::pair().unwrap();
        let frame = Frame::Control(Control::Cancel(DispatchId(7)));
        a.write_frame(&frame).unwrap();
        let got = b.read_frame().unwrap().unwrap();
        assert_eq!(got, frame);
    }

    #[test]
    fn eof_returns_none() {
        let (mut a, b) = WireChannel::pair().unwrap();
        drop(b);
        let got = a.read_frame().unwrap();
        assert!(got.is_none());
    }

    /// Silence is not readable and a frame in flight is — the difference the
    /// engine's silence deadline rests on, pinned here rather than left to the
    /// transport's integration tests.
    #[cfg(unix)]
    #[test]
    fn poll_readable_tells_silence_from_traffic() {
        let (mut a, b) = WireChannel::pair().unwrap();
        assert!(
            !b.poll_readable(Some(std::time::Duration::from_millis(50)))
                .unwrap(),
            "an idle channel is not readable"
        );
        a.write_frame(&Frame::Ping(1)).unwrap();
        assert!(
            b.poll_readable(Some(std::time::Duration::from_secs(5)))
                .unwrap(),
            "a channel carrying a frame is readable"
        );
    }
}
