//! The inbound queue: [`Inbox`] is the owned consumer end and [`Mailbox`]
//! the cloneable sender end of a session's typed, multi-producer queue. A
//! push either coalesces into a still-queued entry or is checked against a
//! per-source quota ([`Shared::try_push`]); the two drains are a
//! tool-boundary pull mid-exchange ([`Inbox::drain_steering`]) and the
//! exchange-boundary pull the attend loop parks on ([`Inbox::next_or_idle`]).
//!
//! # Lock order: inbox before registries
//!
//! The attend loop evaluates its park verdict *while holding its inbox queue
//! mutex* — [`Inbox::next_or_idle`] recomputes it under the lock on every
//! wake — and the verdict reads the fleet's [`crate::fleet::registry::AgentRegistry`] and the
//! session's [`crate::fleet::schedule::ScheduleRegistry`].  The process-wide lock order is therefore
//! **inbox → registry**, and the converse is forbidden: never post to or
//! wake a [`Mailbox`] while holding a registry lock.  Clone the mailbox out,
//! drop the guard, then push — [`crate::fleet::registry::AgentRegistry::message`] and
//! [`crate::fleet::schedule::ScheduleRegistry::fire`] are the pattern.
//!
//! The two locks also shape how a producer must *sequence* its effects.
//! Each [`Inbox::next_or_idle`] iteration computes the verdict first and pops the
//! queue second, so a producer whose settling both changes a verdict input
//! and delivers a message — a child retiring its registry entry and posting
//! its result — must deliver first (deliver-then-retire,
//! [`crate::shell_eval::tools::agent::spawn_async`]): whichever side of the retirement the
//! verdict reads, the consumer either still parks for the child or finds
//! the result already queued, and can never quiesce between the two facts.

use super::post::{Boundary, is_slash, source_name};
use super::{Item, Post};
use crate::agent::cancel;
use crate::sync::LockExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How [`Inbox::next_or_idle`] should treat an empty inbox — the computed
/// [`Agent::park_mode`](crate::agent::Agent::park_mode) verdict.
///
/// Re-evaluated on every wake: a human exchange engages a parked agent, a
/// live child settles, or a schedule can arm or disarm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkMode {
    /// A human is having a live conversation with this agent — the
    /// interactive trunk, or a `/branch` tab, neither of which ever returns
    /// to a parent.  Park *and ignore cancellation* entirely: an Esc cancels
    /// the current *exchange*, not the agent, which keeps waiting for the next
    /// human line.
    Held,
    /// This agent does not converse — it returns to a parent, or runs
    /// headless — but a human has exchanged a message with it: the registry
    /// carries the exchange, not the TUI's focus cursor.  Parks on an empty
    /// queue exactly like [`Self::Held`], bounded by the registry's idle
    /// lease rather than any cancellation immunity — a *terminate*-cause
    /// cancellation (`agent-cancel`, the lease's own expiry) still ends it
    /// at once: an exchange is not immunity, or a `HeldByChildren` parent
    /// waiting on this very agent would never receive the cancelled result
    /// it is owed.
    Engaged,
    /// No human, but this agent has live children still running (async
    /// `agent`s it launched).  Each will deliver its result up this agent's
    /// own inbox when it settles, so park — a headless root waiting on its
    /// fleet has a legal "keep still" move rather than being killed at
    /// quiescence.  Like [`Self::UntilCancelled`], a cancellation
    /// (`agent-cancel`, the ceiling) terminates at once, and the wait ends on
    /// its own the moment the last child settles (the next re-evaluation sees
    /// no children and falls through to [`Self::Quiesce`]).
    HeldByChildren,
    /// No human, but a self-schedule is armed and may fire a wakeup.  Park,
    /// but a cancellation (`agent-cancel`, the ceiling) terminates at once —
    /// stop now rather than wait for the schedule.
    UntilCancelled,
    /// Nothing will ever feed this agent again: terminate at quiescence.
    Quiesce,
}

/// Per-agent, per-source cap on a *non-idempotent* inbox message
/// (`AgentResult`, `AgentMessage`, `Command`, `Surface`).
///
/// Generous, so an ordinary burst never rejects, but a runaway producer
/// cannot grow one source without bound. The idempotent sources (`user`,
/// `schedule`, `nudge`) never count toward this cap at all — they coalesce
/// instead — so this and [`INBOX_TOTAL_CAP`] bound only the
/// four quota-checked sources, not the inbox as a whole. `nudge` is itself
/// bounded to one outstanding entry (see [`Shared::try_push`]); `schedule`
/// is bounded only by how many schedules are concurrently armed — a count
/// this module does not hold and [`crate::fleet::schedule::ScheduleRegistry`] does
/// not cap either; `user` is bounded only by how many non-slash runs a human
/// queues between slash lines. Both are human/config scale in practice, not
/// machine-floodable the way the quota-checked sources are.
pub(crate) const INBOX_SOURCE_CAP: usize = 64;
/// Total cap across the four quota-checked sources.
///
/// Alongside [`INBOX_SOURCE_CAP`]: several sources sitting near their own cap
/// at once must not add up past one shared ceiling. Like [`INBOX_SOURCE_CAP`],
/// this does not bound the idempotent sources — see its doc for what does.
pub(crate) const INBOX_TOTAL_CAP: usize = 256;

/// Why a non-idempotent [`crate::bus::Post`] push was rejected.
///
/// See [`INBOX_SOURCE_CAP`] for which sources this can (and cannot) happen
/// to. Every producer surfaces this to its own caller as a user-facing
/// error; a push is never silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxReject {
    /// This source alone is already at [`INBOX_SOURCE_CAP`].
    SourceFull { source: &'static str, cap: usize },
    /// The whole inbox is already at [`INBOX_TOTAL_CAP`].
    TotalFull { cap: usize },
}

impl std::fmt::Display for InboxReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceFull { source, cap } => write!(
                f,
                "inbox[{source}] is full ({cap} queued) — drain before sending more"
            ),
            Self::TotalFull { cap } => write!(
                f,
                "inbox is full ({cap} messages queued) — drain before sending more"
            ),
        }
    }
}

/// The queue an [`Inbox`] consumer and its [`Mailbox`] senders share: a
/// [`VecDeque`] of [`crate::bus::Post`] under a `Mutex`, plus a [`Condvar`] a parked
/// `next_or_idle` waits on so a push wakes it without polling.
struct Shared {
    queue: Mutex<VecDeque<Post>>,
    signal: Condvar,
    /// True while the consumer is parked in [`ParkMode::Held`] or
    /// [`ParkMode::Engaged`] on an empty queue: the human-facing yield point.
    /// A producer clears it before waking the consumer, so frontends can
    /// distinguish "prompt is editable" from "the root is still working"
    /// without minting a presentation event.
    waiting_for_input: AtomicBool,
    /// This inbox's clear-epoch, bumped by [`Inbox::clear`] under the same
    /// queue mutex as the drain it runs alongside. A [`ScheduledWakeup`]
    /// composed on the reaper thread ([`crate::fleet::schedule::ScheduleRegistry::fire`], `schedule.rs`)
    /// cannot check staleness itself — its compose and its push are two
    /// separate steps that a `/clear` can fall between — so it stamps
    /// [`Mailbox::epoch`]'s value at compose time instead, and the pop-time
    /// admission check ([`pop_item`]) reads this counter fresh, under the same
    /// lock `clear` uses, to tell the two orderings apart: a push that lands
    /// before `clear`'s bump is swept with the rest of the queue, one that
    /// lands after is stamped stale and refused at the pop.
    ///
    /// [`ScheduledWakeup`]: crate::bus::Post::ScheduledWakeup
    epoch: AtomicU64,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            waiting_for_input: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
        })
    }

    /// Lock the queue, recovering from a poisoned mutex rather than
    /// panicking.  No operation ever run under this lock can leave the
    /// `VecDeque` itself torn — each is a total push/pop/clear — so a
    /// panicked holder leaves the queue in a perfectly usable state; the
    /// alternative (propagating the poison to every subsequent producer)
    /// would turn one unrelated panic into a permanently dead inbox for the
    /// whole fleet.  The same policy as
    /// [`crate::fleet::schedule::ScheduleRegistry::lock`]
    /// (`schedule.rs`).
    fn lock(&self) -> MutexGuard<'_, VecDeque<Post>> {
        self.queue.lock_ignore_poison()
    }

    /// Apply the inbox's push rule for one message, waking a parked consumer
    /// on success. The three idempotent sources always succeed, coalescing
    /// into an existing entry rather than growing the queue:
    ///
    /// - `ScheduledWakeup` replaces a still-queued wakeup for the *same
    ///   schedule id* (newest wins) rather than adding a second.
    /// - `UserSteering` joins onto a still-queued, non-slash tail entry with
    ///   a blank line, preserving arrival order; a slash line is never
    ///   merged either direction, so its exchange-boundary classification
    ///   ([`crate::bus::Post::boundary`]) always survives intact.
    /// - `Nudge` replaces a still-queued nudge outright (newest wins,
    ///   mirroring `ScheduledWakeup`) rather than adding a second: it is
    ///   always self-pushed by the agent's own attend loop reacting to one
    ///   deliberation's outcome, so at most one is ever meaningfully outstanding —
    ///   a second one queuing means a fresher continuation superseded the
    ///   first, not that both are owed to the model.
    ///
    /// Every other source (`AgentResult`, `AgentMessage`, `Command`,
    /// `Surface`) is quota-checked: rejected, never silently dropped, once
    /// its own [`INBOX_SOURCE_CAP`] or the shared [`INBOX_TOTAL_CAP`] is
    /// reached.
    fn try_push(&self, msg: Post) -> Result<(), InboxReject> {
        let mut q = self.lock();
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
                        s.push_str("\n\n");
                        s.push_str(&text);
                    }
                } else {
                    q.push_back(Post::UserSteering(text));
                }
            }
            Post::Nudge(text) => {
                let existing = q.iter().position(|m| matches!(m, Post::Nudge(_)));
                match existing {
                    Some(pos) => q[pos] = Post::Nudge(text),
                    None => q.push_back(Post::Nudge(text)),
                }
            }
            other => {
                let source = source_name(&other);
                let source_count = q.iter().filter(|m| source_name(m) == source).count();
                if source_count >= INBOX_SOURCE_CAP {
                    return Err(InboxReject::SourceFull {
                        source,
                        cap: INBOX_SOURCE_CAP,
                    });
                }
                if q.len() >= INBOX_TOTAL_CAP {
                    return Err(InboxReject::TotalFull {
                        cap: INBOX_TOTAL_CAP,
                    });
                }
                q.push_back(other);
            }
        }
        drop(q);
        self.waiting_for_input.store(false, Ordering::Release);
        self.signal.notify_all();
        Ok(())
    }
}

/// How long a parked [`Inbox::next_or_idle`] sleeps between condvar wakes
/// before re-checking its cancellation token.  A push notifies the condvar
/// immediately; this bound only governs how fast a cancel (which does not
/// notify) is observed by a parked agent.
const PARK_POLL: Duration = Duration::from_millis(100);

/// The cloneable **sender** side of a session's inbox.
///
/// Producers hold a
/// `Mailbox`, never the [`Inbox`]: a schedule re-arms through its own
/// session's `Mailbox`, a finishing child posts its one result through its
/// parent's `Mailbox` ([`Agent::mailbox`](crate::agent::Agent::mailbox)), a
/// `spawn` worker flushes its surface batch through the owning session's
/// `Mailbox`.  The registry holds each peer's `Mailbox` so the frontend can
/// steer a focused tab and the `message` tool can deliver a marked note
/// between live agents without exposing raw senders to model code.
#[derive(Clone)]
pub struct Mailbox {
    shared: Arc<Shared>,
}

impl Mailbox {
    /// Post any message (cron wakeup, agent result, self-nudge, …), applying
    /// the inbox's coalesce/quota rule ([`Shared::try_push`]) and waking a
    /// parked consumer on success.
    ///
    /// Takes the inbox queue mutex — callers must not hold a registry lock
    /// (the module's [lock order](self)): clone the mailbox out and push
    /// after the guard drops.
    ///
    /// # Errors
    /// Returns `Err(InboxReject)` when a non-idempotent source is already at
    /// its queue quota — see [`InboxReject`] for the rejection contract.
    pub(crate) fn push(&self, msg: Post) -> Result<(), InboxReject> {
        self.shared.try_push(msg)
    }

    /// Post a user-typed steering prompt — the TUI `Enter`-while-busy path.
    ///
    /// # Panics
    /// Panics if the push is rejected — ruled out because `UserSteering` is
    /// idempotent (it merges rather than growing the queue) and never
    /// rejects.
    pub(crate) fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt))
            .expect("UserSteering is idempotent and never rejects");
    }

    /// Whether this queue's consumer is parked at a human-input boundary. The
    /// TUI reads the focused tab's bit through its registry mailbox to drive
    /// the prompt chrome and tab-title spinner.
    pub(crate) fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// This inbox's current clear-epoch — the full compose-vs-`/clear` race
    /// a producer stamps this against is [`Shared::epoch`]'s own doc.
    pub(crate) fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }
}

/// A session's inbox: the owned **consumer** of the typed, multi-producer
/// queue the agent's attend loop pulls its next item from.  Senders are minted
/// with [`Self::mailbox`].
///
/// The attend loop drains tool-boundary messages mid-exchange ([`Self::drain_steering`],
/// from `deliberate`) and exchange-boundary items at the boundary
/// ([`Self::next_or_idle`]); a drained message disappears from the pending
/// strip and cannot be delivered twice.
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

    /// Mint a [`Mailbox`] sender onto this inbox's queue.
    pub(crate) fn mailbox(&self) -> Mailbox {
        Mailbox {
            shared: self.shared.clone(),
        }
    }

    /// Post directly through the consumer handle — the self-push path (a
    /// nudge, a self-armed wakeup landing in the agent's own box).  Equivalent
    /// to `self.mailbox().push(msg)`; see [`Mailbox::push`] for the
    /// coalesce/quota rule and the rejection contract.
    ///
    /// # Errors
    /// Returns `Err(InboxReject)` when a non-idempotent source is already at
    /// its queue quota.
    pub(crate) fn push(&self, msg: Post) -> Result<(), InboxReject> {
        self.shared.try_push(msg)
    }

    /// Post a user-typed steering prompt through the consumer handle.
    ///
    /// # Panics
    /// Panics if the push is rejected — ruled out by the same idempotence;
    /// see [`Mailbox::push_user`].
    pub(crate) fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt))
            .expect("UserSteering is idempotent and never rejects");
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.shared.lock().is_empty()
    }

    /// Whether the consumer is parked at a human-input boundary — true once it
    /// yields on an empty queue, cleared the moment a producer enqueues work.
    /// The chrome reads the focused tab's bit through its [`Mailbox`], not this
    /// consumer handle.
    pub(crate) fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// Queue depth per message source — the inbox's probe figures for the
    /// `/resources` fold, one `(source, count)` pair per [`crate::bus::Post`]
    /// variant, zeros included so the row set is stable.  Counts only,
    /// taken in one pass under the queue lock: nothing is drained,
    /// reordered, or woken — enumeration is not observation.
    pub(crate) fn source_depths(&self) -> Vec<(&'static str, u64)> {
        let mut rows = vec![
            ("user", 0u64),
            ("schedule", 0),
            ("agent", 0),
            ("message", 0),
            ("nudge", 0),
            ("command", 0),
            ("surface", 0),
        ];
        for msg in self.shared.lock().iter() {
            if let Some(row) = rows.iter_mut().find(|(s, _)| *s == source_name(msg)) {
                row.1 += 1;
            }
        }
        rows
    }

    /// Pending user-authored steering prompts, oldest first, for the TUI's
    /// queue strip.  Non-human deliveries and slash-command control items stay
    /// invisible here: they are work for the attend loop, not queued user text.
    pub(crate) fn queued_user_messages(&self) -> Vec<String> {
        self.shared
            .lock()
            .iter()
            .filter_map(|msg| match msg {
                Post::UserSteering(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pull every pending user prompt back out for editing at once — all the
    /// `UserSteering` messages in the queue, wherever they sit, leaving any
    /// non-user deliveries (a wakeup, an agent result, a `spawn`'s surface) in
    /// place for the exchange boundary.  A user prompt queued behind a wakeup is
    /// still the user's draft and should come back with the rest; the wakeup is
    /// not the user's draft and stays queued.
    ///
    /// Returns oldest-first, or `None` if no user prompts are queued.
    pub(crate) fn pop_back_user_all(&self) -> Option<Vec<String>> {
        let mut q = self.shared.lock();
        let mut prompts: Vec<String> = Vec::new();
        let mut kept: VecDeque<Post> = VecDeque::with_capacity(q.len());
        while let Some(msg) = q.pop_front() {
            match msg {
                Post::UserSteering(s) => prompts.push(s),
                other => kept.push_back(other),
            }
        }
        *q = kept;
        drop(q);
        (!prompts.is_empty()).then_some(prompts)
    }

    /// Mid-exchange drain at a tool-call boundary: take the leading run of
    /// tool-boundary messages — every source but a slash command — and deliver
    /// them, in order, each tagged with its source so the attend loop renders it in
    /// its honest medium (a `↘` subagent block for an agent result, a marked
    /// wakeup, the cards of a settled `spawn`).  A consecutive run of user
    /// steering coalesces into one [`crate::bus::Item::Human`].
    ///
    /// The scan stops at the first exchange-boundary entry
    /// ([`Boundary::Exchange`]) — a [`Post::Command`], which must run against a
    /// `ReadyForUser` session, or a slash-prefixed steering line holding its
    /// place — so it and everything queued behind it stay for
    /// [`Self::next_or_idle`].  This is also what holds the human's
    /// own ordering — a `/model` then a prompt swaps before the prompt runs —
    /// since steering queued behind the command is left with it.
    ///
    /// # Panics
    /// Panics only on an internal invariant violation (a bug): every
    /// `pop_front` here follows a `front` check made in the same loop
    /// iteration, under the same lock.
    pub(crate) fn drain_steering(&self) -> Vec<Item> {
        let mut q = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        let mut items = Vec::new();
        while q.front().is_some_and(|m| m.boundary() == Boundary::Tool) {
            if matches!(q.front(), Some(Post::UserSteering(_))) {
                items.push(coalesce_steering(&mut q));
            } else {
                let msg = q.pop_front().expect("front present and tool-boundary");
                if let Some(item) = to_item(msg, epoch) {
                    items.push(item);
                }
            }
        }
        drop(q);
        items
    }

    /// Exchange-boundary drain: the next deliverable, tagged with its source, or
    /// `None` if the queue is empty.  Never blocks — see [`Self::next_or_idle`]
    /// for the parking variant the attend loop uses.
    pub(crate) fn next_item(&self) -> Option<Item> {
        let mut q = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        pop_item(&mut q, epoch)
    }

    /// The attend loop's exchange-boundary pull.  Returns the next deliverable; on
    /// an empty queue the `park` verdict — recomputed on every wake — decides
    /// whether to park or terminate; the immunity ladder is [`ParkMode`]'s
    /// own doc.
    ///
    /// `park` is re-evaluated each iteration, so a lease expiry's
    /// terminate-cause cancel, or the last live child settling, is seen on
    /// the very next wake.  A push wakes the park at once through the
    /// condvar; a cancellation does not notify, so a non-`Held` park
    /// re-checks `cancel` every [`PARK_POLL`].
    ///
    /// Two orderings carry the loop's correctness.  The verdict runs *under
    /// the queue mutex*, so a push can never interleave between the verdict
    /// and the wait (the condvar releases the lock atomically) — a lost
    /// wakeup is impossible.  And the verdict is
    /// computed *before* the pop, so a producer that both changes a verdict
    /// input and delivers a message need only deliver first
    /// (deliver-then-retire, the module's [lock order](self)): a `Quiesce`
    /// verdict can then never win a race against a delivery it was supposed
    /// to wait for.
    pub(crate) fn next_or_idle(
        &self,
        park: impl Fn() -> ParkMode,
        cancel: &cancel::Token,
    ) -> Option<Item> {
        let mut q = self.shared.lock();
        loop {
            let mode = park();
            // Every park but a genuinely conversing `Held` terminates the
            // instant a *terminate*-cause cancel trips — `agent-cancel`, the
            // ceiling, or `/clear` means stop now, dropping any queued
            // messages; `Engaged` gets no immunity from the exchange alone.
            // An *interrupt*-cause cancel is not a terminate: it drops the
            // in-flight exchange but the agent re-parks.  Only a live human
            // conversation (`Held`) ignores cancellation entirely: an Esc
            // interrupts an exchange, not the agent.
            if mode != ParkMode::Held && cancel.terminated() {
                return None;
            }
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            if let Some(item) = pop_item(&mut q, epoch) {
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

    /// Drop every pending message — `/clear` rebuilds the agent, so neither
    /// queued user prompts nor stale non-human deliveries carry across.
    /// Runs each dropped message's drain side effect
    /// ([`crate::bus::Post::on_drain`]) first, so a queued-but-unconsumed
    /// `ScheduledWakeup`'s `pending` flag clears here too — `clear` does not
    /// depend on its callers also clearing the schedule registry to avoid
    /// stranding a schedule that can never fire again. Bumps the clear-epoch
    /// under the same queue lock as the drain — [`Shared::epoch`]'s doc has
    /// the full race against a racing schedule fire.
    pub(crate) fn clear(&self) {
        let mut q = self.shared.lock();
        for msg in q.drain(..) {
            msg.on_drain();
        }
        self.shared.epoch.fetch_add(1, Ordering::Release);
        drop(q);
    }
}

/// Pop the next exchange-boundary item from a locked queue, tagged with its
/// source.  A leading run of *non-slash* user steering coalesces into one
/// [`crate::bus::Item::Human`], matching the push-time never-merge rule
/// ([`Shared::try_push`]): a slash line neither absorbs the run ahead of it
/// nor merges with the steering behind it, so it is always delivered alone,
/// as ordinary prompt text — the same rule [`Inbox::drain_steering`] already
/// applies at the tool boundary.  Every other source is delivered on its own
/// so the attend loop can render each in its honest medium.
///
/// `epoch` is this inbox's *current* clear-epoch, read by the caller under
/// the same lock this function runs under — see [`Shared::epoch`] for the
/// full staleness race.  A stale `ScheduledWakeup` is dropped rather than
/// converted, so the loop below just tries the next queued message.
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

/// Pop the leading run of consecutive, non-slash [`crate::bus::Post::UserSteering`]
/// entries off `q` and join them with a blank line into one [`crate::bus::Item::Human`] —
/// the coalesce half of the never-merge rule ([`Shared::try_push`]), shared by
/// [`Inbox::drain_steering`] and [`pop_item`]. Both callers enter with a
/// guaranteed non-slash steering at the front, so the loop always pops at
/// least one entry.
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
            text.push_str("\n\n");
        }
        text.push_str(&s);
    }
    Item::Human(text)
}

/// Convert one non-user-steering message into the [`crate::bus::Item`] it delivers,
/// running its drain side effect ([`crate::bus::Post::on_drain`]) — or `None` for a
/// [`ScheduledWakeup`](crate::bus::Post::ScheduledWakeup) whose stamped epoch has
/// fallen behind `epoch`, refused rather than converted.  Shared by the
/// tool-boundary drain ([`Inbox::drain_steering`]) and the exchange-boundary drain
/// ([`pop_item`]).
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
        Post::Nudge(s) => Item::Nudge(s),
        Post::Command(s) => Item::Command(s),
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
    use super::{Boundary, INBOX_SOURCE_CAP, Inbox, InboxReject, Item, ParkMode, Post};
    use crate::agent::cancel;
    use crate::bus::AgentMessage;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A scheduled-wakeup message with a fresh pending flag, stamped with
    /// epoch 0 (a fresh [`Inbox`]'s own starting epoch), for the inbox drain
    /// tests. `id` matters only to the dedupe tests below; the drain-order
    /// tests all use the same arbitrary id.
    fn wakeup(id: u64, label: &str, trigger: &str, prompt: &str) -> Post {
        wakeup_at(id, label, trigger, prompt, 0)
    }

    /// [`wakeup`], stamped with an explicit epoch — for the admission tests
    /// below, which need a wakeup composed under a *stale* epoch.
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
            std::thread::spawn(move || worker_inbox.next_or_idle(|| ParkMode::Held, &worker_token));

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
                || {
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

    /// The complement of the test above: a non-`Held` park ignores an
    /// *interrupt*-cause cancel — an interrupt drops the in-flight exchange, it does
    /// not end the agent — where a *terminate* cause ends it.
    ///
    /// "Still parked" is proved without a timing race by making the release a
    /// real item: after `cancel(Interrupt)`, the only exit `next_or_idle` has
    /// left is a pushed item.  It cannot return `None` — `terminated()` never
    /// trips for an interrupt, and the park is not `Quiesce` — so it stays
    /// parked until the push wakes it, then pops the item and returns `Some`.  A
    /// terminate cancel would instead have returned `None`, dropping the item;
    /// observing the item come back through the join is therefore exactly the
    /// evidence the interrupt was ignored.  No sleep gates the assertion.
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
                || {
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

        // An interrupt is not a terminate: the park re-checks `cancel` each
        // PARK_POLL and stays, because `terminated()` stays false.
        token.cancel(ral_core::process::CancelCause::Interrupt);

        // The only remaining exit is a real item.  Getting it back proves the
        // interrupt did not end the park (which would have dropped it, `None`).
        inbox.mailbox().push_user("resume".into());
        assert!(
            matches!(
                handle.join().expect("parked worker joins"),
                Some(Item::Human(s)) if s == "resume"
            ),
            "the interrupt was ignored; the pushed item released the park"
        );
    }

    /// [`ParkMode::Engaged`] — a non-conversing agent a human has exchanged a
    /// message with — grants no cancellation immunity: `agent-cancel`/the
    /// lease expiring while the exchange is recent must still kill it, or
    /// its `HeldByChildren` parent would sit waiting on a cancelled-result
    /// delivery that never comes.  Contrast
    /// [`held_park_survives_a_terminate_cause`], where a *genuine*
    /// conversation stays immune.
    #[test]
    fn engaged_park_dies_on_a_terminate_cause_despite_the_exchange() {
        let inbox = Inbox::new();
        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            inbox.next_or_idle(
                || {
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

    /// The complement: a genuinely conversing [`ParkMode::Held`] stays immune
    /// even to a terminate cause — the split introduced for `Engaged` must not
    /// weaken `Held`'s existing immunity for a live human conversation.  Proved
    /// the same way [`non_human_park_survives_an_interrupt`] proves an
    /// interrupt is ignored: the only remaining exit is a pushed item, so
    /// getting it back shows the terminate cancel did not end the park.
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
                || {
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

    /// Tool-boundary drain stops at the first slash-prefixed steering line.
    /// It waits for the exchange boundary like a real [`crate::bus::Post::Command`], but
    /// is delivered as ordinary prompt text, never interpreted — and, per the
    /// never-merge rule, it neither absorbs the non-slash run ahead of it nor
    /// merges with the plain steering queued behind it.
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

    /// A scheduled wakeup drains at the tool boundary, marked, alongside the
    /// steering ahead of it — so it reaches the model as soon as the tool
    /// batch settles rather than waiting out the whole exchange.
    #[test]
    fn inbox_wakeup_drains_at_tool_boundary_marked() {
        let inbox = Inbox::new();
        inbox.push_user("steer".into());
        inbox
            .push(wakeup(1, "nightly", "0 3 * * *", "run the tests"))
            .unwrap();

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

    /// Asynchronous deliveries — a settled detached agent's `AgentResult`, a
    /// `spawn`'s `Surface`, a `ScheduledWakeup` — drain at the tool boundary
    /// too, in queue order, so a result that settles during a long tool-call
    /// loop reaches the model at the next boundary, not at exchange's end.
    #[test]
    fn inbox_tool_drain_takes_async_deliveries() {
        let inbox = Inbox::new();
        // A wakeup that fired, then a barging human, in arrival order.
        inbox.push(wakeup(1, "nightly", "@", "go")).unwrap();
        inbox.push_user("redirect now".into());
        inbox.push_user("and also this".into());

        assert!(
            matches!(
                inbox.drain_steering().as_slice(),
                [Item::Wakeup(_), Item::Human(s)] if s == "redirect now\n\nand also this",
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
            }))
            .unwrap();

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

    /// A slash command holds the line: it is the lone exchange-boundary item,
    /// so the drain stops at it and everything queued behind — here steering
    /// the human typed after a mid-exchange `/model` — stays for the exchange boundary,
    /// running after the swap.  Async deliveries ahead of it still drain.
    #[test]
    fn inbox_tool_drain_stops_at_command_barrier() {
        let inbox = Inbox::new();
        inbox.push_user("before".into());
        inbox.push(wakeup(1, "x", "@", "p")).unwrap();
        inbox.push(Post::Command("/model".into())).unwrap();
        inbox.push_user("after model".into());

        // "before" and the wakeup drain; the /model command stops the run, so
        // "after model" stays behind it for the exchange boundary.
        assert!(matches!(
            inbox.drain_steering().as_slice(),
            [Item::Human(b), Item::Wakeup(_)] if b == "before"
        ));
        assert!(inbox.drain_steering().is_empty());
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/model"));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "after model"));
        assert!(inbox.is_empty());
    }

    /// The TUI queue strip is a user-text projection, not a generic inbox
    /// debugger: wakeups and control items stay out, while user steering keeps
    /// its queue order even when interleaved with them.
    #[test]
    fn inbox_queued_user_messages_shows_only_user_steering() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "morning", "@daily", "check")).unwrap();
        inbox.push_user("first".into());
        inbox.push(Post::Command("/model".into())).unwrap();
        inbox.push_user("second".into());

        assert_eq!(
            inbox.queued_user_messages(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "first"));
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/model"));
        assert!(matches!(inbox.next_item(), Some(Item::Human(s)) if s == "second"));
    }

    /// A queue with no user prompts yields `None`: a sole wakeup is not the
    /// user's draft and stays for the exchange boundary.
    #[test]
    fn inbox_pop_back_user_all_no_user_prompts() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "x", "@", "p")).unwrap();
        assert_eq!(inbox.pop_back_user_all(), None, "no user prompts to recall");
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
    }

    /// `pop_back_user_all` extracts every user prompt entry from the queue —
    /// even ones sandwiched between non-user deliveries — and leaves the
    /// non-user messages in their original order for the exchange boundary.
    /// "second" and "third" arrive back-to-back with nothing between them,
    /// so the push-time merge rule already folded them into one entry; the
    /// wakeup and the command each still separate a run and force a fresh
    /// entry.
    #[test]
    fn inbox_pop_back_user_all_extracts_all_leaving_non_user_in_order() {
        let inbox = Inbox::new();
        inbox.push_user("first".into());
        inbox.push(wakeup(1, "x", "@", "p")).unwrap();
        inbox.push_user("second".into());
        inbox.push_user("third".into());
        inbox.push(Post::Command("/model".into())).unwrap();
        inbox.push_user("fourth".into());
        assert_eq!(
            inbox.pop_back_user_all(),
            Some(vec![
                "first".to_string(),
                "second\n\nthird".to_string(),
                "fourth".to_string(),
            ]),
            "all user prompts come back oldest-first, past interspersed deliveries",
        );
        // The non-user messages survive in their original order.
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
        assert!(matches!(inbox.next_item(), Some(Item::Command(s)) if s == "/model"));
        assert!(inbox.is_empty());
    }

    /// A deferred `spawn` worker's delivered surface batch, terminated by
    /// the `` `done `` event core appends.
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

    /// A `Surface` drains at the tool boundary as a [`crate::bus::Item::Surface`] in the
    /// root viewport, and `clear` drops a queued batch for free (the deque is
    /// emptied), so a `/clear` between delivery and drain delivers nothing.
    #[test]
    fn inbox_surface_drains_at_tool_boundary_and_cleared() {
        let inbox = Inbox::new();
        assert_eq!(surface().boundary(), Boundary::Tool);

        inbox.push(surface()).unwrap();
        inbox.clear();
        assert!(
            inbox.drain_steering().is_empty(),
            "a /clear drops the queued batch"
        );

        // A fresh, un-cleared batch surfaces mid-exchange.
        inbox.push(surface()).unwrap();
        assert!(matches!(
            inbox.drain_steering().as_slice(),
            [Item::Surface { id, .. }] if *id == 0
        ));
    }

    /// A wakeup's pending flag clears when it drains, re-opening its
    /// schedule for the next occurrence (the overlap-skip mechanism).
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
            })
            .unwrap();
        assert!(pending.load(std::sync::atomic::Ordering::Acquire));
        let _ = inbox.next_item();
        assert!(
            !pending.load(std::sync::atomic::Ordering::Acquire),
            "draining the wakeup re-opens its schedule"
        );
    }

    /// `clear` runs the same drain side effect a real drain would, so a
    /// queued-but-unconsumed wakeup's `pending` flag is cleared rather than
    /// stranded `true` forever.  Production never observes this today only
    /// because both call sites also clear the schedule registry outright
    /// (`agent.rs`'s `/clear`); `clear` must not depend on that coupling to
    /// be correct on its own.
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
            })
            .unwrap();
        inbox.clear();
        assert!(
            !pending.load(std::sync::atomic::Ordering::Acquire),
            "clear must not strand a wakeup's pending flag at true"
        );
        assert!(inbox.is_empty());
    }

    /// The schedule race ([`crate::fleet::schedule::ScheduleRegistry::fire`], `schedule.rs`): a wakeup
    /// composed while a stale epoch was still current, then pushed after an
    /// intervening `/clear` already bumped it, is refused at the pop rather
    /// than surfacing into the rebuilt context.
    #[test]
    fn stale_epoch_wakeup_is_refused_at_pop() {
        let inbox = Inbox::new();
        let stale = inbox.mailbox().epoch();
        inbox.clear();
        inbox.push(wakeup_at(1, "n", "@", "go", stale)).unwrap();
        assert!(
            inbox.next_item().is_none(),
            "a wakeup stamped with an epoch older than the inbox's own is dropped"
        );
    }

    /// The positive half: a wakeup stamped with the live epoch is delivered
    /// exactly like any other.
    #[test]
    fn current_epoch_wakeup_is_delivered() {
        let inbox = Inbox::new();
        let live = inbox.mailbox().epoch();
        inbox.push(wakeup_at(1, "n", "@", "go", live)).unwrap();
        assert!(matches!(inbox.next_item(), Some(Item::Wakeup(_))));
    }

    // ── inbox quotas without silent loss ───────────────────────────────────

    /// The source name a `source_depths` row carries, for these tests'
    /// convenience.
    fn depth_of(inbox: &Inbox, source: &str) -> u64 {
        inbox
            .source_depths()
            .into_iter()
            .find(|(s, _)| *s == source)
            .map_or(0, |(_, n)| n)
    }

    /// A newer wakeup for the same schedule id replaces a still-queued older
    /// one in place — one entry, not two — while a different schedule's
    /// wakeup is untouched and keeps its own arrival order.
    #[test]
    fn inbox_scheduled_wakeup_dedupes_by_schedule_id_newest_wins() {
        let inbox = Inbox::new();
        inbox.push(wakeup(1, "nightly", "@daily", "first")).unwrap();
        inbox
            .push(wakeup(1, "nightly", "@daily", "second"))
            .unwrap();
        inbox
            .push(wakeup(2, "morning", "@daily", "other schedule"))
            .unwrap();
        assert_eq!(
            depth_of(&inbox, "schedule"),
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

    /// Consecutive `UserSteering` pushes merge into one queue entry —
    /// newline-joined, order kept — rather than growing the queue one per
    /// keystroke of a fast typist.
    #[test]
    fn inbox_user_steering_merges_pre_boundary_preserving_order() {
        let inbox = Inbox::new();
        inbox.push_user("first line".into());
        inbox.push_user("second line".into());
        assert_eq!(
            depth_of(&inbox, "user"),
            1,
            "consecutive steering merges into one entry at push time"
        );
        match inbox.next_item() {
            Some(Item::Human(text)) => {
                assert_eq!(
                    text, "first line\n\nsecond line",
                    "both texts survive in order"
                );
            }
            _ => panic!("expected a merged Human item"),
        }
    }

    /// A slash line is never merged into an adjacent plain-text entry, in
    /// either direction — merging it away would silently change its
    /// exchange-boundary classification ([`crate::bus::Post::boundary`]).
    #[test]
    fn inbox_user_steering_never_merges_across_a_slash_command() {
        let inbox = Inbox::new();
        inbox.push_user("plain text".into());
        inbox.push_user("/clear".into());
        assert_eq!(
            depth_of(&inbox, "user"),
            2,
            "a slash line is never folded into a preceding plain-text entry"
        );
        inbox.push_user("after clear".into());
        assert_eq!(
            depth_of(&inbox, "user"),
            3,
            "a plain line is never folded into a preceding slash entry either"
        );
    }

    /// A newer `Nudge` replaces a still-queued one outright (newest wins,
    /// mirroring `ScheduledWakeup`'s own dedupe-by-id) rather than adding a
    /// second: it is always self-pushed by the agent reacting to one
    /// deliberation's outcome, so at most one is ever meaningfully outstanding — a second
    /// arriving means a fresher continuation superseded the first.
    #[test]
    fn inbox_nudge_replaces_a_still_queued_one_newest_wins() {
        let inbox = Inbox::new();
        inbox.push(Post::Nudge("retry".into())).unwrap();
        inbox.push(Post::Nudge("retry".into())).unwrap();
        inbox.push(Post::Nudge("different".into())).unwrap();
        assert_eq!(
            depth_of(&inbox, "nudge"),
            1,
            "a nudge never grows past one outstanding entry"
        );
        assert!(
            matches!(inbox.next_item(), Some(Item::Nudge(s)) if s == "different"),
            "the newest nudge is the one delivered"
        );
    }

    /// A non-idempotent source (`Command`, here) rejects once it reaches its
    /// own per-source cap — the producer observes the rejection directly as
    /// an `Err`, never a silent drop.
    #[test]
    fn inbox_non_idempotent_source_rejects_at_quota() {
        let inbox = Inbox::new();
        for _ in 0..INBOX_SOURCE_CAP {
            inbox
                .push(Post::Command("/noop".into()))
                .expect("under quota");
        }
        let err = inbox
            .push(Post::Command("/noop".into()))
            .expect_err("the cap-th push is rejected");
        assert_eq!(
            err,
            InboxReject::SourceFull {
                source: "command",
                cap: INBOX_SOURCE_CAP,
            }
        );
    }

    /// Draining one queued message frees exactly one slot of quota for its
    /// source.
    #[test]
    fn inbox_drain_frees_quota_for_a_rejected_source() {
        let inbox = Inbox::new();
        for _ in 0..INBOX_SOURCE_CAP {
            inbox.push(Post::Command("/noop".into())).unwrap();
        }
        assert!(inbox.push(Post::Command("/noop".into())).is_err());
        assert!(matches!(inbox.next_item(), Some(Item::Command(_))));
        inbox
            .push(Post::Command("/noop".into()))
            .expect("draining freed one slot of quota");
    }

    /// The idempotent sources (`user`, `schedule`, `nudge`) never reject,
    /// however far past `INBOX_SOURCE_CAP` they are pushed.  `nudge` also
    /// stays bounded to its one outstanding entry (the dedupe above);
    /// `schedule` and `user` are not bounded by this cap at all — a distinct
    /// schedule id or a slash-interleaved run of steering each mint their own
    /// entry — which is accepted, human/config-scale risk this module does
    /// not itself enforce (see [`INBOX_SOURCE_CAP`]'s doc).
    #[test]
    fn inbox_idempotent_sources_never_reject_past_the_source_cap() {
        let inbox = Inbox::new();
        for i in 0..(INBOX_SOURCE_CAP * 3) {
            inbox
                .push(Post::Nudge(format!("n{i}")))
                .expect("nudge never rejects");
            inbox
                .push(wakeup(i as u64, "s", "@", "p"))
                .expect("wakeup never rejects");
            inbox.push_user(format!("line {i}"));
        }
        assert_eq!(
            depth_of(&inbox, "nudge"),
            1,
            "nudge alone stays bounded, however many distinct texts are pushed"
        );
    }
}
