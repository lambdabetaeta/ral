//! The bus transport: a bounded, coalescing queue shaped like `std::sync::mpsc`
//! — same method names, same error types — so `Emitter` and `sink.rs`'s
//! `drain_pass` need only name the type.
//!
//! Pushing a `Token`/`Thinking` (concatenate) or `Phase` (replace) merges into
//! the tail entry iff that tail is the same class and the same agent; every
//! other [`Kind`] is reserved, always pushed as its own entry, never merged,
//! never dropped. So a token run can never migrate across a `ToolCall` of the
//! same agent, and a flood bounds itself to one growing entry.
//!
//! Past [`MERGE_TEXT_CAP`] a merged entry sheds its oldest text and the shed
//! count rides out as a `SystemNote` marker when the entry drains —
//! degradation the user sees, never silence.

use super::AgentId;
use crate::bus::event::{Event, Kind};
use crate::sync::LockExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The coalescible class a `Kind` belongs to; `None` is a reserved kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeClass {
    Token,
    Thinking,
    Phase,
}

fn merge_class(kind: &Kind) -> Option<MergeClass> {
    match kind {
        Kind::Token(_) => Some(MergeClass::Token),
        Kind::Thinking(_) => Some(MergeClass::Thinking),
        Kind::Phase(_) => Some(MergeClass::Phase),
        _ => None,
    }
}

/// Cap on a merged `Token`/`Thinking` entry's accumulated text. `Phase`
/// replaces rather than grows, so it never reaches the cap.
pub(crate) const MERGE_TEXT_CAP: usize = 256 * 1024;

/// One resident entry, plus the bytes shed off its front past
/// [`MERGE_TEXT_CAP`] — zero for `Phase` and for every reserved kind.
struct QueueEntry {
    id: AgentId,
    kind: Kind,
    elided: u64,
}

struct BusQueue {
    items: VecDeque<QueueEntry>,
    /// Served ahead of `items`, so a marker immediately follows the entry it
    /// describes.
    markers: VecDeque<Event>,
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

    fn push(&mut self, id: AgentId, kind: Kind) {
        if let Some(class) = merge_class(&kind)
            && let Some(tail) = self.items.back_mut()
            && tail.id == id
            && merge_class(&tail.kind) == Some(class)
        {
            merge_into(tail, kind, &mut self.bytes);
            return;
        }
        self.bytes += payload_len(&kind);
        self.items.push_back(QueueEntry {
            id,
            kind,
            elided: 0,
        });
    }
}

/// `tail` is already confirmed to be the same class and agent as `incoming`.
fn merge_into(tail: &mut QueueEntry, incoming: Kind, bytes: &mut usize) {
    match (&mut tail.kind, incoming) {
        (Kind::Token(acc), Kind::Token(add)) | (Kind::Thinking(acc), Kind::Thinking(add)) => {
            *bytes += add.len();
            acc.push_str(&add);
            if acc.len() > MERGE_TEXT_CAP {
                // Round the cut forward to a char boundary, or the retained
                // tail is no longer valid UTF-8.
                let cut = ral_core::text::ceil_char_boundary(acc, acc.len() - MERGE_TEXT_CAP);
                acc.drain(..cut);
                tail.elided += cut as u64;
                *bytes -= cut;
            }
        }
        (Kind::Phase(acc), Kind::Phase(add)) => {
            *bytes -= acc.len();
            *acc = add;
            *bytes += acc.len();
        }
        _ => unreachable!("merge_class agrees the incoming and tail kinds match"),
    }
}

fn payload_len(kind: &Kind) -> usize {
    match kind {
        Kind::Token(s) | Kind::Thinking(s) | Kind::Phase(s) => s.len(),
        _ => 0,
    }
}

fn overflow_note(class: MergeClass, elided: u64) -> String {
    let label = match class {
        MergeClass::Token => "token",
        MergeClass::Thinking => "thinking",
        MergeClass::Phase => "phase",
    };
    format!(
        "presentation bus: elided {elided} B of coalesced {label} output past the {MERGE_TEXT_CAP}-B cap"
    )
}

/// A pending marker first, else the front entry — minting that entry's marker,
/// for the *next* pop, when it shed text.
fn pop_one(q: &mut BusQueue) -> Option<Event> {
    if let Some(ev) = q.markers.pop_front() {
        return Some(ev);
    }
    let entry = q.items.pop_front()?;
    if merge_class(&entry.kind).is_some() {
        q.bytes -= payload_len(&entry.kind);
    }
    if entry.elided > 0 {
        let class =
            merge_class(&entry.kind).expect("elided is only ever set on a coalescible entry");
        q.markers.push_back(Event {
            id: entry.id,
            kind: Kind::SystemNote(overflow_note(class, entry.elided)),
        });
    }
    Some(Event {
        id: entry.id,
        kind: entry.kind,
    })
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
    pub fn send(&self, ev: Event) -> Result<(), SendError<Event>> {
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            return Err(SendError(ev));
        }
        self.0.lock().push(ev.id, ev.kind);
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
    pub fn recv(&self) -> Result<Event, std::sync::mpsc::RecvError> {
        let mut q = self.0.lock();
        loop {
            if let Some(ev) = pop_one(&mut q) {
                return Ok(ev);
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
    pub fn try_recv(&self) -> Result<Event, TryRecvError> {
        let mut q = self.0.lock();
        match pop_one(&mut q) {
            Some(ev) => Ok(ev),
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
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut q = self.0.lock();
        loop {
            if let Some(ev) = pop_one(&mut q) {
                return Ok(ev);
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
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
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
        let ev = rx.try_recv().expect("the merged entry");
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

        let ev = rx.try_recv().expect("the merged, capped entry");
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

        let marker = rx.try_recv().expect("exactly one overflow marker");
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
        assert!(matches!(rx.try_recv().unwrap().kind, Kind::Born { .. }));
        match rx.try_recv().unwrap().kind {
            Kind::Token(t) => assert_eq!(t, "x".repeat(50)),
            _ => panic!("expected the pre-ToolCall merged run"),
        }
        assert!(matches!(rx.try_recv().unwrap().kind, Kind::ToolCall { .. }));
        match rx.try_recv().unwrap().kind {
            Kind::Token(t) => assert_eq!(t, "y".repeat(50)),
            _ => panic!("expected the post-ToolCall merged run"),
        }
        assert!(matches!(rx.try_recv().unwrap().kind, Kind::Died));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// A `Phase` is superseded by the next one, so the merge rule replaces it
    /// in place rather than growing the entry.
    #[test]
    fn bus_queue_newer_phase_replaces_older() {
        let (tx, rx) = channel();
        tx.send(Event {
            id: 1,
            kind: Kind::Phase("thinking".into()),
        })
        .unwrap();
        tx.send(Event {
            id: 1,
            kind: Kind::Phase("compacting".into()),
        })
        .unwrap();
        assert_eq!(
            rx.depth(),
            1,
            "a same-agent Phase run replaces in place rather than growing"
        );
        match rx.try_recv().unwrap().kind {
            Kind::Phase(p) => assert_eq!(p, "compacting", "the newer phase replaced the older"),
            _ => panic!("expected Phase"),
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
            let ev = rx.try_recv().expect("three separate entries");
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
        rx.try_recv().unwrap();
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
