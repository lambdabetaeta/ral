//! A session's typed, multi-producer inbound queue: [`Inbox`] is the owned
//! consumer end, [`Mailbox`] the cloneable sender end.  Idempotent pushes
//! coalesce; the attend loop drains mid-exchange at a tool boundary
//! ([`Inbox::drain_steering`]) and parks at the exchange boundary
//! ([`Inbox::next_or_idle`]).
//!
//! The park verdict is computed *under the queue mutex* and *before* the pop.
//! Every fact it reads is either kept under that same mutex (the exchange
//! clock), written only by the consumer's own thread (`Agent`'s status), or
//! changes only *after* a delivery into this queue (a child dies after posting
//! its result) — so a delivery can never lose to the verdict it should wake.

use super::post::{Boundary, Source, is_slash};
use super::{Item, Post};
use crate::agent::cancel;
use crate::sync::LockExt;
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
    /// Bumped by [`Inbox::clear`] under the queue mutex.  A `ScheduledWakeup`
    /// is composed on the reaper thread and pushed as a second step, with a
    /// `/clear` free to fall between, so it stamps [`Mailbox::epoch`] at
    /// compose time and [`pop_item`] compares that against this counter under
    /// the lock `clear` holds: a push landing before the bump is swept with the
    /// queue, one landing after is refused as stale.
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

/// The push rule.  The idempotent sources coalesce: a `ScheduledWakeup`
/// replaces a still-queued one for the same schedule id, a `Nudge` replaces a
/// still-queued nudge (a second means a fresher continuation superseded the
/// first, not that both are owed), and `UserSteering` joins a non-slash tail
/// entry on a new line — never across a slash line, whose exchange-boundary
/// classification ([`Post::boundary`]) must survive the merge.  Every other
/// source queues: each is posted at most once per child, worker, or human
/// keystroke, so the fuel and admission caps that bound those already bound
/// the queue.
fn enqueue(q: &mut VecDeque<Post>, msg: Post) {
    match msg {
            Post::ScheduledWakeup { id, .. } => {
                let existing = q
                    .iter()
                    .position(|m| matches!(m, Post::ScheduledWakeup { id: eid, .. } if *eid == id));
                match existing {
                    Some(pos) => q[pos] = msg,
                    None => q.push_back(msg),
                }
            }
            Post::UserSteering(text) => {
                let merge = !is_slash(&text)
                    && matches!(q.back(), Some(Post::UserSteering(s)) if !is_slash(s));
                if merge {
                    if let Some(Post::UserSteering(s)) = q.back_mut() {
                        s.push('\n');
                        s.push_str(&text);
                    }
                } else {
                    q.push_back(Post::UserSteering(text));
                }
            }
            Post::Nudge { .. } => {
                let existing = q.iter().position(|m| matches!(m, Post::Nudge { .. }));
                match existing {
                    Some(pos) => q[pos] = msg,
                    None => q.push_back(msg),
                }
            }
            other => q.push_back(other),
        }
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
}

/// A session's inbox: the owned **consumer** the attend loop pulls from, with
/// senders minted by [`Self::mailbox`].  Tool-boundary messages drain
/// mid-exchange ([`Self::drain_steering`], from `agent::deliberate`), the rest
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
    /// be the reason the rest are waiting — a [`Boundary::Barrier`] holds the
    /// queue behind it, and a strip that showed only prompts would leave that
    /// wait with no visible cause.  Other deliveries stay invisible: they are
    /// work, not queued human text.
    pub(crate) fn queued_human_messages(&self) -> Vec<String> {
        self.shared
            .lock()
            .posts
            .iter()
            .filter_map(|msg| match msg {
                Post::UserSteering(s) | Post::Command(s) | Post::Barrier(s) => Some(s.clone()),
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

    /// Mid-exchange drain at a tool-call boundary: the tool-boundary messages
    /// that may reach the model now, in order, each tagged with its source so
    /// the attend loop renders it in its honest medium.  A consecutive run of
    /// user steering coalesces into one [`Item::Human`].
    ///
    /// A [`Boundary::Exchange`] entry — a session-reading command — is left
    /// where it lies for [`Self::next_or_idle`] and the scan goes *on past* it:
    /// it changes nothing about what a prompt behind it means, so making that
    /// prompt wait out the whole exchange buys nothing and costs the human the
    /// turn they were trying to steer.
    ///
    /// The scan stops at the first [`Boundary::Barrier`], where the human's
    /// ordering is the meaning: `/rewind` then a prompt must not answer the
    /// prompt in the context the rewind is about to drop.
    ///
    /// # Panics
    /// Never: every removal follows a same-iteration check of the same entry.
    pub(crate) fn drain_steering(&self) -> Vec<Item> {
        let mut guard = self.shared.lock();
        let q = &mut guard.posts;
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        let mut items = Vec::new();
        // Rebuilt rather than popped: a passed-over entry stays queued, so what
        // this takes is not always a prefix.
        let mut kept: VecDeque<Post> = VecDeque::with_capacity(q.len());
        while let Some(front) = q.front() {
            match front.boundary() {
                Boundary::Barrier => break,
                Boundary::Exchange => {
                    kept.push_back(q.pop_front().expect("front present"));
                }
                Boundary::Tool => {
                    if matches!(front, Post::UserSteering(_)) {
                        items.push(coalesce_steering(q));
                    } else {
                        let msg = q.pop_front().expect("front present and tool-boundary");
                        if let Some(item) = to_item(msg, epoch) {
                            items.push(item);
                        }
                    }
                }
            }
        }
        while let Some(msg) = kept.pop_back() {
            q.push_front(msg);
        }
        drop(guard);
        items
    }

    /// The next exchange-boundary deliverable.  Never blocks —
    /// [`Self::next_or_idle`] is the parking variant the attend loop uses.
    pub(crate) fn next_item(&self) -> Option<Item> {
        let mut q = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        pop_item(&mut q.posts, epoch)
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
    ) -> Option<Item> {
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
            if let Some(item) = pop_item(&mut q.posts, epoch) {
                self.shared
                    .waiting_for_input
                    .store(false, Ordering::Release);
                return Some(item);
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
    /// queued carries across — running each one's [`Post::on_drain`] first, so
    /// a queued-but-unconsumed `ScheduledWakeup` still clears its `pending` flag
    /// rather than stranding a schedule that could never fire again.  Bumps the
    /// clear-epoch under the same lock as the drain ([`Shared::epoch`]).
    pub(crate) fn clear(&self) {
        let mut q = self.shared.lock();
        for msg in q.posts.drain(..) {
            msg.on_drain();
        }
        self.shared.epoch.fetch_add(1, Ordering::Release);
        drop(q);
    }

    /// Drop queued self-nudges while preserving user, command, and worker
    /// messages. A rewind makes a nudge for its removed exchange stale.
    pub(crate) fn drop_nudges(&self) {
        self.shared
            .lock()
            .posts
            .retain(|msg| !matches!(msg, Post::Nudge { .. }));
    }
}

/// Pop the next exchange-boundary item from a locked queue, tagged with its
/// source.  A leading run of *non-slash* steering coalesces into one
/// [`Item::Human`], matching the push-time never-merge rule
/// ([`Shared::try_push`]) — a slash line is always delivered alone, as ordinary
/// prompt text — and every other source is delivered on its own.
///
/// `epoch` is the caller's read of the clear-epoch, taken under the lock this
/// runs under ([`Shared::epoch`]); a stale `ScheduledWakeup` is dropped rather
/// than converted, so the loop just tries the next queued message.
pub(super) fn pop_item(q: &mut VecDeque<Post>, epoch: u64) -> Option<Item> {
    loop {
        if let Post::UserSteering(front) = q.front()? {
            if is_slash(front) {
                let Some(Post::UserSteering(s)) = q.pop_front() else {
                    unreachable!("front just checked to be a slash UserSteering")
                };
                return Some(Item::Human(s));
            }
            return Some(coalesce_steering(q));
        }
        let msg = q.pop_front().expect("front checked present");
        if let Some(item) = to_item(msg, epoch) {
            return Some(item);
        }
    }
}

/// Pop the leading run of consecutive, non-slash [`Post::UserSteering`] entries
/// and join them one per line — the coalesce half of the never-merge rule.
/// Both callers enter with a non-slash steering at the front, so one always pops.
fn coalesce_steering(q: &mut VecDeque<Post>) -> Item {
    let mut text = String::new();
    while let Some(Post::UserSteering(s)) = q.front() {
        if is_slash(s) {
            break;
        }
        let Some(Post::UserSteering(s)) = q.pop_front() else {
            unreachable!("front just checked to be user steering")
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&s);
    }
    Item::Human(text)
}

/// Convert one non-steering message into the [`Item`] it delivers, running its
/// [`Post::on_drain`] — or `None` for a `ScheduledWakeup` whose stamped epoch
/// has fallen behind `epoch`, refused rather than converted.
fn to_item(msg: Post, epoch: u64) -> Option<Item> {
    msg.on_drain();
    if let Post::ScheduledWakeup { epoch: fired, .. } = &msg
        && *fired != epoch
    {
        return None;
    }
    Some(match msg {
        Post::ScheduledWakeup {
            label,
            trigger,
            prompt,
            ..
        } => Item::Wakeup(format!("[scheduled '{label}' · {trigger}] {prompt}")),
        Post::AgentResult(r) => Item::Agent(r),
        Post::AgentMessage(m) => Item::Message(m),
        Post::Nudge { exchange, text } => Item::Nudge { exchange, text },
        Post::Command(s) | Post::Barrier(s) => Item::Command(s),
        Post::Surface {
            id,
            values,
            generation,
        } => Item::Surface {
            id,
            values,
            generation,
        },
        Post::UserSteering(_) => {
            unreachable!("user steering coalesced by the caller")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{Boundary, Inbox, Item, ParkMode, Post, Source};
    use crate::agent::cancel;
    use crate::bus::AgentMessage;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Epoch 0 is a fresh [`Inbox`]'s own; `id` matters only to the dedupe tests.
    fn wakeup(id: u64, label: &str, trigger: &str, prompt: &str) -> Post {
        wakeup_at(id, label, trigger, prompt, 0)
    }

    /// [`wakeup`] with an explicit epoch, for the stale-admission tests.
    fn wakeup_at(id: u64, label: &str, trigger: &str, prompt: &str, epoch: u64) -> Post {
        Post::ScheduledWakeup {
            id,
            label: label.into(),
            trigger: trigger.into(),
            prompt: prompt.into(),
            pending: Arc::new(AtomicBool::new(true)),
            epoch,
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
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "work"));
        assert!(
            !inbox.waiting_for_input(),
            "draining an item means work has started; yield resumes only at park"
        );

        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token;
        let handle =
            std::thread::spawn(move || worker_inbox.next_or_idle(|_| ParkMode::Held, &worker_token));

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
            matches!(handle.join().expect("parked worker joins"), Some(Item::Human(s)) if s == "next"),
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
        assert!(matches!(inbox.next_item(), Some(Item::Human(_))));
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
        assert!(matches!(inbox.next_item(), Some(Item::Human(_))));

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
                Some(Item::Human(s)) if s == "resume"
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
        assert!(matches!(inbox.next_item(), Some(Item::Human(_))));

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
                Some(Item::Human(s)) if s == "resume"
            ),
            "a live conversation ignores even a terminate cause"
        );
    }

    /// A slash-prefixed line waits for the exchange boundary like a real
    /// [`Post::Command`], yet is delivered as ordinary prompt text.
    #[test]
    fn inbox_tool_drain_stops_before_slash_command() {
        let inbox = Inbox::new();
        inbox.push_user("steer first".into());
        inbox.push_user("/clear".into());
        inbox.push_user("after clear".into());

        assert!(
            matches!(inbox.drain_steering().as_slice(), [Item::Human(s)] if s == "steer first"),
            "the non-slash steering drains; the slash line stops the run",
        );
        assert!(inbox.drain_steering().is_empty());
        assert!(
            matches!(inbox.next_item(), Some(Item::Human(s)) if s == "/clear"),
            "the slash line is delivered alone, never merged with what follows",
        );
        assert!(
            matches!(inbox.next_item(), Some(Item::Human(s)) if s == "after clear"),
            "the plain steering behind it is its own item",
        );
        assert!(inbox.is_empty());
    }

    /// A wakeup reaches the model as soon as the tool batch settles.
    #[test]
    fn inbox_wakeup_drains_at_tool_boundary_marked() {
        let inbox = Inbox::new();
        inbox.push_user("steer".into());
        inbox
            .push(wakeup(1, "nightly", "0 3 * * *", "run the tests"));

        assert!(
            matches!(
                inbox.drain_steering().as_slice(),
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
                inbox.drain_steering().as_slice(),
                [Item::Wakeup(_), Item::Human(s)] if s == "redirect now\nand also this",
            ),
            "the async wakeup and the coalesced steering both drain, in order",
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_agent_message_drains_marked_at_tool_boundary() {
        let inbox = Inbox::new();
        inbox
            .push(Post::AgentMessage(AgentMessage {
                from: 7,
                from_name: "review".into(),
                text: "please inspect the parser branch".into(),
            }));

        assert!(matches!(
            inbox.drain_steering().as_slice(),
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
        inbox.push(Post::Barrier("/rewind 7".into()));
        inbox.push_user("after the rewind".into());

        assert!(matches!(
            inbox.drain_steering().as_slice(),
            [Item::Human(b), Item::Wakeup(_)] if b == "before"
        ));
        assert!(inbox.drain_steering().is_empty());
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/rewind 7"));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "after the rewind"));
        assert!(inbox.is_empty());
    }

    /// A session-*reading* command waits for the exchange boundary itself, but
    /// does not make the prompts queued behind it wait too: they reach the
    /// model at the next tool boundary, and the command keeps its place.
    #[test]
    fn inbox_tool_drain_passes_over_a_session_reading_command() {
        let inbox = Inbox::new();
        inbox.push(Post::Command("/branch scout".into()));
        inbox.push_user("look at the parser too".into());

        assert!(
            matches!(
                inbox.drain_steering().as_slice(),
                [Item::Human(s)] if s == "look at the parser too",
            ),
            "the prompt behind a /branch reaches the model mid-exchange",
        );
        assert!(matches!(
            inbox.next_item(),
            Some(Item::Command(s)) if s == "/branch scout"
        ));
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
        inbox.push(Post::Command("/branch scout".into()));
        inbox.push_user("second".into());

        assert_eq!(
            inbox.queued_human_messages(),
            vec![
                "first".to_string(),
                "/branch scout".to_string(),
                "second".to_string()
            ]
        );
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "first"));
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/branch scout"));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "second"));
    }

    /// A sole wakeup is not the user's draft, so nothing comes back.
    #[test]
    fn inbox_pop_back_user_all_no_user_prompts() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "x", "@", "p"));
        assert_eq!(inbox.pop_back_user_all(), None, "no user prompts to recall");
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
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
        inbox.push(Post::Command("/model".into()));
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
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/model"));
        assert!(inbox.is_empty());
    }

    /// A `spawn` worker's batch, terminated by the `` `done `` event core appends.
    fn surface() -> Post {
        use ral_core::Value;
        let done = Value::Variant {
            label: "done".into(),
            payload: Some(Box::new(Value::map(vec![
                ("cmd".into(), Value::String("<block>".into())),
                (
                    "outcome".into(),
                    Value::Variant {
                        label: "ok".into(),
                        payload: Some(Box::new(Value::Unit)),
                    },
                ),
            ]))),
        };
        Post::Surface {
            id: 0,
            values: vec![done],
            generation: 0,
        }
    }

    /// A `/clear` landing between delivery and drain empties the deque.
    #[test]
    fn inbox_surface_drains_at_tool_boundary_and_cleared() {
        let inbox = Inbox::new();
        assert_eq!(surface().boundary(), Boundary::Tool);

        inbox.push(surface());
        inbox.clear();
        assert!(
            inbox.drain_steering().is_empty(),
            "a /clear drops the queued batch"
        );

        inbox.push(surface());
        assert!(matches!(
            inbox.drain_steering().as_slice(),
            [Item::Surface { id, .. }] if *id == 0
        ));
    }

    /// Draining a wakeup re-opens its schedule for the next occurrence.
    #[test]
    fn inbox_wakeup_clears_its_pending_flag_on_drain() {
        let pending = Arc::new(AtomicBool::new(true));
        let inbox = Inbox::new();
        inbox
            .push(Post::ScheduledWakeup {
                id: 1,
                label: "n".into(),
                trigger: "* * * * *".into(),
                prompt: "go".into(),
                pending: pending.clone(),
                epoch: 0,
            });
        assert!(pending.load(std::sync::atomic::Ordering::Acquire));
        let _ = inbox.next_item();
        assert!(
            !pending.load(std::sync::atomic::Ordering::Acquire),
            "draining the wakeup re-opens its schedule"
        );
    }

    /// `clear` runs the same drain side effect a real drain would, so an
    /// unconsumed wakeup's `pending` flag is not stranded `true` forever.
    /// `Avatar::clear` disarms the schedule registry alongside, but the TUI's
    /// `App::clear` reaches only the inbox, so this must hold on its own.
    #[test]
    fn inbox_clear_runs_the_drain_side_effect_on_a_stranded_wakeup() {
        let pending = Arc::new(AtomicBool::new(true));
        let inbox = Inbox::new();
        inbox
            .push(Post::ScheduledWakeup {
                id: 1,
                label: "n".into(),
                trigger: "* * * * *".into(),
                prompt: "go".into(),
                pending: pending.clone(),
                epoch: 0,
            });
        inbox.clear();
        assert!(
            !pending.load(std::sync::atomic::Ordering::Acquire),
            "clear must not strand a wakeup's pending flag at true"
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
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
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
        inbox
            .push(wakeup(1, "nightly", "@daily", "second"));
        inbox
            .push(wakeup(2, "morning", "@daily", "other schedule"));
        assert_eq!(
            depth_of(&inbox, Source::Schedule),
            2,
            "schedule 1 replaced in place; schedule 2 is its own entry"
        );
        match inbox.next_item() {
            Some(Item::Wakeup(text)) => assert!(
                text.contains("second") && !text.contains("first"),
                "the newest wakeup for schedule 1 wins: {text}"
            ),
            _ => panic!("expected schedule 1's (replaced) wakeup first"),
        }
        match inbox.next_item() {
            Some(Item::Wakeup(text)) => assert!(text.contains("other schedule")),
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
            Some(Item::Human(text)) => {
                assert_eq!(
                    text, "first line\nsecond line",
                    "both texts survive in order"
                );
            }
            _ => panic!("expected a merged Human item"),
        }
    }

    /// In either direction: merging would silently change the slash line's
    /// boundary ([`Post::boundary`]).
    #[test]
    fn inbox_user_steering_never_merges_across_a_slash_command() {
        let inbox = Inbox::new();
        inbox.push_user("plain text".into());
        inbox.push_user("/clear".into());
        assert_eq!(
            depth_of(&inbox, Source::User),
            2,
            "a slash line is never folded into a preceding plain-text entry"
        );
        inbox.push_user("after clear".into());
        assert_eq!(
            depth_of(&inbox, Source::User),
            3,
            "a plain line is never folded into a preceding slash entry either"
        );
    }

    /// The agent self-pushes a nudge per deliberation, so a second one means a
    /// fresher continuation superseded the first, not that both are owed.
    #[test]
    fn inbox_nudge_replaces_a_still_queued_one_newest_wins() {
        let inbox = Inbox::new();
        inbox
            .push(Post::Nudge {
                exchange: 1,
                text: "retry".into(),
            });
        inbox
            .push(Post::Nudge {
                exchange: 1,
                text: "retry".into(),
            });
        inbox
            .push(Post::Nudge {
                exchange: 2,
                text: "different".into(),
            });
        assert_eq!(
            depth_of(&inbox, Source::Nudge),
            1,
            "a nudge never grows past one outstanding entry"
        );
        assert!(
            matches!(inbox.next_item(), Some(Item::Nudge { text, .. }) if text == "different"),
            "the newest nudge is the one delivered"
        );
    }

}
