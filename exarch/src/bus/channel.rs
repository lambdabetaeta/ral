//! The bus transport: a bounded, coalescing queue shaped like `std::sync::mpsc`
//! — same method names, same error types — so `Emitter` and `sink.rs`'s
//! `drain_pass` need only name the type.
//!
//! Pushing a `Token`/`Thinking` (concatenate) or `State` (replace) merges into
//! the tail entry iff that tail is the same class and the same agent; every
//! other [`Kind`] is reserved, always pushed as its own entry, never merged,
//! never dropped. So a token run can never migrate across a `ToolCall` of the
//! same agent, and a flood bounds itself to one growing entry.
//!
//! Past [`MERGE_TEXT_CAP`] a merged entry sheds its oldest text and the shed
//! count rides out as a `SystemNote` marker when the entry drains —
//! degradation the user sees, never silence.

use super::AgentId;
use crate::bus::event::{Event, Kind, Signal};
use crate::record::Transient;
use crate::sync::LockExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The coalescible class a signal belongs to; `None` is a reserved signal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeClass {
    Token,
    Thinking,
    State,
}

/// The envelope a coalescible signal rides — merging never crosses envelopes,
/// so a legacy `Kind::Token` run and a seam-published `Transient::Token` run
/// stay two entries even for one agent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Kind,
    Seam,
}

fn merge_key(sig: &Signal) -> Option<(Lane, AgentId, MergeClass)> {
    match sig {
        Signal::Event(ev) => {
            let class = match &ev.kind {
                Kind::Token(_) => MergeClass::Token,
                Kind::Thinking(_) => MergeClass::Thinking,
                Kind::State(_) => MergeClass::State,
                _ => return None,
            };
            Some((Lane::Kind, ev.id, class))
        }
        Signal::Transient(id, t) => {
            let class = match t {
                Transient::Token(_) => MergeClass::Token,
                Transient::Thinking(_) => MergeClass::Thinking,
                Transient::State(_) => MergeClass::State,
                _ => return None,
            };
            Some((Lane::Seam, *id, class))
        }
        Signal::Fact(..) => None,
    }
}

/// The accumulated text of a `Token`/`Thinking` signal, in either envelope.
fn merged_text(sig: &mut Signal) -> Option<&mut String> {
    match sig {
        Signal::Event(ev) => match &mut ev.kind {
            Kind::Token(s) | Kind::Thinking(s) => Some(s),
            _ => None,
        },
        Signal::Transient(_, Transient::Token(s) | Transient::Thinking(s)) => Some(s),
        Signal::Transient(..) | Signal::Fact(..) => None,
    }
}

fn text_len(sig: &Signal) -> usize {
    match sig {
        Signal::Event(ev) => match &ev.kind {
            Kind::Token(s) | Kind::Thinking(s) => s.len(),
            _ => 0,
        },
        Signal::Transient(_, Transient::Token(s) | Transient::Thinking(s)) => s.len(),
        Signal::Transient(..) | Signal::Fact(..) => 0,
    }
}

/// Cap on a merged `Token`/`Thinking` entry's accumulated text. `State`
/// replaces rather than grows, so it never reaches the cap.
pub(crate) const MERGE_TEXT_CAP: usize = 256 * 1024;

/// One resident entry, plus the bytes shed off its front past
/// [`MERGE_TEXT_CAP`] — zero for `State` and for every reserved signal.
struct QueueEntry {
    signal: Signal,
    elided: u64,
}

struct BusQueue {
    items: VecDeque<QueueEntry>,
    /// Served ahead of `items`, so a marker immediately follows the entry it
    /// describes.
    markers: VecDeque<Signal>,
    /// Resident coalescible bytes, kept incrementally so no reader has to walk
    /// the queue for the figure.
    bytes: usize,
}

impl BusQueue {
    fn new() -> Self {
        Self {
            items: VecDeque::new(),
            markers: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, signal: Signal) {
        if let Some(key) = merge_key(&signal)
            && let Some(tail) = self.items.back_mut()
            && merge_key(&tail.signal) == Some(key)
        {
            merge_into(tail, signal, &mut self.bytes);
            return;
        }
        self.bytes += text_len(&signal);
        self.items.push_back(QueueEntry { signal, elided: 0 });
    }
}

/// `tail` is already confirmed to be the same lane, class, and agent as
/// `incoming`: text concatenates under the cap, a newer `State` replaces the
/// older in place.
fn merge_into(tail: &mut QueueEntry, mut incoming: Signal, bytes: &mut usize) {
    match merged_text(&mut incoming) {
        Some(add) => {
            let acc = merged_text(&mut tail.signal)
                .expect("merge_key agrees the incoming and tail signals match");
            *bytes += add.len();
            acc.push_str(add);
            if acc.len() > MERGE_TEXT_CAP {
                // Round the cut forward to a char boundary, or the retained
                // tail is no longer valid UTF-8.
                let cut = ral_core::text::ceil_char_boundary(acc, acc.len() - MERGE_TEXT_CAP);
                acc.drain(..cut);
                tail.elided += cut as u64;
                *bytes -= cut;
            }
        }
        // `State` in either envelope: supersession, not accumulation.
        None => tail.signal = incoming,
    }
}

fn overflow_note(class: MergeClass, elided: u64) -> String {
    let label = match class {
        MergeClass::Token => "token",
        MergeClass::Thinking => "thinking",
        MergeClass::State => "state",
    };
    format!(
        "presentation bus: elided {elided} B of coalesced {label} output past the {MERGE_TEXT_CAP}-B cap"
    )
}

/// A pending marker first, else the front entry — minting that entry's marker,
/// for the *next* pop, when it shed text.
fn pop_one(q: &mut BusQueue) -> Option<Signal> {
    if let Some(sig) = q.markers.pop_front() {
        return Some(sig);
    }
    let entry = q.items.pop_front()?;
    q.bytes -= text_len(&entry.signal);
    if entry.elided > 0 {
        let (_, id, class) =
            merge_key(&entry.signal).expect("elided is only ever set on a coalescible entry");
        q.markers.push_back(Signal::Event(Event {
            id,
            kind: Kind::SystemNote(overflow_note(class, entry.elided)),
        }));
    }
    Some(entry.signal)
}

/// `receiver_alive` lets a sender whose receiver is already gone — which
/// `Emitter::muted_child` arranges deliberately — no-op its pushes instead of
/// growing a queue nobody will drain.
struct BusShared {
    state: Mutex<BusQueue>,
    signal: Condvar,
    receiver_alive: AtomicBool,
    senders: AtomicUsize,
}

impl BusShared {
    /// Ignore poison rather than propagate it — the inbox's policy too: every
    /// mutation under this lock is total, so a panicked holder cannot leave the
    /// queue torn, and poisoning would deafen every later sender and receiver
    /// over one unrelated panic.
    fn lock(&self) -> MutexGuard<'_, BusQueue> {
        self.state.lock_ignore_poison()
    }
}

/// The cloneable sender side — the `mpsc::Sender<Event>` replacement.
pub struct BusSender(Arc<BusShared>);

impl Clone for BusSender {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}

impl Drop for BusSender {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Wake a parked receiver so it sees the disconnect rather than
            // waiting out its timeout. Taking the lock first, with nothing left
            // to push, is what closes the gap between a receiver reading
            // `senders` and parking on the condvar — otherwise this wake can
            // land unheard in between.
            drop(self.0.lock());
            self.0.signal.notify_all();
        }
    }
}

impl BusSender {
    /// Push `ev` under the merge rule and wake a parked receiver; a no-op once
    /// the receiver is gone, which is what lets `Emitter::muted_child` swallow
    /// a display stream forever without leaking it.
    ///
    /// # Errors
    /// Returns `Err(SendError(ev))` when the receiver has been dropped.
    #[allow(
        clippy::result_large_err,
        reason = "the Err payload is the undelivered Event itself — handing it back is the contract, and its width is Event's, not an error type's"
    )]
    pub fn send(&self, ev: Event) -> Result<(), SendError<Event>> {
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            return Err(SendError(ev));
        }
        self.0.lock().push(Signal::Event(ev));
        self.0.signal.notify_all();
        Ok(())
    }

    /// [`Self::send`] over the full [`Signal`] payload — what the record
    /// seam's publisher rides.
    ///
    /// # Errors
    /// Returns `Err(SendError(sig))` when the receiver has been dropped.
    #[allow(
        clippy::result_large_err,
        reason = "the Err payload is the undelivered Signal itself — handing it back is the contract, and its width is Signal's, not an error type's"
    )]
    pub fn send_signal(&self, sig: Signal) -> Result<(), SendError<Signal>> {
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            return Err(SendError(sig));
        }
        self.0.lock().push(sig);
        self.0.signal.notify_all();
        Ok(())
    }

    /// Downgrade to a sender that does not hold the channel open — the record
    /// seam's publisher.  A session's log outlives any one bus, and its facts
    /// are durable without a channel, so the seam must never make a drain's
    /// disconnect wait on a session object's lifetime: liveness belongs to
    /// the real producers alone.
    pub(crate) fn downgrade(&self) -> WeakSender {
        WeakSender(self.0.clone())
    }
}

/// A [`BusSender`] that plays no part in disconnect: it never counts as a
/// live sender, and a push after every real sender has gone may be seen or
/// may race the receiver's disconnect — either is sound, because a fact it
/// carries is already durable and a consumer catches up from the file.
pub(crate) struct WeakSender(Arc<BusShared>);

impl WeakSender {
    /// Push under the same merge rule as [`BusSender::send`]; a no-op once
    /// the receiver is gone.
    ///
    /// # Errors
    /// Returns `Err(SendError(sig))` when the receiver has been dropped.
    #[allow(
        clippy::result_large_err,
        reason = "the Err payload is the undelivered Signal itself — the same contract as BusSender::send"
    )]
    pub(crate) fn send_signal(&self, sig: Signal) -> Result<(), SendError<Signal>> {
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            return Err(SendError(sig));
        }
        self.0.lock().push(sig);
        self.0.signal.notify_all();
        Ok(())
    }
}

/// The single-consumer receiver side — the `mpsc::Receiver<Event>` replacement.
pub struct BusReceiver(Arc<BusShared>);

impl BusReceiver {
    /// Block until an event arrives or every sender has dropped.
    ///
    /// # Errors
    /// Returns `Err(RecvError)` once the queue is empty and every sender has
    /// dropped.
    pub fn recv(&self) -> Result<Signal, std::sync::mpsc::RecvError> {
        let mut q = self.0.lock();
        loop {
            if let Some(sig) = pop_one(&mut q) {
                return Ok(sig);
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(std::sync::mpsc::RecvError);
            }
            q = self
                .0
                .signal
                .wait(q)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Non-blocking [`Self::recv`].
    ///
    /// # Errors
    /// Returns `Err(TryRecvError::Empty)` when no event is queued but senders
    /// remain, or `Err(TryRecvError::Disconnected)` when the queue is empty
    /// and every sender has dropped.
    pub fn try_recv(&self) -> Result<Signal, TryRecvError> {
        let mut q = self.0.lock();
        match pop_one(&mut q) {
            Some(sig) => Ok(sig),
            None if self.0.senders.load(Ordering::Acquire) == 0 => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    /// [`Self::recv`] bounded by `timeout`.
    ///
    /// # Errors
    /// Returns `Err(RecvTimeoutError::Timeout)` when `timeout` elapses before
    /// an event arrives, or `Err(RecvTimeoutError::Disconnected)` when the
    /// queue is empty and every sender has dropped.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Signal, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut q = self.0.lock();
        loop {
            if let Some(sig) = pop_one(&mut q) {
                return Ok(sig);
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
            let (guard, _) = self
                .0
                .signal
                .wait_timeout(q, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            q = guard;
        }
    }

    /// [`Self::try_recv`] through the same transitional legacy projection
    /// `drain_pass` applies: the next signal that still renders as a `Kind`
    /// event, skipping the seam's other passengers.  Dies with `Kind` once
    /// both printers draw the view fold.
    ///
    /// # Errors
    /// As [`Self::try_recv`], once no projectable signal remains buffered.
    pub fn try_next_event(&self) -> Result<Event, TryRecvError> {
        loop {
            if let Some(ev) = self.try_recv()?.into_event() {
                return Ok(ev);
            }
        }
    }

    /// Queue depth, a whole merged run counting as one entry — the
    /// `/resources` `bus.depth` figure. Drains nothing and wakes nobody.
    pub fn depth(&self) -> usize {
        let q = self.0.lock();
        q.items.len() + q.markers.len()
    }

    /// Resident merged text — the `/resources` `bus.bytes` figure.
    pub fn bytes(&self) -> usize {
        self.0.lock().bytes
    }
}

impl Drop for BusReceiver {
    fn drop(&mut self) {
        self.0.receiver_alive.store(false, Ordering::Release);
    }
}

/// Blocking iteration, so `for ev in rx` and `rx.into_iter()` read as they
/// would on an `mpsc::Receiver`.
impl Iterator for BusReceiver {
    type Item = Signal;

    fn next(&mut self) -> Option<Signal> {
        self.recv().ok()
    }
}

/// A fresh queue — the `mpsc::channel()` replacement.
pub fn channel() -> (BusSender, BusReceiver) {
    let shared = Arc::new(BusShared {
        state: Mutex::new(BusQueue::new()),
        signal: Condvar::new(),
        receiver_alive: AtomicBool::new(true),
        senders: AtomicUsize::new(1),
    });
    (BusSender(shared.clone()), BusReceiver(shared))
}

#[cfg(test)]
mod tests {
    use super::{Event, Kind, MERGE_TEXT_CAP, channel};
    use std::path::PathBuf;
    use std::sync::mpsc::TryRecvError;

    #[test]
    fn bus_queue_token_flood_coalesces_to_one_entry_in_order() {
        let (tx, rx) = channel();
        for i in 0..200 {
            tx.send(Event {
                id: 1,
                kind: Kind::Token(i.to_string()),
            })
            .unwrap();
        }
        assert_eq!(
            rx.depth(),
            1,
            "an uninterrupted same-agent token run merges into one entry"
        );
        let ev = rx.try_next_event().expect("the merged entry");
        let expected: String = (0..200).map(|i| i.to_string()).collect();
        match ev.kind {
            Kind::Token(text) => assert_eq!(text, expected, "concatenation keeps arrival order"),
            _ => panic!("expected a merged Token entry"),
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// The newest tail is the part worth keeping, so elision takes the front.
    #[test]
    fn bus_queue_flood_past_the_byte_cap_yields_one_overflow_marker() {
        let (tx, rx) = channel();
        let first = "a".repeat(MERGE_TEXT_CAP);
        tx.send(Event {
            id: 1,
            kind: Kind::Token(first),
        })
        .unwrap();
        let overflow = "b".repeat(100);
        tx.send(Event {
            id: 1,
            kind: Kind::Token(overflow.clone()),
        })
        .unwrap();

        let ev = rx.try_next_event().expect("the merged, capped entry");
        match ev.kind {
            Kind::Token(text) => {
                assert_eq!(
                    text.len(),
                    MERGE_TEXT_CAP,
                    "elision holds the entry at the cap"
                );
                assert!(text.ends_with(&overflow), "the newest tail survives");
                assert!(
                    text.starts_with(&"a".repeat(MERGE_TEXT_CAP - 100)),
                    "exactly the 100 oldest bytes were elided from the front, no more"
                );
            }
            _ => panic!("expected the merged Token entry"),
        }

        let marker = rx.try_next_event().expect("exactly one overflow marker");
        match marker.kind {
            Kind::SystemNote(note) => {
                assert!(note.contains("100"), "names the elided count: {note}");
                assert!(note.contains("token"), "names the class: {note}");
            }
            _ => panic!("expected a SystemNote overflow marker"),
        }
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "exactly one marker, nothing else"
        );
    }

    /// Two floods either side of a `ToolCall` stay two entries rather than
    /// merging through it, which is what keeps ordering intact.
    #[test]
    fn bus_queue_lifecycle_events_survive_a_flood_uncrossed() {
        let (tx, rx) = channel();
        tx.send(Event {
            id: 1,
            kind: Kind::Born {
                log_dir: PathBuf::new(),
                name: "a".into(),
                parent: 0,
                branch: false,
            },
        })
        .unwrap();
        for _ in 0..50 {
            tx.send(Event {
                id: 1,
                kind: Kind::Token("x".into()),
            })
            .unwrap();
        }
        tx.send(Event {
            id: 1,
            kind: Kind::ToolCall {
                tool: "ral",
                cmd: "pwd".into(),
                summary: None,
            },
        })
        .unwrap();
        for _ in 0..50 {
            tx.send(Event {
                id: 1,
                kind: Kind::Token("y".into()),
            })
            .unwrap();
        }
        tx.send(Event {
            id: 1,
            kind: Kind::Died,
        })
        .unwrap();

        assert_eq!(
            rx.depth(),
            5,
            "Born, one merged run, ToolCall, one merged run, Died: five entries"
        );
        assert!(matches!(rx.try_next_event().unwrap().kind, Kind::Born { .. }));
        match rx.try_next_event().unwrap().kind {
            Kind::Token(t) => assert_eq!(t, "x".repeat(50)),
            _ => panic!("expected the pre-ToolCall merged run"),
        }
        assert!(matches!(rx.try_next_event().unwrap().kind, Kind::ToolCall { .. }));
        match rx.try_next_event().unwrap().kind {
            Kind::Token(t) => assert_eq!(t, "y".repeat(50)),
            _ => panic!("expected the post-ToolCall merged run"),
        }
        assert!(matches!(rx.try_next_event().unwrap().kind, Kind::Died));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// A `State` is superseded by the next one, so the merge rule replaces it
    /// in place rather than growing the entry: a frontend that fell behind
    /// resumes at the state the agent is in, not the one it was leaving.
    #[test]
    fn bus_queue_newer_state_replaces_older() {
        let (tx, rx) = channel();
        tx.send(Event {
            id: 1,
            kind: Kind::State(crate::bus::AgentState::AwaitingModel),
        })
        .unwrap();
        tx.send(Event {
            id: 1,
            kind: Kind::State(crate::bus::AgentState::Compacting),
        })
        .unwrap();
        assert_eq!(
            rx.depth(),
            1,
            "a same-agent State run replaces in place rather than growing"
        );
        match rx.try_next_event().unwrap().kind {
            Kind::State(s) => assert_eq!(
                s,
                crate::bus::AgentState::Compacting,
                "the newer state replaced the older"
            ),
            _ => panic!("expected State"),
        }
    }

    /// The merge rule keys on agent id as well as class.
    #[test]
    fn bus_queue_never_merges_across_agents() {
        let (tx, rx) = channel();
        tx.send(Event {
            id: 1,
            kind: Kind::Token("a".into()),
        })
        .unwrap();
        tx.send(Event {
            id: 2,
            kind: Kind::Token("b".into()),
        })
        .unwrap();
        tx.send(Event {
            id: 1,
            kind: Kind::Token("c".into()),
        })
        .unwrap();
        assert_eq!(
            rx.depth(),
            3,
            "an interleaving agent id never merges into another agent's tail entry"
        );
        for (want_id, want_text) in [(1, "a"), (2, "b"), (1, "c")] {
            let ev = rx.try_next_event().expect("three separate entries");
            assert_eq!(ev.id, want_id);
            match ev.kind {
                Kind::Token(t) => assert_eq!(t, want_text),
                _ => panic!("expected Token"),
            }
        }
    }

    #[test]
    fn bus_queue_bytes_tracks_resident_merged_text() {
        let (tx, rx) = channel();
        assert_eq!(rx.bytes(), 0);
        tx.send(Event {
            id: 1,
            kind: Kind::Token("abc".into()),
        })
        .unwrap();
        assert_eq!(rx.bytes(), 3);
        tx.send(Event {
            id: 1,
            kind: Kind::Token("de".into()),
        })
        .unwrap();
        assert_eq!(rx.bytes(), 5, "the merge grows the byte figure");
        let _ = rx.try_next_event().unwrap();
        assert_eq!(rx.bytes(), 0, "draining the entry frees its bytes");
    }

    /// `Emitter::muted_child` leans on this to swallow a display stream forever
    /// without leaking the queue behind it.
    #[test]
    fn bus_sender_send_past_dropped_receiver_is_rejected_not_grown() {
        let (tx, rx) = channel();
        drop(rx);
        let err = tx
            .send(Event {
                id: 1,
                kind: Kind::Token("x".into()),
            })
            .unwrap_err();
        assert!(matches!(err.0.kind, Kind::Token(ref s) if s == "x"));
    }
}
