//! The bus transport: a coalescing queue shaped like `std::sync::mpsc` — same
//! method names, same error types — so `Emitter` and `sink.rs`'s
//! `drain_signals` and `Sink::drive` need only name the type.  Coalescing
//! bounds a text flood's *entry*, not the queue's *depth*: reserved signals
//! still push one entry apiece, uncapped and undropped.
//!
//! Pushing a `Token`/`Thinking` (concatenate) or `State` (replace) merges into
//! the tail entry iff that tail is the same class and the same agent; every
//! other signal is reserved, always pushed as its own entry, never merged,
//! never dropped. So a token run can never migrate across a `ToolCall` of the
//! same agent, and a flood of one class bounds itself to one growing entry.
//!
//! Past [`MERGE_TEXT_CAP`] a merged entry sheds its oldest text and the shed
//! count rides out as a `Transient::Fault` marker when the entry drains —
//! degradation the user sees, never silence; being a fact about the
//! transport, not the session, it is never durable.

use crate::bus::signal::Signal;
use crate::record::AgentId;
use crate::record::Transient;
use ral_core::sync::LockExt;
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

fn merge_key(sig: &Signal) -> Option<(AgentId, MergeClass)> {
    match sig {
        Signal::Transient(id, t) => {
            let class = match t {
                Transient::Token(_) => MergeClass::Token,
                Transient::Thinking(_) => MergeClass::Thinking,
                Transient::State(_) => MergeClass::State,
                _ => return None,
            };
            Some((*id, class))
        }
        Signal::Fact(..) => None,
    }
}

/// The accumulated text of a `Token`/`Thinking` signal.
fn merged_text(sig: &mut Signal) -> Option<&mut String> {
    match sig {
        Signal::Transient(_, Transient::Token(s) | Transient::Thinking(s)) => Some(s),
        Signal::Transient(..) | Signal::Fact(..) => None,
    }
}

fn text_len(sig: &Signal) -> usize {
    match sig {
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

/// `tail` is already confirmed to be the same class and agent as `incoming`:
/// text concatenates under the cap, a newer `State` replaces the older in
/// place.
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
                let cut = acc.ceil_char_boundary(acc.len() - MERGE_TEXT_CAP);
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
        let (id, class) =
            merge_key(&entry.signal).expect("elided is only ever set on a coalescible entry");
        q.markers.push_back(Signal::Transient(
            id,
            Transient::Fault {
                text: overflow_note(class, entry.elided),
            },
        ));
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

/// The cloneable sender side — the `mpsc::Sender<Signal>` replacement.
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
            // The lock enqueues nothing; taking it *is* the handshake. A
            // receiver holds it from its `senders == 0` test until `wait`
            // atomically releases it on parking, so acquiring it here orders
            // this drop wholly before that test — where the receiver reads the
            // zero itself — or wholly after the park, where `notify_all` is
            // heard. Notifying without it may land in the gap between the two,
            // and `recv` has no timeout to recover a wake lost there.
            drop(self.0.lock());
            self.0.signal.notify_all();
        }
    }
}

impl BusSender {
    /// Push `sig` under the merge rule and wake a parked receiver; a no-op
    /// once the receiver is gone, which is what lets `Emitter::muted_child`
    /// swallow a display stream forever without leaking it.
    ///
    /// # Errors
    /// Returns `Err(SendError(sig))` when the receiver has been dropped.
    #[allow(
        clippy::result_large_err,
        reason = "the Err payload is the undelivered Signal itself: handing it back is the contract, and its width is Signal's, not an error type's"
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
    /// Push under the same merge rule as [`BusSender::send_signal`]; a no-op once
    /// the receiver is gone.
    ///
    /// # Errors
    /// Returns `Err(SendError(sig))` when the receiver has been dropped.
    #[allow(
        clippy::result_large_err,
        reason = "the Err payload is the undelivered Signal itself: the same contract as BusSender::send"
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

/// The single-consumer receiver side — the `mpsc::Receiver<Signal>` replacement.
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
mod tests;
