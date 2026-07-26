//! This is the bus's bounded, coalescing transport.
//!
//! `Emitter`/`FleetBus` carry events through [`BusSender`]/[`BusReceiver`]: a
//! bounded, coalescing queue, so a producer flood (a token stream the
//! renderer can't keep up with) is capped rather than growing heap without
//! limit. The pair exposes the same `Sender`/`Receiver`-shaped
//! API (`send`, `try_recv`, `recv_timeout`, even reusing `std::sync::mpsc`'s
//! own error types), so [`crate::bus::drain_pass`]/[`crate::bus::Sink::drive`] and every call site
//! need only name the type, not change the logic.
//!
//! # The merge rule
//!
//! Pushing a coalescible [`crate::bus::Kind`] — `Token`/`Thinking`
//! (concatenate) or `Phase` (replace; its own doc already declares
//! superseded-by-next semantics) — merges into the queue's TAIL entry *iff*
//! that tail is the same class and the same agent id; every other `Kind` is
//! reserved and always pushed as its own entry, never merged, never dropped.
//! A token run can therefore never migrate across a `ToolCall`/`Born`/`Died`
//! of the same agent (ordering is preserved by construction), and a flood
//! bounds itself to one growing entry rather than one entry per token.
//!
//! # Elision
//!
//! A merged `Token`/`Thinking` entry's accumulated text is capped at
//! [`MERGE_TEXT_CAP`]; past it, the front of the text is dropped (the newest
//! tail survives) and the drop count rides to one [`crate::bus::Kind::SystemNote`]
//! overflow marker the next time the entry is drained — degradation the user
//! sees, never silence. `Phase` replaces outright and never elides.

use super::AgentId;
use crate::bus::event::{Event, Kind};
use crate::sync::LockExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Coalescible class a [`crate::bus::Kind`] belongs to. `None` (every other variant) is
/// reserved: always pushed, never merged, never dropped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeClass {
    /// [`crate::bus::Kind::Token`] — concatenates.
    Token,
    /// [`crate::bus::Kind::Thinking`] — concatenates.
    Thinking,
    /// [`crate::bus::Kind::Phase`] — replaces.
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

/// Cap on a merged `Token`/`Thinking` entry's accumulated text (256 KiB).
/// `Phase` replaces rather than growing, so it never crosses this cap.
pub(crate) const MERGE_TEXT_CAP: usize = 256 * 1024;

/// One resident entry in the [`BusQueue`]: the event, plus the bytes elided
/// from the front of a merged `Token`/`Thinking` run once it crossed
/// [`MERGE_TEXT_CAP`] — always zero for a reserved kind and for `Phase`.
struct QueueEntry {
    id: AgentId,
    kind: Kind,
    elided: u64,
}

/// The coalescing queue behind [`BusSender`]/[`BusReceiver`]. See the
/// module-level "merge rule" doc above.
struct BusQueue {
    items: VecDeque<QueueEntry>,
    /// Overflow markers minted when a merged entry's elided text is finally
    /// drained ([`pop_one`]) — served ahead of `items` so a marker always
    /// immediately follows the entry it describes.
    markers: VecDeque<Event>,
    /// Running total of bytes held in every merged `Token`/`Thinking`/`Phase`
    /// entry currently resident — the probe's cheap byte figure, maintained
    /// incrementally rather than walked.
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

    /// Apply the merge rule for one incoming `(id, kind)`.
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

/// Merge `incoming` into `tail` — the queue's tail entry, already confirmed
/// the same class and agent as `incoming`. `Token`/`Thinking` concatenate,
/// eliding from the front past [`MERGE_TEXT_CAP`]; `Phase` replaces outright
/// and never elides.
fn merge_into(tail: &mut QueueEntry, incoming: Kind, bytes: &mut usize) {
    match (&mut tail.kind, incoming) {
        (Kind::Token(acc), Kind::Token(add)) | (Kind::Thinking(acc), Kind::Thinking(add)) => {
            *bytes += add.len();
            acc.push_str(&add);
            if acc.len() > MERGE_TEXT_CAP {
                // Drop from the front, rounded forward to the next char
                // boundary so the retained tail stays valid UTF-8.
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

/// Bytes of a coalescible kind's payload; zero for a reserved kind, which
/// never contributes to [`BusQueue::bytes`].
fn payload_len(kind: &Kind) -> usize {
    match kind {
        Kind::Token(s) | Kind::Thinking(s) | Kind::Phase(s) => s.len(),
        _ => 0,
    }
}

/// The dim one-liner naming what a merged run elided, through the existing
/// operational-note vocabulary ([`crate::bus::Kind::SystemNote`]) — transcript-recorded
/// like any other note, never silent.
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

/// Pop the next event: a pending overflow marker first (so it immediately
/// follows the entry it describes), else the queue's front entry — minting
/// its marker, queued for the *next* pop, when it carries elided text.
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

/// Shared state behind [`BusSender`]/[`BusReceiver`]. `receiver_alive` lets a
/// sender whose receiver was already dropped — the `muted_child` pattern: a
/// throwaway channel built purely to swallow display output — no-op its
/// pushes instead of growing a queue nobody will ever drain.
struct BusShared {
    state: Mutex<BusQueue>,
    signal: Condvar,
    receiver_alive: AtomicBool,
    senders: AtomicUsize,
}

impl BusShared {
    /// Lock the queue, recovering from a poisoned mutex rather than
    /// panicking — the same policy as [`crate::bus::inbox::Shared::lock`], for the same reason:
    /// every operation under this lock ([`BusQueue::push`], [`pop_one`]) is a
    /// total mutation, so a panicked holder cannot leave it torn, and
    /// propagating the poison would deafen every future sender and receiver
    /// over one unrelated panic.
    fn lock(&self) -> MutexGuard<'_, BusQueue> {
        self.state.lock_ignore_poison()
    }
}

/// The cloneable sender side of the bus's bounded, coalescing queue — the
/// `mpsc::Sender<Event>` replacement threaded through [`crate::bus::Emitter`]/[`crate::bus::FleetBus`].
///
/// Public alongside [`channel`] and [`crate::bus::Emitter::new`], which each hand one out directly.
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
            // The last sender is gone: wake a parked receiver so it observes
            // the disconnect instead of waiting out its timeout.  Taking the
            // queue lock first — even though there is nothing left to push —
            // is what makes the notify reliable: a lock-then-notify always
            // closes the gap between a receiver checking `senders` and
            // actually starting to wait on the condvar, so the wake can
            // never land unheard in between.
            drop(self.0.lock());
            self.0.signal.notify_all();
        }
    }
}

impl BusSender {
    /// Push `ev`, applying the merge rule, and wake a parked receiver.
    /// A no-op once the receiver is gone — no merge work, no growth — exactly
    /// `mpsc`'s "send to a dropped receiver fails" contract, which is what
    /// lets [`crate::bus::Emitter::muted_child`] swallow a display stream forever without
    /// leaking it.
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

/// The single-consumer receiver side of the bus's bounded, coalescing queue —
/// the `mpsc::Receiver<Event>` replacement.
pub struct BusReceiver(Arc<BusShared>);

impl BusReceiver {
    /// Block until an event arrives or every sender has dropped. The
    /// `mpsc::Receiver::recv` replacement — [`Iterator`] is implemented off
    /// this, so `for ev in rx` / `rx.into_iter()` still work unchanged.
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

    /// Queue depth — a merged run and a reserved kind each count as one entry,
    /// pending overflow markers included. The `/resources` `bus.depth`
    /// figure ([`crate::agent::resources::frontend_rows`]): one pass over the lock,
    /// nothing drained or woken — enumeration is not observation.
    pub fn depth(&self) -> usize {
        let q = self.0.lock();
        q.items.len() + q.markers.len()
    }

    /// Resident bytes across every merged `Token`/`Thinking`/`Phase` entry —
    /// the `/resources` `bus.bytes` figure, a running total rather than a walk.
    pub fn bytes(&self) -> usize {
        self.0.lock().bytes
    }
}

impl Drop for BusReceiver {
    fn drop(&mut self) {
        self.0.receiver_alive.store(false, Ordering::Release);
    }
}

/// Blocking iteration off [`Self::recv`] — the `mpsc::Receiver` shape, so
/// `for ev in rx` and `rx.into_iter()` keep working unchanged over the new
/// transport.
impl Iterator for BusReceiver {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        self.recv().ok()
    }
}

/// A fresh bounded, coalescing queue — the `mpsc::channel()` replacement
/// behind [`crate::bus::FleetBus`]/[`crate::bus::Emitter`].
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

    // ── the bounded, coalescing bus transport ──────────────────────────────

    /// A single agent's token flood coalesces to one queue entry, and the
    /// concatenated text preserves arrival order.
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

    /// Past `MERGE_TEXT_CAP` a merged run elides from the front (the newest
    /// tail survives) and the drain yields exactly one overflow marker naming
    /// the class and the elided count — degradation the user sees, not
    /// silence.
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

    /// `Born`/`ToolCall`/`Died` interleaved with token floods are never
    /// dropped, and a merged run can never cross one: two floods either side
    /// of a `ToolCall` stay two separate entries rather than merging through
    /// it.
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

    /// A newer `Phase` replaces an older one in place rather than growing —
    /// its own doc's superseded-by-next semantics, enforced by the merge
    /// rule.
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

    /// Two agents' token streams never merge together, even interleaved —
    /// the merge rule keys on agent id as well as class.
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

    /// The byte figure grows with a merge and shrinks when the entry drains
    /// — the `/resources` `bus.bytes` row's cheap running total.
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

    /// A send past a dropped receiver is rejected, not silently grown — the
    /// [`crate::bus::Emitter::muted_child`] pattern relies on this to swallow a display
    /// stream forever without leaking the queue behind it.
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
