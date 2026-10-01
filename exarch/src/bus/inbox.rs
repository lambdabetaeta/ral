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
use super::{Item, Next, Post};
use crate::agent::cancel;
use crate::fleet::schedule::ScheduleId;
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

    /// The minted epoch; test assertions only — production judges
    /// staleness at the pop or through [`Self::is_stale`].
    #[cfg(test)]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
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
mod tests {
    use super::{Inbox, Item, Minted, Next, ParkMode, Post, Source, Stamped};
    use crate::agent::cancel;
    use crate::bus::{AgentMessage, AgentOutcome, AgentResult};
    use crate::bus::{Read, Rewrite};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// The deliveries of one mid-exchange drain, for tests that queue no reads.
    fn drained(inbox: &Inbox) -> Vec<Item> {
        inbox
            .drain_mid_exchange()
            .into_iter()
            .map(|next| match next {
                Next::Item(item) => item,
                other => panic!("expected a delivery, got {other:?}"),
            })
            .collect()
    }

    /// Epoch 0 is a fresh [`Inbox`]'s own; `id` matters only to the dedupe tests.
    fn wakeup(id: u64, label: &str, trigger: &str, prompt: &str) -> Post {
        wakeup_at(id, label, trigger, prompt, 0)
    }

    /// [`wakeup`] with an explicit epoch, for the stale-admission tests.
    /// Forging a [`Minted`] is possible only here inside `bus`; everyone
    /// else sends through a [`Stamp`].
    fn wakeup_at(id: u64, label: &str, trigger: &str, prompt: &str, epoch: u64) -> Post {
        Post::Stamped {
            epoch: Minted(epoch),
            kind: Stamped::Wakeup {
                id,
                label: label.into(),
                trigger: trigger.into(),
                prompt: prompt.into(),
            },
        }
    }

    fn eventually(timeout: Duration, pred: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn inbox_waiting_for_input_tracks_human_park() {
        let inbox = Inbox::new();
        assert!(
            inbox.waiting_for_input(),
            "a fresh interactive inbox starts at the human boundary"
        );

        inbox.push_user("work".into());
        assert!(
            !inbox.waiting_for_input(),
            "posting input wakes the consumer out of the yielded state"
        );
        assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "work"));
        assert!(
            !inbox.waiting_for_input(),
            "draining an item means work has started; yield resumes only at park"
        );

        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token;
        let handle = std::thread::spawn(move || {
            worker_inbox.next_or_idle(|_| ParkMode::Held, &worker_token)
        });

        assert!(
            eventually(Duration::from_secs(1), || inbox.waiting_for_input()),
            "a Held empty-inbox park is the human-input yield point"
        );

        inbox.mailbox().push_user("next".into());
        assert!(
            !inbox.waiting_for_input(),
            "a submitted prompt clears the yielded bit before waking the worker"
        );
        assert!(
            matches!(handle.join().expect("parked worker joins"), Some(Next::Item(Item::Human(s))) if s == "next"),
            "the wakeup delivered the submitted prompt"
        );
        assert!(
            !inbox.waiting_for_input(),
            "taking the item leaves the root working until it parks again"
        );
    }

    #[test]
    fn inbox_waiting_for_input_ignores_non_human_parks() {
        let inbox = Inbox::new();
        inbox.push_user("work".into());
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Human(_)))
        ));
        assert!(!inbox.waiting_for_input());

        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            worker_inbox.next_or_idle(
                |_| {
                    worker_observed.store(true, Ordering::Release);
                    ParkMode::HeldByChildren
                },
                &worker_token,
            )
        });

        assert!(
            eventually(Duration::from_secs(1), || observed.load(Ordering::Acquire)),
            "the worker reached the park predicate"
        );
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !inbox.waiting_for_input(),
            "waiting on children is still work, not a human-input yield"
        );

        token.cancel(ral_core::process::CancelCause::Explicit);
        assert!(
            handle.join().expect("cancelled worker joins").is_none(),
            "non-human parks terminate on cancellation"
        );
    }

    /// The complement of the test above: an *interrupt* drops the in-flight
    /// exchange rather than ending the agent.  Proved without a timing race —
    /// after the interrupt the only exit left is a pushed item, so getting it
    /// back through the join is the evidence a terminate would have destroyed.
    #[test]
    fn non_human_park_survives_an_interrupt() {
        let inbox = Inbox::new();
        inbox.push_user("work".into());
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Human(_)))
        ));

        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            worker_inbox.next_or_idle(
                |_| {
                    worker_observed.store(true, Ordering::Release);
                    ParkMode::HeldByChildren
                },
                &worker_token,
            )
        });

        assert!(
            eventually(Duration::from_secs(1), || observed.load(Ordering::Acquire)),
            "the worker reached the park predicate"
        );

        token.cancel(ral_core::process::CancelCause::Interrupt);

        inbox.mailbox().push_user("resume".into());
        assert!(
            matches!(
                handle.join().expect("parked worker joins"),
                Some(Next::Item(Item::Human(s))) if s == "resume"
            ),
            "the interrupt was ignored; the pushed item released the park"
        );
    }

    /// [`ParkMode::Engaged`] grants no immunity: were it to, its
    /// `HeldByChildren` parent would wait forever on a cancelled result.
    #[test]
    fn engaged_park_dies_on_a_terminate_cause_despite_the_exchange() {
        let inbox = Inbox::new();
        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            inbox.next_or_idle(
                |_| {
                    worker_observed.store(true, Ordering::Release);
                    ParkMode::Engaged
                },
                &worker_token,
            )
        });

        assert!(
            eventually(Duration::from_secs(1), || observed.load(Ordering::Acquire)),
            "the worker reached the park predicate"
        );

        token.cancel(ral_core::process::CancelCause::Explicit);
        assert!(
            handle.join().expect("cancelled worker joins").is_none(),
            "a terminate cause ends an Engaged park despite the exchange"
        );
    }

    /// The complement: a conversing [`ParkMode::Held`] stays immune even to a
    /// terminate cause, proved as [`non_human_park_survives_an_interrupt`] is.
    #[test]
    fn held_park_survives_a_terminate_cause() {
        let inbox = Inbox::new();
        inbox.push_user("work".into());
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Human(_)))
        ));

        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            worker_inbox.next_or_idle(
                |_| {
                    worker_observed.store(true, Ordering::Release);
                    ParkMode::Held
                },
                &worker_token,
            )
        });

        assert!(
            eventually(Duration::from_secs(1), || observed.load(Ordering::Acquire)),
            "the worker reached the park predicate"
        );

        token.cancel(ral_core::process::CancelCause::Explicit);

        inbox.mailbox().push_user("resume".into());
        assert!(
            matches!(
                handle.join().expect("parked worker joins"),
                Some(Next::Item(Item::Human(s))) if s == "resume"
            ),
            "a live conversation ignores even a terminate cause"
        );
    }

    /// A wakeup reaches the model as soon as the tool batch settles.
    #[test]
    fn inbox_wakeup_drains_at_tool_boundary_marked() {
        let inbox = Inbox::new();
        inbox.push_user("steer".into());
        inbox.push(wakeup(1, "nightly", "0 3 * * *", "run the tests"));

        assert!(
            matches!(
                drained(&inbox).as_slice(),
                [Item::Human(h), Item::Wakeup(w)]
                    if h == "steer"
                        && w == "[scheduled 'nightly' · 0 3 * * *] run the tests",
            ),
            "the wakeup drains mid-exchange, marked, after the steering",
        );
        assert!(inbox.is_empty());
    }

    /// Async deliveries drain in queue order too, so a result settling during a
    /// long tool-call loop need not wait for the exchange to end.
    #[test]
    fn inbox_tool_drain_takes_async_deliveries() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "nightly", "@", "go"));
        inbox.push_user("redirect now".into());
        inbox.push_user("and also this".into());

        assert!(
            matches!(
                drained(&inbox).as_slice(),
                [Item::Wakeup(_), Item::Human(s)] if s == "redirect now\nand also this",
            ),
            "the async wakeup and the coalesced steering both drain, in order",
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_agent_message_drains_marked_at_tool_boundary() {
        let inbox = Inbox::new();
        inbox.push(Post::AgentMessage(AgentMessage {
            from: 7,
            from_name: "review".into(),
            text: "please inspect the parser branch".into(),
        }));

        assert!(matches!(
            drained(&inbox).as_slice(),
            [Item::Message(m)]
                if m.from == 7
                    && m.from_name == "review"
                    && m.text == "please inspect the parser branch"
                    && m.render()
                        == "[EXARCH AGENT 7 MESSAGE: review]\nplease inspect the parser branch\n[/EXARCH]"
        ));
        assert!(inbox.is_empty());
    }

    /// A `/rewind` is a barrier: it drops the very context a prompt typed after
    /// it would land in, so nothing queued behind it drains until it has run.
    /// Deliveries ahead of it still drain.
    #[test]
    fn inbox_tool_drain_stops_at_a_barrier() {
        let inbox = Inbox::new();
        inbox.push_user("before".into());
        inbox.push(wakeup(1, "x", "@", "p"));
        inbox.push(Post::Rewrite(Rewrite::Rewind(7)));
        inbox.push_user("after the rewind".into());

        assert!(matches!(
            drained(&inbox).as_slice(),
            [Item::Human(b), Item::Wakeup(_)] if b == "before"
        ));
        assert!(drained(&inbox).is_empty());
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Rewrite(Rewrite::Rewind(7)))
        ));
        assert!(
            matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "after the rewind")
        );
        assert!(inbox.is_empty());
    }

    /// A read drains mid-exchange in the order typed, beside the prompts; a
    /// rewrite holds the scan, and everything behind it waits for the exchange
    /// boundary.
    #[test]
    fn inbox_mid_exchange_drain_takes_reads_in_order_and_holds_at_a_rewrite() {
        let inbox = Inbox::new();
        inbox.push(Post::Read(Read::Resources));
        inbox.push_user("a".into());
        inbox.push(Post::Rewrite(Rewrite::Evict));
        inbox.push_user("b".into());

        assert!(matches!(
            inbox.drain_mid_exchange().as_slice(),
            [Next::Read(Read::Resources), Next::Item(Item::Human(s))] if s == "a"
        ));
        assert!(inbox.drain_mid_exchange().is_empty(), "the rewrite holds");
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Rewrite(Rewrite::Evict))
        ));
        assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "b"));
        assert!(inbox.is_empty());
    }

    /// The queue strip is a projection of what the human typed, not an inbox
    /// debugger: prompts and commands alike, in the order they were typed, and
    /// a wakeup — which nobody typed — stays out.
    #[test]
    fn inbox_queued_human_messages_shows_typed_text_in_order() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "morning", "@daily", "check"));
        inbox.push_user("first".into());
        inbox.push(Post::Read(Read::Branch(Some("scout".into()))));
        inbox.push_user("second".into());

        assert_eq!(
            inbox.queued_human_messages(),
            vec![
                "first".to_string(),
                "/branch scout".to_string(),
                "second".to_string()
            ]
        );
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Wakeup(_)))
        ));
        assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "first"));
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Read(Read::Branch(Some(s)))) if s == "scout"
        ));
        assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "second"));
    }

    /// A sole wakeup is not the user's draft, so nothing comes back.
    #[test]
    fn inbox_pop_back_user_all_no_user_prompts() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "x", "@", "p"));
        assert_eq!(inbox.pop_back_user_all(), None, "no user prompts to recall");
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Wakeup(_)))
        ));
    }

    /// Even prompts sandwiched between non-user deliveries, which keep their
    /// order.  "second" and "third" arrive back-to-back, so the push-time merge
    /// already folded them into one entry.
    #[test]
    fn inbox_pop_back_user_all_extracts_all_leaving_non_user_in_order() {
        let inbox = Inbox::new();
        inbox.push_user("first".into());
        inbox.push(wakeup(1, "x", "@", "p"));
        inbox.push_user("second".into());
        inbox.push_user("third".into());
        inbox.push(Post::Read(Read::Resources));
        inbox.push_user("fourth".into());
        assert_eq!(
            inbox.pop_back_user_all(),
            Some(vec![
                "first".to_string(),
                "second\nthird".to_string(),
                "fourth".to_string(),
            ]),
            "all user prompts come back oldest-first, past interspersed deliveries",
        );
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Wakeup(_)))
        ));
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Read(Read::Resources))
        ));
        assert!(inbox.is_empty());
    }

    /// A `spawn` worker's batch, terminated by the `` `done `` event core
    /// appends, stamped with `epoch` — a live one for the ordinary path, a
    /// stale one to exercise the pop-time fence.
    fn surface(epoch: u64) -> Post {
        use crate::bus::card::testkit::{map_value, s, variant};
        let done = variant(
            "done",
            map_value(vec![
                ("cmd", s("<block>")),
                ("outcome", variant("ok", ral_core::serial::FOValue::Unit)),
            ]),
        );
        Post::Stamped {
            epoch: Minted(epoch),
            kind: Stamped::Surface {
                id: 0,
                values: vec![done],
            },
        }
    }

    /// A `/clear` landing between delivery and drain empties the deque.
    #[test]
    fn inbox_surface_drains_at_tool_boundary_and_cleared() {
        let inbox = Inbox::new();
        inbox.push(surface(inbox.mailbox().epoch()));
        inbox.clear();
        assert!(
            drained(&inbox).is_empty(),
            "a /clear drops the queued batch"
        );

        inbox.push(surface(inbox.mailbox().epoch()));
        assert!(matches!(
            drained(&inbox).as_slice(),
            [Item::Surface { id, .. }] if *id == 0
        ));
    }

    /// Draining a wakeup re-opens its schedule for the next occurrence: the
    /// queue itself is the overlap-check's source of truth, so popping the
    /// item is all `has_queued_wakeup` needs to flip.
    #[test]
    fn inbox_wakeup_leaves_the_queue_once_drained() {
        let inbox = Inbox::new();
        let mailbox = inbox.mailbox();
        mailbox.stamp().post(Stamped::Wakeup {
            id: 1,
            label: "n".into(),
            trigger: "* * * * *".into(),
            prompt: "go".into(),
        });
        assert!(mailbox.has_queued_wakeup(1));
        let _ = inbox.next_item();
        assert!(
            !mailbox.has_queued_wakeup(1),
            "draining the wakeup re-opens its schedule"
        );
    }

    /// `clear` drops the queue outright, so an unconsumed wakeup is not
    /// stranded as "still queued" forever.  `Avatar::clear` disarms the
    /// schedule registry alongside, but the TUI's `App::clear` reaches only
    /// the inbox, so this must hold on its own.
    #[test]
    fn inbox_clear_leaves_no_wakeup_stranded_as_queued() {
        let inbox = Inbox::new();
        let mailbox = inbox.mailbox();
        mailbox.stamp().post(Stamped::Wakeup {
            id: 1,
            label: "n".into(),
            trigger: "* * * * *".into(),
            prompt: "go".into(),
        });
        inbox.clear();
        assert!(
            !mailbox.has_queued_wakeup(1),
            "clear must not strand a wakeup as still queued"
        );
        assert!(inbox.is_empty());
    }

    /// The reaper's compose-then-push race (`ScheduleRegistry::fire`): a wakeup
    /// composed under an epoch an intervening `/clear` has since bumped never
    /// surfaces into the rebuilt context.
    #[test]
    fn stale_epoch_wakeup_is_refused_at_pop() {
        let inbox = Inbox::new();
        let stale = inbox.mailbox().epoch();
        inbox.clear();
        inbox.push(wakeup_at(1, "n", "@", "go", stale));
        assert!(
            inbox.next_item().is_none(),
            "a wakeup stamped with an epoch older than the inbox's own is dropped"
        );
    }

    /// The positive half: the live epoch is delivered like any other message.
    #[test]
    fn current_epoch_wakeup_is_delivered() {
        let inbox = Inbox::new();
        let live = inbox.mailbox().epoch();
        inbox.push(wakeup_at(1, "n", "@", "go", live));
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Wakeup(_)))
        ));
    }

    /// The same one-rule fence, over an `AgentResult` instead of a wakeup.
    #[test]
    fn stale_epoch_agent_result_is_refused_at_pop() {
        let inbox = Inbox::new();
        let stamp = inbox.mailbox().stamp();
        inbox.clear();
        stamp.post(Stamped::AgentResult(AgentResult {
            id: 1,
            name: "worker".into(),
            outcome: AgentOutcome::Stopped("done".into()),
            elapsed: Duration::ZERO,
        }));
        assert!(
            inbox.next_item().is_none(),
            "an agent result stamped with an epoch older than the inbox's own is dropped"
        );
    }

    /// The positive half: a live-epoch `AgentResult` is delivered.
    #[test]
    fn current_epoch_agent_result_is_delivered() {
        let inbox = Inbox::new();
        inbox
            .mailbox()
            .stamp()
            .post(Stamped::AgentResult(AgentResult {
                id: 1,
                name: "worker".into(),
                outcome: AgentOutcome::Stopped("done".into()),
                elapsed: Duration::ZERO,
            }));
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Agent(_)))
        ));
    }

    /// The same one-rule fence, over a `Surface` batch instead of a wakeup.
    #[test]
    fn stale_epoch_surface_batch_is_refused_at_pop() {
        let inbox = Inbox::new();
        let stale = inbox.mailbox().epoch();
        inbox.clear();
        inbox.push(surface(stale));
        assert!(
            inbox.next_item().is_none(),
            "a surface batch stamped with an epoch older than the inbox's own is dropped"
        );
    }

    /// The positive half: a live-epoch `Surface` batch is delivered.
    #[test]
    fn current_epoch_surface_batch_is_delivered() {
        let inbox = Inbox::new();
        let live = inbox.mailbox().epoch();
        inbox.push(surface(live));
        assert!(matches!(
            inbox.next_item(),
            Some(Next::Item(Item::Surface { .. }))
        ));
    }

    // ── inbox quotas without silent loss ───────────────────────────────────

    fn depth_of(inbox: &Inbox, source: Source) -> u64 {
        inbox
            .source_depths()
            .into_iter()
            .find(|(s, _)| *s == source)
            .map_or(0, |(_, n)| n)
    }

    /// One entry, not two, and a different schedule's is untouched and keeps
    /// its arrival order.
    #[test]
    fn inbox_scheduled_wakeup_dedupes_by_schedule_id_newest_wins() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "nightly", "@daily", "first"));
        inbox.push(wakeup(1, "nightly", "@daily", "second"));
        inbox.push(wakeup(2, "morning", "@daily", "other schedule"));
        assert_eq!(
            depth_of(&inbox, Source::Schedule),
            2,
            "schedule 1 replaced in place; schedule 2 is its own entry"
        );
        match inbox.next_item() {
            Some(Next::Item(Item::Wakeup(text))) => assert!(
                text.contains("second") && !text.contains("first"),
                "the newest wakeup for schedule 1 wins: {text}"
            ),
            _ => panic!("expected schedule 1's (replaced) wakeup first"),
        }
        match inbox.next_item() {
            Some(Next::Item(Item::Wakeup(text))) => assert!(text.contains("other schedule")),
            _ => panic!("expected schedule 2's wakeup, arrival order preserved"),
        }
    }

    /// Otherwise a fast typist grows the queue one entry per line.
    #[test]
    fn inbox_user_steering_merges_pre_boundary_preserving_order() {
        let inbox = Inbox::new();
        inbox.push_user("first line".into());
        inbox.push_user("second line".into());
        assert_eq!(
            depth_of(&inbox, Source::User),
            1,
            "consecutive steering merges into one entry at push time"
        );
        match inbox.next_item() {
            Some(Next::Item(Item::Human(text))) => {
                assert_eq!(
                    text, "first line\nsecond line",
                    "both texts survive in order"
                );
            }
            _ => panic!("expected a merged Human item"),
        }
    }

    /// The agent self-pushes a nudge per deliberation, so a second one means a
    /// fresher continuation superseded the first, not that both are owed.
    #[test]
    fn inbox_nudge_replaces_a_still_queued_one_newest_wins() {
        let inbox = Inbox::new();
        inbox.push(Post::Nudge {
            prompt: 1,
            text: "retry".into(),
        });
        inbox.push(Post::Nudge {
            prompt: 1,
            text: "retry".into(),
        });
        inbox.push(Post::Nudge {
            prompt: 2,
            text: "different".into(),
        });
        assert_eq!(
            depth_of(&inbox, Source::Nudge),
            1,
            "a nudge never grows past one outstanding entry"
        );
        assert!(
            matches!(inbox.next_item(), Some(Next::Item(Item::Nudge { text, .. })) if text == "different"),
            "the newest nudge is the one delivered"
        );
    }

    /// Dropping the nudge between two steering lines leaves them adjacent, and
    /// the inbox restores the push-time merge rather than deliver them apart.
    #[test]
    fn inbox_drop_nudges_remerges_the_steering_it_leaves_adjacent() {
        let inbox = Inbox::new();
        inbox.push_user("first".into());
        inbox.push(Post::Nudge {
            prompt: 1,
            text: "retry".into(),
        });
        inbox.push_user("second".into());
        inbox.drop_nudges();
        assert_eq!(depth_of(&inbox, Source::User), 1);
        assert!(
            matches!(inbox.next_item(), Some(Next::Item(Item::Human(text))) if text == "first\nsecond")
        );
        assert!(inbox.is_empty());
    }
}
