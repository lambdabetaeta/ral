//! A session's typed, multi-producer inbound queue: [`Inbox`] is the owned
//! consumer end, [`Mailbox`] the cloneable sender end.  Idempotent pushes
//! coalesce; the attend loop drains mid-exchange at a tool boundary
//! ([`Inbox::drain_mid_exchange`]) and parks at the exchange boundary
//! ([`Inbox::next_or_idle`]).
//!
//! The park verdict is computed *under the queue mutex* and *before* the pop.
//! Every fact it reads is either kept under that same mutex (the exchange
//! clock), written only by the consumer's own thread (`Agent`'s status), or
//! changes only *after* a delivery into this queue (a child dies after posting
//! its result) — so a delivery can never lose to the verdict it should wake.

use super::post::{Minted, Source, Stamped};
use super::{Item, Next, Post, ScheduleId};
use crate::cancel;
use ral_core::sync::LockExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How [`Inbox::next_or_idle`] should treat an empty inbox — the verdict
/// `Avatar::park_mode` recomputes on every wake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkMode {
    /// A live human conversation: park *and ignore cancellation* entirely, for
    /// an Esc cancels the exchange, not the agent.
    Held,
    /// A non-conversing agent a human has exchanged with, per its own exchange
    /// clock rather than the TUI's focus cursor.  Parks like [`Self::Held`] but
    /// with no immunity, or a `HeldByChildren` parent would wait forever on the
    /// cancelled result it is owed; the fleet's idle lease bounds it.
    Engaged,
    /// Live children will each deliver a result up this inbox, so park rather
    /// than kill a headless root waiting on its fleet; the last one settling
    /// drops the next verdict to [`Self::Quiesce`].
    HeldByChildren,
    /// An armed self-schedule may fire a wakeup — park, but a terminate-cause
    /// cancel still stops now rather than wait for it.
    UntilCancelled,
    /// Nothing will ever feed this agent again: terminate at quiescence.
    Quiesce,
}

/// The queue an [`Inbox`] and its [`Mailbox`]es share: posts under a mutex,
/// plus a [`Condvar`] a parked `next_or_idle` waits on so a push wakes it
/// without polling.
struct Shared {
    queue: Mutex<Queue>,
    signal: Condvar,
    /// True while the consumer is parked in [`ParkMode::Held`] or
    /// [`ParkMode::Engaged`] on an empty queue.  A producer clears it before
    /// waking the consumer, so a frontend can tell "prompt is editable" from
    /// "the root is still working" without minting a presentation event.
    waiting_for_input: AtomicBool,
    /// Bumped by [`Inbox::clear`] under the queue mutex.  A [`Stamped`]
    /// message — one whose producer cannot judge its own staleness — is
    /// composed elsewhere and pushed as a second step, with a `/clear` free
    /// to fall between, so it arrives through a [`Stamp`] minted at
    /// composition and [`pop_next`] compares that against this counter under
    /// the lock `clear` holds: a push landing before the bump is swept with
    /// the queue, one landing after is refused as stale.  A single `/clear`
    /// gesture may bump this more than once — the TUI's pre-drain
    /// `App::clear` and `Avatar::clear`'s own drain both run on the same
    /// inbox — which only widens refusal, never narrows it.
    epoch: AtomicU64,
}

/// What the queue mutex guards: the posts, and the exchange clock they are
/// stamped against, so an exchange's stamp and its delivery are one atomic
/// step and a park verdict — computed under this same mutex — reads both.
struct Queue {
    posts: VecDeque<Post>,
    /// The last human or parent exchange — [`Mailbox::steer`] or
    /// [`Agent::message`](crate::agent::Agent::message) — that reached this
    /// inbox; `None` before the first.
    last_exchange: Option<Instant>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue {
                posts: VecDeque::new(),
                last_exchange: None,
            }),
            signal: Condvar::new(),
            waiting_for_input: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
        })
    }

    /// Recovers from a poisoned mutex rather than panicking, as
    /// `ScheduleRegistry::lock` does: every operation here is a whole push, pop
    /// or clear, so a panicked holder leaves the deque usable, where
    /// propagating the poison would kill the fleet's inbox for good.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock_ignore_poison()
    }

    fn push(&self, msg: Post) {
        let mut q = self.lock();
        enqueue(&mut q.posts, msg);
        drop(q);
        self.wake();
    }

    /// A delivery that is also an exchange: stamp and enqueue under one
    /// acquisition of the queue mutex.
    fn exchange(&self, msg: Post) {
        let mut q = self.lock();
        q.last_exchange = Some(Instant::now());
        enqueue(&mut q.posts, msg);
        drop(q);
        self.wake();
    }

    fn wake(&self) {
        self.waiting_for_input.store(false, Ordering::Release);
        self.signal.notify_all();
    }
}

/// The push rule.  The idempotent sources coalesce: a wakeup
/// replaces a still-queued one for the same schedule id, a `Nudge` replaces a
/// still-queued nudge (a second means a fresher continuation superseded the
/// first, not that both are owed), and `UserSteering` joins a steering tail
/// entry on a new line.  Every other source queues: each is posted at most
/// once per child, worker, or human keystroke, so the fuel and admission caps
/// that bound those already bound the queue.
fn enqueue(q: &mut VecDeque<Post>, msg: Post) {
    match msg {
        Post::Stamped {
            kind: Stamped::Wakeup { id, .. },
            ..
        } => replace_or_push(q, msg, |m| queued_wakeup_for(m, id)),
        Post::UserSteering(text) => match q.back_mut() {
            Some(Post::UserSteering(s)) => {
                s.push('\n');
                s.push_str(&text);
            }
            _ => q.push_back(Post::UserSteering(text)),
        },
        Post::Nudge { .. } => replace_or_push(q, msg, |m| matches!(m, Post::Nudge { .. })),
        other => q.push_back(other),
    }
}

/// Coalesce: overwrite the queued entry matching `pred` in place, else push.
fn replace_or_push(q: &mut VecDeque<Post>, msg: Post, pred: impl Fn(&Post) -> bool) {
    match q.iter().position(pred) {
        Some(pos) => q[pos] = msg,
        None => q.push_back(msg),
    }
}

/// Whether `m` is a still-queued wakeup for `id` — the predicate [`enqueue`]'s
/// dedupe and [`Mailbox::has_queued_wakeup`] both read.
fn queued_wakeup_for(m: &Post, id: ScheduleId) -> bool {
    matches!(m, Post::Stamped { kind: Stamped::Wakeup { id: eid, .. }, .. } if *eid == id)
}

/// How long a parked [`Inbox::next_or_idle`] sleeps between condvar wakes.  A
/// push notifies immediately; this bound governs only how fast a cancel, which
/// does not notify, is observed.
const PARK_POLL: Duration = Duration::from_millis(100);

/// The cloneable **sender** side of a session's inbox — producers hold this,
/// never the [`Inbox`].
///
/// Every agent's own is reached off it, so the frontend can steer a focused
/// tab and the `message` tool can reach a live agent without exposing raw
/// senders to model code.
#[derive(Clone)]
pub struct Mailbox {
    shared: Arc<Shared>,
}

impl Mailbox {
    /// Post any message, applying the coalesce rule ([`Shared::push`]) and
    /// waking a parked consumer.  Takes the queue mutex, so no caller may hold
    /// a fleet lock: clone the mailbox out and push after the guard drops.
    pub(crate) fn push(&self, msg: Post) {
        self.shared.push(msg);
    }

    /// Post a user-typed steering prompt — the TUI's `Enter`-while-busy path.
    pub(crate) fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt));
    }

    /// The fleet's delivery door for a human message: an exchange, so it
    /// stamps the clock in the same step as it delivers.
    pub(crate) fn steer(&self, text: String) {
        self.exchange(Post::UserSteering(text));
    }

    /// Deliver `msg` as an exchange — a steer, or a parent's marked
    /// [`Post::AgentMessage`] — stamping the clock atomically with the push.
    pub(crate) fn exchange(&self, msg: Post) {
        self.shared.exchange(msg);
    }

    /// Stamp the exchange clock with no delivery, for tests whose scripted
    /// consumer has nothing to answer a steer with.
    #[cfg(test)]
    pub(crate) fn stamp_exchange(&self) {
        self.shared.lock().last_exchange = Some(Instant::now());
    }

    /// The last exchange this inbox witnessed, or `None` before the first.
    pub(crate) fn last_exchange(&self) -> Option<Instant> {
        self.shared.lock().last_exchange
    }

    /// Whether this queue's consumer is parked at a human-input boundary — the
    /// TUI reads the focused tab's to drive the prompt chrome and the spinner.
    pub(crate) fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// This inbox's clear-epoch — the race is [`Shared::epoch`]'s doc.
    pub(crate) fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    /// Whether a wakeup for `id` is still queued, unconsumed — the reaper's
    /// overlap check: a fire finding one still here skips, since the queue's
    /// own dedupe ([`enqueue`]) means at most one is ever waiting.
    pub(crate) fn has_queued_wakeup(&self, id: ScheduleId) -> bool {
        self.shared
            .lock()
            .posts
            .iter()
            .any(|m| queued_wakeup_for(m, id))
    }

    /// Mint the addressed envelope for a message composed now but pushed
    /// later — the only way to send a [`Stamped`] message.
    pub(crate) fn stamp(&self) -> Stamp {
        Stamp {
            epoch: self.epoch(),
            mailbox: self.clone(),
        }
    }
}

/// An addressed envelope: the destination mailbox and its clear-epoch,
/// captured together at composition ([`Mailbox::stamp`]).  Because the pair
/// travels as one value, a stamp can be neither forgotten ([`Stamped`] is
/// unsendable without one), taken from a different inbox than the one that
/// judges it, nor refreshed at push time.
#[derive(Clone)]
pub(crate) struct Stamp {
    mailbox: Mailbox,
    epoch: u64,
}

impl Stamp {
    /// Deliver to the minting mailbox, judged against the minted epoch at
    /// that inbox's own pop ([`pop_next`]).
    pub(crate) fn post(&self, kind: Stamped) {
        self.mailbox.push(Post::Stamped {
            epoch: Minted(self.epoch),
            kind,
        });
    }

    /// Whether the minting inbox has cleared since — the desk's spawn
    /// refusal, with no second epoch read to get wrong.
    pub(crate) fn is_stale(&self) -> bool {
        self.mailbox.epoch() != self.epoch
    }
}

/// A session's inbox: the owned **consumer** the attend loop pulls from, with
/// senders minted by [`Self::mailbox`].  Tool-boundary messages drain
/// mid-exchange ([`Self::drain_mid_exchange`], from `agent::deliberate`), the rest
/// at the exchange boundary ([`Self::next_or_idle`]).
#[derive(Clone)]
pub(crate) struct Inbox {
    shared: Arc<Shared>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    pub(crate) fn new() -> Self {
        Self {
            shared: Shared::new(),
        }
    }

    pub(crate) fn mailbox(&self) -> Mailbox {
        Mailbox {
            shared: self.shared.clone(),
        }
    }

    /// The self-push path — a nudge or a self-armed wakeup landing in the
    /// agent's own box.  Same rule as [`Mailbox::push`].
    pub(crate) fn push(&self, msg: Post) {
        self.shared.push(msg);
    }

    pub(crate) fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt));
    }

    /// Whether anything is queued.  The attend loop's ready boundary reads it
    /// to tell an idle pass from one that already has work in hand.
    pub(crate) fn is_empty(&self) -> bool {
        self.shared.lock().posts.is_empty()
    }

    /// True once the consumer yields on an empty queue, cleared the moment a
    /// producer enqueues work.  The chrome reads a [`Mailbox`]'s, not this.
    pub(crate) fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// Queue depth per source for the `/resources` fold, zeros included so the
    /// row set is stable.  One pass under the lock; nothing is drained or woken.
    pub(crate) fn source_depths(&self) -> Vec<(Source, u64)> {
        let mut rows: Vec<(Source, u64)> = Source::ALL.into_iter().map(|s| (s, 0u64)).collect();
        for msg in &self.shared.lock().posts {
            if let Some(row) = rows.iter_mut().find(|(s, _)| *s == msg.source()) {
                row.1 += 1;
            }
        }
        rows
    }

    /// Everything the human typed and is still waiting on, oldest first, for
    /// the TUI's queue strip: prompts and the commands queued among them, in
    /// the one order they were typed.  A command earns its place because it can
    /// be the reason the rest are waiting — a rewrite holds the queue behind
    /// it, and a strip that showed only prompts would leave that wait with no
    /// visible cause.  Other deliveries stay invisible: they are work, not
    /// queued human text.
    pub(crate) fn queued_human_messages(&self) -> Vec<String> {
        self.shared
            .lock()
            .posts
            .iter()
            .filter_map(|msg| match msg {
                Post::UserSteering(s) => Some(s.clone()),
                Post::Read(r) => Some(r.to_string()),
                Post::Rewrite(r) => Some(r.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Pull every queued user prompt back out for editing, oldest first,
    /// wherever it sits: a prompt queued behind a wakeup is still the user's
    /// draft, and the wakeup, left in place, is not.
    pub(crate) fn pop_back_user_all(&self) -> Option<Vec<String>> {
        let mut guard = self.shared.lock();
        let q = &mut guard.posts;
        let mut prompts: Vec<String> = Vec::new();
        let mut kept: VecDeque<Post> = VecDeque::with_capacity(q.len());
        while let Some(msg) = q.pop_front() {
            match msg {
                Post::UserSteering(s) => prompts.push(s),
                other => kept.push_back(other),
            }
        }
        *q = kept;
        drop(guard);
        (!prompts.is_empty()).then_some(prompts)
    }

    /// Mid-exchange drain at a tool-call boundary: the reads to run and the
    /// deliveries to hand the model, in the one order they were typed.
    ///
    /// The scan stops at a rewrite, which drains only at the exchange boundary
    /// and holds everything typed after it; a [`Next::Rewrite`] is therefore
    /// never among the results.
    pub(crate) fn drain_mid_exchange(&self) -> Vec<Next> {
        let mut guard = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        let mut arrivals = Vec::new();
        while let Some(next) = pop_next(&mut guard.posts, epoch) {
            match next {
                Next::Rewrite(r) => {
                    guard.posts.push_front(Post::Rewrite(r));
                    break;
                }
                read_or_item => arrivals.push(read_or_item),
            }
        }
        drop(guard);
        arrivals
    }

    /// The next exchange-boundary deliverable.  Never blocks —
    /// [`Self::next_or_idle`] is the parking variant the attend loop uses.
    pub(crate) fn next_item(&self) -> Option<Next> {
        let mut q = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        pop_next(&mut q.posts, epoch)
    }

    /// The attend loop's exchange-boundary pull: the next deliverable, or, on
    /// an empty queue, whatever the `park` verdict says — the immunity ladder is
    /// [`ParkMode`]'s doc.  A push wakes the park at once through the condvar;
    /// a cancellation does not notify, so a non-`Held` park re-checks it every
    /// [`PARK_POLL`].
    ///
    /// Two orderings carry the correctness.  The verdict runs *under the queue
    /// mutex*, which the condvar releases atomically, so no push interleaves
    /// between verdict and wait and no wakeup is lost; `park` is handed the
    /// exchange clock's reading from under that same lock.  And it runs
    /// *before* the pop, so a `Quiesce` can never win against a delivery
    /// already queued.
    pub(crate) fn next_or_idle(
        &self,
        park: impl Fn(bool) -> ParkMode,
        cancel: &cancel::Token,
    ) -> Option<Next> {
        let mut q = self.shared.lock();
        loop {
            let mode = park(q.last_exchange.is_some());
            // A *terminate*-cause cancel ends every park but `Held`; an
            // *interrupt* drops the in-flight exchange and the agent re-parks.
            // Only a live human conversation ignores cancellation entirely.
            if mode != ParkMode::Held && cancel.terminated() {
                return None;
            }
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            if let Some(next) = pop_next(&mut q.posts, epoch) {
                self.shared
                    .waiting_for_input
                    .store(false, Ordering::Release);
                return Some(next);
            }
            if mode == ParkMode::Quiesce {
                self.shared
                    .waiting_for_input
                    .store(false, Ordering::Release);
                return None;
            }
            self.shared.waiting_for_input.store(
                matches!(mode, ParkMode::Held | ParkMode::Engaged),
                Ordering::Release,
            );
            let (guard, _timeout) = self
                .shared
                .signal
                .wait_timeout(q, PARK_POLL)
                .unwrap_or_else(PoisonError::into_inner);
            q = guard;
        }
    }

    /// Drop every pending message — `/clear` rebuilds the agent, so nothing
    /// queued carries across.  A queued-but-unconsumed wakeup needs no
    /// separate release: dropping it from the queue is itself what
    /// [`Mailbox::has_queued_wakeup`] will see.  Bumps the clear-epoch under
    /// the same lock as the drain ([`Shared::epoch`]).
    pub(crate) fn clear(&self) {
        let mut q = self.shared.lock();
        q.posts.clear();
        self.shared.epoch.fetch_add(1, Ordering::Release);
        drop(q);
    }

    /// Drop queued self-nudges while preserving user, command, and worker
    /// messages.  A nudge continues the exchange it was decided in, so a
    /// rewind that removes that exchange, or a new exchange opening ahead of
    /// it, makes it stale.
    pub(crate) fn drop_nudges(&self) {
        remove_where(&mut self.shared.lock().posts, |msg| {
            matches!(msg, Post::Nudge { .. })
        });
    }
}

/// Remove every entry matching `pred`, then restore [`enqueue`]'s invariant:
/// no two steering entries adjacent.  A removal from the middle is the one
/// thing that can break it.
fn remove_where(q: &mut VecDeque<Post>, pred: impl Fn(&Post) -> bool) {
    let before = q.len();
    q.retain(|m| !pred(m));
    if q.len() == before {
        return;
    }
    let mut merged: VecDeque<Post> = VecDeque::with_capacity(q.len());
    for msg in q.drain(..) {
        enqueue(&mut merged, msg);
    }
    *q = merged;
}

/// Pop the front of a locked queue.  Stale stamped posts are fenced first,
/// against the caller's read of the clear-epoch taken under this same lock.
pub(super) fn pop_next(q: &mut VecDeque<Post>, epoch: u64) -> Option<Next> {
    remove_where(q, |m| m.stale(epoch));
    q.pop_front().map(to_next)
}

/// Convert one already-fenced post into what it delivers.
fn to_next(msg: Post) -> Next {
    match msg {
        Post::Stamped { kind, .. } => Next::Item(match kind {
            Stamped::Wakeup {
                label,
                trigger,
                prompt,
                ..
            } => Item::Wakeup(format!("[scheduled '{label}' · {trigger}] {prompt}")),
            Stamped::AgentResult(line) => Item::Agent(line),
            Stamped::Surface { id, values } => Item::Surface { id, values },
        }),
        Post::AgentMessage(m) => Next::Item(Item::Message(m)),
        Post::Nudge { prompt, text } => Next::Item(Item::Nudge { prompt, text }),
        Post::Read(r) => Next::Read(r),
        Post::Rewrite(r) => Next::Rewrite(r),
        Post::UserSteering(s) => Next::Item(Item::Human(s)),
    }
}

#[cfg(test)]
mod tests;
