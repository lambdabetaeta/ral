//! Agent / frontend boundary.  Workers stamp [`Kind`]s with their
//! [`AgentId`] through an [`Emitter`]; consumers implement [`Sink`].
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
pub mod card;

use crate::agent::cancel;
use crate::agent::event::ProviderErrorRecord;
use crate::agent::transcript::Transcript;
use crate::bus::card::{Card, IoEvent};
use crate::fleet::schedule::ScheduleId;
use crate::provider::{Tuning, Usage};
use crate::sync::LockExt;
use ral_core::Value;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The identity of an agent node.
///
/// Every agent — the trunk and every forked
/// child alike — has one; a child's id *is* its `AgentId`, so the `agents`
/// listing and `agent-cancel` reuse the node identity rather than minting a
/// parallel one.  Opaque: a capability for status and cancellation, not a
/// content hash.
pub type AgentId = u64;

/// When a message in the inbox may be drained into the model's context.
///
/// The boundary is a *per-message* property, not a global rule.  Everything
/// drains at the next tool-call boundary — user steering, a finished agent's
/// result, a scheduled wakeup, a settled `spawn`'s surface batch, a self-nudge
/// — so it reaches the model as soon as the current tool batch settles rather
/// than waiting out the whole exchange.  The sole exception is
/// [`Post::Command`]: it is handed to the attend loop's `Control` (a
/// `/model` swap, a `/clear`), never shown to the model, so it waits for the
/// exchange boundary where the session is `ReadyForUser`.  A slash-prefixed
/// [`Post::UserSteering`] waits there too, purely to keep its place
/// relative to whatever else is queued around it — it is still delivered as
/// ordinary prompt text, never interpreted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    /// May drain mid-exchange, at a tool-call boundary — everything but a command.
    Tool,
    /// Drains only at the exchange boundary: a slash command, run by `Control`.
    Exchange,
}

/// How [`Inbox::next_or_idle`] should treat an empty inbox — the computed
/// [`Agent::park_mode`](crate::agent::Agent::park_mode) verdict.
///
/// Re-evaluated on every wake: a human exchange engages a parked agent, a
/// live child settles, or a schedule can arm or disarm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkMode {
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

/// How an async agent settled, already reduced to what the parent's
/// synthetic item needs to say.  The provider-message detail stays in the
/// child's own log; this is the digest delivered through the inbox.
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    /// Finished with a final answer (carried in [`AgentResult::text`]).
    Complete,
    /// Finished but produced no text.
    Empty,
    /// Stopped for a non-routine reason (content filter, step cap, …).
    Stopped(String),
    /// Cancelled — by `agent-cancel`, `/clear`, or the worker ceiling.
    Cancelled,
    /// The run errored (provider error, panic).
    Failed(String),
}

impl AgentOutcome {
    /// The `(body, error)` a `↘` subagent breadcrumb shows: body text on a
    /// completed run, the reason in the header suffix otherwise.  Used by both
    /// the synchronous child's [`Kind::SubagentDone`] and the async result's
    /// fresh item, so the two render as the identical dialable block.
    pub fn breadcrumb(&self, text: &str) -> (String, Option<String>) {
        match self {
            Self::Complete => (text.to_string(), None),
            Self::Empty => (String::new(), None),
            Self::Stopped(r) => (String::new(), Some(r.clone())),
            Self::Cancelled => (String::new(), Some("cancelled".into())),
            Self::Failed(e) => (String::new(), Some(e.clone())),
        }
    }

    /// The marked synthetic-item text the model sees when an async result is
    /// drained, named with the child's tab label.
    pub fn marked_item(&self, name: &str, text: &str) -> String {
        match self {
            Self::Complete => format!("[agent '{name}' finished]\n{text}"),
            Self::Empty => format!("[agent '{name}' finished with no output]"),
            Self::Stopped(r) => format!("[agent '{name}' stopped: {r}]"),
            Self::Cancelled => format!("[agent '{name}' was cancelled]"),
            Self::Failed(e) => format!("[agent '{name}' failed: {e}]"),
        }
    }
}

/// The settle record an async agent posts to its parent's inbox.
///
/// It is *not* raw `<agent=…>` text in a prompt queue: the source tag and
/// drain boundary are data, and the model boundary is the only place this
/// renders to prose ([`AgentResult::render`]).
#[derive(Clone, Debug)]
pub struct AgentResult {
    pub id: AgentId,
    pub name: String,
    pub outcome: AgentOutcome,
    pub text: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
    /// The session generation that owned the worker.  A result whose
    /// generation is older than the live session (a worker that settled
    /// after a `/clear`) is rejected at drain rather than delivered into a
    /// rebuilt context.
    pub generation: u64,
}

impl AgentResult {
    /// The marked synthetic-item text the model sees when this is drained.
    fn render(&self) -> String {
        self.outcome.marked_item(&self.name, &self.text)
    }
}

/// A live agent sent a peer message to another live agent.
#[derive(Clone, Debug)]
pub struct AgentMessage {
    pub from: AgentId,
    pub from_name: String,
    pub text: String,
}

impl AgentMessage {
    /// The marked synthetic-item text the recipient model sees when this
    /// message drains.
    fn render(&self) -> String {
        format!(
            "[EXARCH AGENT {} MESSAGE: {}]\n{}\n[/EXARCH]",
            self.from, self.from_name, self.text
        )
    }
}

/// One typed message waiting in a session's [`Inbox`].
///
/// This is the inbound twin of the outbound [`Kind`] event stream: where
/// the old prompt queue held bare user `String`s, the inbox holds every
/// producer's message, each carrying its *source* (the variant itself) and
/// its *drain boundary* ([`Post::boundary`]).  A cancellation is
/// deliberately **not** a variant: the control plane (cancel a scope) and
/// the data plane (deliver a message) ride separate rails, so a
/// cancellation is unconstructable here by type.
#[derive(Clone, Debug)]
pub enum Post {
    /// The user typed a prompt while an exchange was running.  Drains at the
    /// tool boundary, except a slash-prefixed line, which waits for the
    /// exchange boundary alongside [`Self::Command`] — but is still delivered as
    /// ordinary prompt text ([`Item::Human`]), never interpreted: only
    /// [`Self::Command`] reaches [`Control`](crate::agent::Control).
    UserSteering(String),
    /// A scheduled wakeup fired (cron / after).  Drains at the tool boundary
    /// as a *marked* injection, so it reaches the model as soon as the current
    /// tool batch settles.
    ScheduledWakeup {
        /// The firing schedule's id — the inbox's dedupe key: a newer
        /// wakeup for the same schedule replaces a still-queued older one
        /// rather than growing the queue. In practice the
        /// schedule's own overlap-skip (`pending`, below) already keeps at
        /// most one in flight per schedule; this is the inbox's own,
        /// independent guarantee of the same invariant.
        id: ScheduleId,
        /// The human label the schedule was given (or its id).
        label: String,
        /// The trigger as text — a cron expression or `after <dur>` — for
        /// the marked render and the events record.
        trigger: String,
        /// The natural-language instructions the model acts on.
        prompt: String,
        /// The schedule's "a wakeup is unconsumed" flag: set when this
        /// message is posted, cleared when it drains.  The next occurrence
        /// reads it for the overlap-skip rule (at most one pending wakeup
        /// per schedule), so a tick arriving while the previous wakeup still
        /// waits is dropped rather than stacked.
        pending: Arc<AtomicBool>,
        /// This inbox's own clear-epoch ([`Mailbox::epoch`]) at the moment
        /// the reaper thread composed this message ([`crate::fleet::schedule::ScheduleRegistry::fire`],
        /// `schedule.rs`) — read and stamped before the registry lock that
        /// guards the entry is dropped, so a fire that then straddles a
        /// `/clear` before it reaches `push` still carries the epoch as it
        /// stood at composition.  Checked against the live epoch at the pop
        /// boundary ([`pop_item`]), which shares its queue mutex with
        /// [`Inbox::clear`]'s own epoch bump: the two operations can never
        /// interleave, so a fire that lands after the bump is refused,
        /// while one that lands before is swept with the rest of the queue.
        epoch: u64,
    },
    /// An async agent settled.  Drains at the tool boundary as a dialable `↘`
    /// subagent block, so its result reaches the model as soon as the current
    /// tool batch settles.
    AgentResult(AgentResult),
    /// A live peer agent sent a message.  Drains at the tool boundary as a
    /// marked injection, never as human text.
    AgentMessage(AgentMessage),
    /// A synthetic continuation the agent posted to *itself* after an attempt
    /// the nudge registry decided to retry (an empty reply, an early stop, a
    /// budget-free completion gate).  Carries the synthetic user message; it
    /// is the same exchange continuing, so it resets no exchange latch and renders
    /// with no human chrome.  Self-pushed through the agent's own
    /// [`Mailbox`], never across agents.
    Nudge(String),
    /// A session-affecting slash command (`/clear`, `/model`, `/compact`,
    /// `/quit`) the frontend posted at the exchange boundary.  The attend loop —
    /// which owns the session the command mutates — hands it to its
    /// [`Control`](crate::agent::Control); view-only commands (`/help`,
    /// `/copy`, …) are handled frontend-side and never reach here.  Carries
    /// the raw command line.
    Command(String),
    /// A deferred `spawn` worker delivered its `surface` batch at
    /// settlement — the un-awaited delivery path.  The batch is ordinary
    /// surface vocabulary (io maps, `` `card `` variants) terminated by a
    /// `` `done `` event; the deferred sink posts it here, stamped with the
    /// *root* session id so its cards render in the root viewport (a spawn
    /// worker registers no tab of its own).  Drains at the tool boundary,
    /// like a wakeup or an agent result.  Already once-only when posted:
    /// core's completion path wins the worker's deliver-once latch before
    /// the sink ever sees the batch.
    Surface {
        id: AgentId,
        values: Vec<Value>,
        /// The fleet `AgentRegistry` generation the deferred sink captured at
        /// construction (`InboxDeferred`, `shell_eval.rs`) — the same birth
        /// generation an [`AgentResult`] carries. Checked at the same
        /// consuming edge ([`Agent::admits`](crate::agent::Agent::admits)) against the
        /// live generation, so a batch flushed after a `/clear` is dropped
        /// there rather than posted-or-withheld by the worker thread itself.
        generation: u64,
    },
}

impl Post {
    /// Where this message may be drained.  Only a slash command waits for the
    /// exchange boundary (the `Control` interprets it against the session); every
    /// other message — content or a self-nudge continuation — drains at the
    /// next tool boundary.
    pub fn boundary(&self) -> Boundary {
        match self {
            Self::Command(_) => Boundary::Exchange,
            Self::UserSteering(s) if is_slash(s) => Boundary::Exchange,
            _ => Boundary::Tool,
        }
    }

    /// The side effect of draining this message into context.  A scheduled
    /// wakeup clears its pending flag here, re-opening its schedule for the
    /// next occurrence — the overlap-skip holds only until the wakeup is
    /// taken.  Other messages have none.
    fn on_drain(&self) {
        if let Self::ScheduledWakeup { pending, .. } = self {
            pending.store(false, Ordering::Release);
        }
    }
}

/// Whether `s` is a slash command line — trimmed leading whitespace, then a
/// `/`. Shared by [`Post::boundary`] (a slash steering waits for the
/// exchange boundary) and the inbox's steering-merge rule ([`Shared::try_push`]):
/// a slash line is never folded into an adjacent plain-text run, so its
/// exchange-boundary classification always survives the merge intact.
fn is_slash(s: &str) -> bool {
    s.trim_start().starts_with('/')
}

/// The probe/quota source name for one message — the seven-way split
/// [`Inbox::source_depths`] and the quota check ([`Shared::try_push`]) both
/// key on.
fn source_name(msg: &Post) -> &'static str {
    match msg {
        Post::UserSteering(_) => "user",
        Post::ScheduledWakeup { .. } => "schedule",
        Post::AgentResult(_) => "agent",
        Post::AgentMessage(_) => "message",
        Post::Nudge(_) => "nudge",
        Post::Command(_) => "command",
        Post::Surface { .. } => "surface",
    }
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
pub const INBOX_SOURCE_CAP: usize = 64;
/// Total cap across the four quota-checked sources.
///
/// Alongside [`INBOX_SOURCE_CAP`]: several sources sitting near their own cap
/// at once must not add up past one shared ceiling. Like [`INBOX_SOURCE_CAP`],
/// this does not bound the idempotent sources — see its doc for what does.
pub const INBOX_TOTAL_CAP: usize = 256;

/// Why a non-idempotent [`Post`] push was rejected.
///
/// The idempotent
/// sources (`user`, `schedule`, `nudge`) never produce this — they coalesce
/// instead of counting toward a cap. Every producer surfaces this to its
/// own caller as a user-facing error; a push is never silently dropped.
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

/// The model-facing notice [`Item::Surface`] delivers when a detached `spawn`
/// worker settles un-awaited: which spawn finished, how it settled, and where
/// its output now lives.  This is the "host notifies, don't poll" wake — terse
/// and in the register the model already sees from a subagent breadcrumb.  The
/// worker's surfaced cards have already reached the rail through
/// `agent::attend::announce`'s decode; the value record (a return
/// value, captured bytes) is pulled on demand with `await $h`.
fn surface_notice(values: &[Value]) -> String {
    use crate::bus::card::DoneOutcome;
    let settled = match values
        .iter()
        .rev()
        .find_map(crate::bus::card::value_to_done)
    {
        Some(DoneOutcome::Ok) => "finished (exit 0)".to_string(),
        Some(DoneOutcome::Err { message, status }) => {
            format!("finished (exit {status}): {message}")
        }
        Some(DoneOutcome::Panic { message }) => format!("panicked: {message}"),
        None => "finished".to_string(),
    };
    format!("Background block {settled}. Await its handle for the value.")
}

/// The next deliverable an exchange-boundary drain yields, carrying both the
/// model-facing text *and* its source.
///
/// Each deliverable carries the source it came from, so the attend loop renders
/// it in its honest medium — a human prompt echoes as the user's turn, a
/// wakeup as marked chrome, an agent reply as the same `↘` block a
/// synchronous child gets — while the model still receives [`Self::text`]
/// unchanged.
#[derive(Clone, Debug)]
pub enum Item {
    /// A coalesced run of human prompts (the old whole-queue join).  A
    /// slash-prefixed line is never part of the run — it is always its own
    /// `Human` item, per the never-merge rule.  Verbatim model text.
    Human(String),
    /// A scheduled wakeup fired — a fresh, marked item, rendered as marked
    /// chrome rather than a prompt-echo.  Its text is never interpreted as a
    /// command, even if it happens to start with `/`.
    Wakeup(String),
    /// An async agent settled — rendered as a dialable `↘` subagent block.
    Agent(AgentResult),
    /// A peer agent sent this agent a marked message.
    Message(AgentMessage),
    /// A synthetic nudge continuation the agent posted to itself.  Renders
    /// with no human chrome and, crucially, does **not** reset the exchange
    /// latches — it is the same exchange continuing.
    Nudge(String),
    /// A session-affecting slash command for the attend loop's [`Control`]
    /// (`/clear`, `/model`, `/compact`, `/quit`).  Carries the raw line.
    ///
    /// [`Control`]: crate::agent::Control
    Command(String),
    /// A detached `spawn` worker flushed its deferred `surface` batch at
    /// completion.  `agent::attend::announce`'s arm for this variant
    /// decodes `values` with the shared surface decoder and feeds the
    /// resulting cards/io into the *root* viewport (the carried `id`) exactly
    /// as a live tool run would; the model is woken with [`Self::text`]'s
    /// notice.
    Surface {
        id: AgentId,
        values: Vec<Value>,
        /// The batch's birth generation, carried through unchanged from
        /// [`Post::Surface`] for [`Agent::admits`](crate::agent::Agent::admits) to
        /// check.
        generation: u64,
    },
}

impl Item {
    /// The text the model sees when this item is drained into context —
    /// unchanged from what each source always rendered.  A `Surface` is the
    /// host's "your spawn settled" notice — it does not re-narrate the cards
    /// (those rendered on the rail), only names the spawn, its outcome, and
    /// that `await` yields its value.
    pub fn text(&self) -> String {
        match self {
            Self::Human(s) | Self::Wakeup(s) | Self::Nudge(s) | Self::Command(s) => s.clone(),
            Self::Agent(r) => r.render(),
            Self::Message(m) => m.render(),
            Self::Surface { values, .. } => surface_notice(values),
        }
    }

    /// Whether draining this item opens a new exchange — resetting the
    /// per-exchange nudge latches and (on the root path) re-minting the
    /// cancellation token — or continues the current one.  A genuine
    /// arrival opens a new exchange; a self-nudge ([`Self::Nudge`])
    /// continues the current one, so it opens nothing.
    pub fn opens_exchange(&self) -> bool {
        !matches!(self, Self::Nudge(_))
    }
}

/// The queue an [`Inbox`] consumer and its [`Mailbox`] senders share: a
/// [`VecDeque`] of [`Post`] under a `Mutex`, plus a [`Condvar`] a parked
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
    /// [`ScheduledWakeup`]: Post::ScheduledWakeup
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
    ///   ([`Post::boundary`]) always survives intact.
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
                let existing = q.iter().position(
                    |m| matches!(m, Post::ScheduledWakeup { id: eid, .. } if *eid == id),
                );
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
    /// parked consumer on success. `Err` means a non-idempotent source was
    /// at quota — the caller must surface it to its own caller as a
    /// user-facing error, never drop it silently.
    ///
    /// Takes the inbox queue mutex — callers must not hold a registry lock
    /// (the module's [lock order](self)): clone the mailbox out and push
    /// after the guard drops.
    ///
    /// # Errors
    /// Returns `Err(InboxReject)` when a non-idempotent source is already at
    /// its queue quota.
    pub fn push(&self, msg: Post) -> Result<(), InboxReject> {
        self.shared.try_push(msg)
    }

    /// Post a user-typed steering prompt — the TUI `Enter`-while-busy path.
    /// Infallible: `UserSteering` is idempotent (it merges rather than
    /// growing the queue) and never rejects.
    ///
    /// # Panics
    /// Panics if the push is rejected, which `UserSteering`'s idempotence
    /// rules out.
    pub fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt))
            .expect("UserSteering is idempotent and never rejects");
    }

    /// Whether this queue's consumer is parked at a human-input boundary. The
    /// TUI reads the focused tab's bit through its registry mailbox to drive
    /// the prompt chrome and tab-title spinner.
    pub fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// This inbox's current clear-epoch. A producer that composes a message
    /// on a thread other than the inbox's own consumer — the schedule
    /// reaper firing a wakeup — reads this at compose time and stamps it
    /// into the message, so the pop-time admission check can tell a fire
    /// that lands after an intervening [`Inbox::clear`] from one that
    /// doesn't.
    pub fn epoch(&self) -> u64 {
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
pub struct Inbox {
    shared: Arc<Shared>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    pub fn new() -> Self {
        Self {
            shared: Shared::new(),
        }
    }

    /// Mint a [`Mailbox`] sender onto this inbox's queue.
    pub fn mailbox(&self) -> Mailbox {
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
    pub fn push(&self, msg: Post) -> Result<(), InboxReject> {
        self.shared.try_push(msg)
    }

    /// Post a user-typed steering prompt through the consumer handle.
    /// Infallible — see [`Mailbox::push_user`].
    ///
    /// # Panics
    /// Panics if the push is rejected, which `UserSteering`'s idempotence
    /// rules out.
    pub fn push_user(&self, prompt: String) {
        self.push(Post::UserSteering(prompt))
            .expect("UserSteering is idempotent and never rejects");
    }

    pub fn is_empty(&self) -> bool {
        self.shared.lock().is_empty()
    }

    /// Whether the consumer is parked at a human-input boundary — true once it
    /// yields on an empty queue, cleared the moment a producer enqueues work.
    /// The chrome reads the focused tab's bit through its [`Mailbox`], not this
    /// consumer handle.
    pub fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// Queue depth per message source — the inbox's probe figures for the
    /// `/resources` fold, one `(source, count)` pair per [`Post`]
    /// variant, zeros included so the row set is stable.  Counts only,
    /// taken in one pass under the queue lock: nothing is drained,
    /// reordered, or woken — enumeration is not observation.
    pub fn source_depths(&self) -> Vec<(&'static str, u64)> {
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
    pub fn queued_user_messages(&self) -> Vec<String> {
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
    pub fn pop_back_user_all(&self) -> Option<Vec<String>> {
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
    /// steering coalesces into one [`Item::Human`].
    ///
    /// The scan stops at the first slash command: it is the only
    /// exchange-boundary item ([`Boundary::Exchange`]), and it must run against a `ReadyForUser`
    /// session, so it and everything queued behind it stay for the exchange
    /// boundary ([`Self::next_or_idle`]).  This is also what holds the human's
    /// own ordering — a `/model` then a prompt swaps before the prompt runs —
    /// since steering queued behind the command is left with it.
    ///
    /// # Panics
    /// Panics only on an internal invariant violation (a bug): every
    /// `pop_front` here follows a `front` check made in the same loop
    /// iteration, under the same lock.
    pub fn drain_steering(&self) -> Vec<Item> {
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
    pub fn next_item(&self) -> Option<Item> {
        let mut q = self.shared.lock();
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        pop_item(&mut q, epoch)
    }

    /// The attend loop's exchange-boundary pull.  Returns the next deliverable; on
    /// an empty queue the `park` verdict — recomputed on every wake — decides
    /// whether to park or terminate ([`ParkMode`]):
    ///
    /// - [`ParkMode::Held`] — a human is having a live conversation with this
    ///   agent.  Parks, ignoring cancellation entirely: an Esc cancels the
    ///   current *exchange*, not the agent.
    /// - [`ParkMode::Engaged`] — this agent does not converse, but a human has
    ///   exchanged a message with it.  Parks on an empty queue exactly like
    ///   `Held` above, but a *terminate*-cause cancellation still ends it at
    ///   once — the exchange grants no immunity, only the registry's idle
    ///   lease bounds the wait.
    /// - [`ParkMode::HeldByChildren`] — live children may still deliver a
    ///   result up this inbox.  Parks like [`ParkMode::UntilCancelled`]; a
    ///   cancellation terminates at once, and it falls through to `Quiesce`
    ///   once the last child settles.
    /// - [`ParkMode::UntilCancelled`] — a self-schedule may fire.  Parks, but a
    ///   cancellation terminates at once.
    /// - [`ParkMode::Quiesce`] — nothing will feed this agent again: returns
    ///   `None`, so an un-engaged idle returning child (and a headless trunk)
    ///   terminates at quiescence, delivering its digest upward through the
    ///   worker epilogue.
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
    pub fn next_or_idle(
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
    /// ([`Post::on_drain`]) first, so a queued-but-unconsumed
    /// `ScheduledWakeup`'s `pending` flag clears here too — `clear` does not
    /// depend on its callers also clearing the schedule registry to avoid
    /// stranding a schedule that can never fire again. Bumps the clear-epoch
    /// ([`Mailbox::epoch`]) under the same queue lock as the drain, so a
    /// schedule fire racing this call is unambiguously on one side or the
    /// other of it: swept here if its push already landed, refused at the
    /// pop if it lands only after this returns.
    pub fn clear(&self) {
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
/// [`Item::Human`], matching the push-time never-merge rule
/// ([`Shared::try_push`]): a slash line neither absorbs the run ahead of it
/// nor merges with the steering behind it, so it is always delivered alone,
/// as ordinary prompt text — the same rule [`Inbox::drain_steering`] already
/// applies at the tool boundary.  Every other source is delivered on its own
/// so the attend loop can render each in its honest medium.
///
/// `epoch` is this inbox's *current* clear-epoch, read by the caller under
/// the same lock this function runs under.  A `ScheduledWakeup` whose stamped
/// epoch has fallen behind it settled across an intervening [`Inbox::clear`]
/// and is dropped rather than converted — so the loop below keeps trying the
/// next queued message instead of surfacing a stale one.
fn pop_item(q: &mut VecDeque<Post>, epoch: u64) -> Option<Item> {
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

/// Pop the leading run of consecutive, non-slash [`Post::UserSteering`]
/// entries off `q` and join them with a blank line into one [`Item::Human`] —
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

/// Convert one non-user-steering message into the [`Item`] it delivers,
/// running its drain side effect ([`Post::on_drain`]) — or `None` for a
/// [`ScheduledWakeup`](Post::ScheduledWakeup) whose stamped epoch has
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

/// One [`Kind`] stamped with the [`AgentId`] that produced it — the unit
/// [`BusSender`]/[`BusReceiver`] carry and [`Sink::handle`] consumes.
pub struct Event {
    pub id: AgentId,
    pub kind: Kind,
}

/// Prefix of the [`Kind::Error`] message [`pump`] emits when the worker thread
/// unwinds.
///
/// Shared so a sink can recognise a recovered panic without
/// matching on free text (the headless result reports it as an error rather
/// than a clean completion).
pub const WORKER_PANIC_PREFIX: &str = "worker panicked: ";

pub enum Kind {
    Born {
        log_dir: PathBuf,
        /// This agent's name — its identity everywhere the model can see it
        /// (ASCII alnum / `-` / `_`, 1–24 chars).  The TUI surfaces it in the
        /// tab bar; headless ignores it.
        name: String,
        /// The spawning agent's id — the tab's parent.  The TUI records it so
        /// that when a focused agent ends (`reply`), focus falls back to its
        /// parent, recursing toward the trunk.
        parent: AgentId,
        /// A `/branch` tab (a conversing fork of its parent) rather than a
        /// returning sub-agent.  The TUI records it so `/close` admits only a
        /// branch tab.
        branch: bool,
    },
    Died,
    Token(String),
    /// A live reasoning token, streamed during the model's thinking phase.
    /// Accumulated by the frontend into a provisional deliberation seat until
    /// the final `Reasoning` event commits a real thinking block.
    Thinking(String),
    /// Ends the current streaming step: the frontend flushes whatever
    /// `Token`/`Thinking` text is still open into a committed block
    /// (`tui::viewport::close_boundary`) before the next step or exchange begins.
    /// Interactive-only chrome — no transcript record, since it carries no
    /// content of its own.
    Boundary,
    /// The step's final model reasoning. The frontend commits `text` as a
    /// standalone dialable thinking block; `answer_chars` is the whole turn's
    /// answer mass, the deliberation grain's denominator.
    Reasoning {
        text: String,
        answer_chars: u32,
    },
    Usage(Usage),
    Step {
        /// Steps in the current segment: restarts at 1 each time this
        /// attend loop resumes (e.g. after an async-spawned block settles),
        /// so a run-wide step count is the consumer's own running tally
        /// ([`crate::headless::Headless::steps`]), not this field.
        n: u32,
        tuning: Tuning,
    },
    /// A transient label for the worker's current synchronous phase —
    /// "awaiting model", "compacting".  Emitted before a long op so the
    /// frontend can name what the worker is doing during an otherwise silent
    /// gap (a progress label alongside the spinner); the headless
    /// `events.json` keeps it for post-mortem.  Superseded by the next event
    /// of any kind.
    Phase(String),
    /// A call to `ral` — the one call that genuinely crosses the provider
    /// boundary.  See [`Kind::HarnessCall`] for a desk verb's rail-identical
    /// twin.
    ToolCall {
        tool: &'static str,
        cmd: String,
        /// Short, single-line label the sink shows on the rail — `ral`'s
        /// mandatory `description` — with `cmd` revealed when the user
        /// opens the call. `None` means there is nothing to reveal, so the
        /// call renders statically from `cmd` (the invalid-input rail
        /// header).
        summary: Option<String>,
    },
    ToolResult(String),
    /// A desk verb **acted** — `spawn`, `cancel`, `message`, `reply`,
    /// `schedule`, `unschedule`.  An act changes the world *outside* the
    /// exchange: it starts a process, fills another agent's inbox, arms a wakeup,
    /// ends a run.  A [`Kind::ToolCall`] only observes, so the two never
    /// share a rendered shape — and an act's whole
    /// information content is these three fields, never a host-authored
    /// sentence about them.
    HarnessCall {
        /// The bare imperative the model invoked.
        verb: &'static str,
        /// What the act names: the agent name or schedule label. `None` for
        /// an act that addresses nothing (`reply` answers its parent).
        subject: Option<String>,
        /// The act's argument — the launch prompt, the message text, the
        /// trigger description, the replied value — or, when `failed`, the
        /// short reason it was refused.  Empty when the act carries no
        /// argument and simply landed (`cancel`, `unschedule`).
        payload: String,
        /// Whether the act was refused.  The refusal's *long* form is the
        /// raise, which reaches the model through captured stderr; this bit
        /// only tiers the row.
        failed: bool,
    },
    /// The paired result for a [`Kind::HarnessCall`] — the desk's twin of
    /// [`Kind::ToolResult`], and a *forensic* record only: the act row says
    /// on screen everything the act has to say, and the model's copy of a
    /// refusal travels the raise.  Its one consumer is
    /// [`crate::agent::transcript`].
    HarnessResult(String),
    /// The text of an item as it enters context — a human prompt, a wakeup,
    /// or a peer agent message alike (`agent::attend::announce`), despite the name;
    /// an agent result renders through [`Kind::SubagentDone`] instead.
    /// Interactive-only: no transcript record, and the TUI disarms its
    /// post-cancel drain guard on the first one after a clear.
    UserPromptEcho(String),
    StopReason(String),
    Error(String),
    /// An operational note the agent's attend loop issued — a truncation recovery,
    /// a compaction step.  Names *what happened*, like every other operational
    /// `Kind`; the display renders it dim, but *dimness is the renderer's
    /// choice*, not a fact in the vocabulary.  The trace records it as
    /// `system_note`; it has no `events.json` twin, since it is not a message
    /// the model saw.
    SystemNote(String),
    /// A recovery nudge the attend loop issued between attempts.  The trace records
    /// it; the display surfaces it as it sees fit (a stderr line in headless,
    /// quiet on the TUI rail).  Its `events.json` twin is the model-view
    /// forensic breadcrumb.
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    ProviderError(ProviderErrorRecord),
    /// Emitted by the `agent` tool when a subagent finishes — *after*
    /// the child's own [`Kind::Died`] and *before* the spawn rejoins the
    /// parent's tool result.  The event's session id is the parent
    /// (typically root); the TUI lands the breadcrumb in root's
    /// scrollback regardless of nesting depth, since subagent output
    /// otherwise lives only in its own tab and ages out at `LINGER`.
    SubagentDone {
        name: String,
        /// How the child settled.  The sink reduces this with `text` through
        /// [`AgentOutcome::breadcrumb`] to the body / header-suffix split —
        /// the same reduction an async result makes — so sync and async land
        /// the identical dialable block.
        outcome: AgentOutcome,
        /// The subagent's final assistant text — empty when the run
        /// failed or was cancelled.
        text: String,
        elapsed: Duration,
    },
    /// A render document a ral kit handed to the `surface` builtin: an
    /// ordered stack of Bertin [`Card`] marks (a diff, a measure, a fields
    /// matrix, roled text, raw ink) composed in ral and decoded once by
    /// [`shell_eval`] onto the bus.  Always rendered — a surfaced card is a
    /// deliberate user-facing act.  The open set of cards over a closed set
    /// of marks is what keeps the renderer total while the kit invents new
    /// cards in pure ral.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Card(Card),
    /// A detached worker's `` `done `` completion event, decoded once by
    /// [`shell_eval`] into its typed [`DoneOutcome`](crate::bus::card::DoneOutcome)
    /// and paired with the one-line [`Card`] composed from it — the
    /// [`Kind::Io`] pattern, so `transcript.jsonl` records how the worker
    /// settled (a clean return, a raised error, a panic) rather than only
    /// the card's ink.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Done {
        outcome: crate::bus::card::DoneOutcome,
        card: Card,
    },
    /// A structural I/O event core surfaced (a read, write, exec, or grep),
    /// decoded once by [`shell_eval`] into a typed [`IoEvent`] and paired with
    /// the [`Card`] composed from it.  The bus carries *both*: the rendered
    /// card is what the rail draws, while the raw `event` keeps the structure
    /// the mark tree erases, so `transcript.jsonl` records the effect itself;
    /// the card is a presentation (the rail, the `user.log`) and is not.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Io {
        event: IoEvent,
        card: Card,
    },
    /// Kit-authored *state* pinned to a keyed register slot — the
    /// model-authored dual of the matrix.  The `` `pin [key, body] `` surface
    /// decodes here, writing `card` to slot `key` and **overwriting in place**
    /// on re-pin.  Unlike [`Kind::Card`], a pin is neither logged nor landed in
    /// scrollback: it is what is *currently true*, not a thing that happened, so
    /// it is rendered ambiently in the reserved register column and updated
    /// where it sits.
    Pin {
        key: String,
        card: Card,
    },
    /// Drop a pinned register slot: the `` `unpin [key] `` surface, or a `` `pin ``
    /// whose body is absent or empty.  A finished plan clears its gauge.
    Unpin {
        key: String,
    },
    /// A ready-boundary housekeeping fact core's own engine pushed as a
    /// `` `notice `` surface class — a worker the lease chain reaped
    /// (unobserved past its idle bound, past its absolute backstop, or the
    /// retention sweep expiring a settled entry's unclaimed result), or a run
    /// of idle top-level bindings the ledger pruned. Core emits the fact itself,
    /// at the ready boundary, through the run's surface sink, rather than a
    /// host draining an accessor and composing the event from what it read.
    /// Unlike [`Kind::Card`], the decoded [`Notice`](crate::bus::card::Notice)
    /// rides alongside the rendered `card` (the [`Kind::Io`] pattern) so
    /// `transcript.jsonl` keeps the structural fact the one-liner erases.
    /// Never model-facing — no `events.json` twin, no inbox message: the
    /// idle-observation lease already bounds a forgotten worker's cost to at
    /// most an hour of one seat in the per-agent cap, so nothing is gained
    /// by also telling the model directly.
    Notice {
        notice: crate::bus::card::Notice,
        card: Card,
    },
    /// The `/resources` probe fold: the agent's own accumulator rows,
    /// assembled on its attend thread at the exchange boundary the command
    /// drains at, beside the card rendering them — the raw-fact/rendering
    /// pairing of [`Kind::Io`] and [`Kind::Notice`], so
    /// `transcript.jsonl` records the rows while the card stays a
    /// presentation.  The TUI appends the rows for the accumulators *it*
    /// owns (viewports, views, the bus) to the card at render time; those
    /// stay frontend-side.  Never model-facing: no `events.json` twin, no
    /// inbox reply — probing is for the operator, and it mutates and
    /// renews nothing.
    Resources {
        rows: Vec<crate::agent::resources::ProbeRow>,
        card: Card,
    },
}

/// A run-scoped usage accumulator.
///
/// Where a [`Transcript`] is **per-session**
/// — each session records its own trace — this meter is **per-run**: the root
/// and every child, muted or live, share the single instance the
/// [`FleetBus`] mints.  That shared lifetime is the whole point: an async
/// sub-agent's display channel is dead in headless, so its usage never reaches
/// a sink, but its `emit` still tees here, so the run total counts it.
/// Accounting follows the event, not its emitter, exactly as recording does.
#[derive(Clone, Default)]
pub struct UsageMeter(Arc<Mutex<Usage>>);

impl UsageMeter {
    /// Fold one usage delta into the run total.
    pub fn add(&self, u: Usage) {
        *self.0.lock_ignore_poison() += u;
    }

    /// The run total so far.
    pub fn total(&self) -> Usage {
        *self.0.lock_ignore_poison()
    }
}

// ---------------------------------------------------------------------------
// The bounded, coalescing transport
// ---------------------------------------------------------------------------
//
// `Emitter`/`FleetBus` carry events through [`BusSender`]/[`BusReceiver`]: a
// bounded, coalescing queue, so a producer flood (a token stream the
// renderer can't keep up with) is capped rather than growing heap without
// limit. The pair exposes the same `Sender`/`Receiver`-shaped
// API (`send`, `try_recv`, `recv_timeout`, even reusing `std::sync::mpsc`'s
// own error types), so [`drain_pass`]/[`Sink::drive`] and every call site
// need only name the type, not change the logic.
//
// THE MERGE RULE: pushing a coalescible [`Kind`] — `Token`/`Thinking`
// (concatenate) or `Phase` (replace; its own doc already declares
// superseded-by-next semantics) — merges into the queue's TAIL entry *iff*
// that tail is the same class and the same agent id; every other `Kind` is
// reserved and always pushed as its own entry, never merged, never dropped.
// A token run can therefore never migrate across a `ToolCall`/`Born`/`Died`
// of the same agent (ordering is preserved by construction), and a flood
// bounds itself to one growing entry rather than one entry per token.
//
// ELISION: a merged `Token`/`Thinking` entry's accumulated text is capped at
// [`MERGE_TEXT_CAP`]; past it, the front of the text is dropped (the newest
// tail survives) and the drop count rides to one [`Kind::SystemNote`]
// overflow marker the next time the entry is drained — degradation the user
// sees, never silence. `Phase` replaces outright and never elides.

/// Coalescible class a [`Kind`] belongs to. `None` (every other variant) is
/// reserved: always pushed, never merged, never dropped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeClass {
    /// [`Kind::Token`] — concatenates.
    Token,
    /// [`Kind::Thinking`] — concatenates.
    Thinking,
    /// [`Kind::Phase`] — replaces.
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
/// operational-note vocabulary ([`Kind::SystemNote`]) — transcript-recorded
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
    /// panicking — the same policy as [`Shared::lock`], for the same reason:
    /// every operation under this lock ([`BusQueue::push`], [`pop_one`]) is a
    /// total mutation, so a panicked holder cannot leave it torn, and
    /// propagating the poison would deafen every future sender and receiver
    /// over one unrelated panic.
    fn lock(&self) -> MutexGuard<'_, BusQueue> {
        self.state.lock_ignore_poison()
    }
}

/// The cloneable sender side of the bus's bounded, coalescing queue — the
/// `mpsc::Sender<Event>` replacement threaded through [`Emitter`]/[`FleetBus`].
///
/// Public alongside [`Emitter::new`]/[`Emitter::with_mailbox`], which take
/// one directly (the integration-test harness builds its own).
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
    /// lets [`Emitter::muted_child`] swallow a display stream forever without
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
/// behind [`FleetBus`]/[`Emitter`].
pub fn channel() -> (BusSender, BusReceiver) {
    let shared = Arc::new(BusShared {
        state: Mutex::new(BusQueue::new()),
        signal: Condvar::new(),
        receiver_alive: AtomicBool::new(true),
        senders: AtomicUsize::new(1),
    });
    (BusSender(shared.clone()), BusReceiver(shared))
}

#[derive(Clone)]
pub struct Emitter {
    tx: BusSender,
    id: AgentId,
    /// The **owning** session's mailbox — root's for the root emitter, the
    /// child's own for a child emitter.  Used by the `spawn` boundary sink to
    /// post a deferred surface batch into the agent that ran the spawn.  An
    /// emitter never carries another agent's mailbox.
    mailbox: Mailbox,
    /// Whether this emitter's channel outlives the spawning exchange, so a
    /// *detached* worker (an async `agent` child) may clone it for a live
    /// tab.  The TUI's session-lived bus sets it; headless's per-exchange bus
    /// leaves it `false`, keeping async children muted *on the display* —
    /// bus lifetime is a TUI property, not a core obligation.  It does not
    /// gate [`Self::transcript`]: a muted child still records its own trace.
    session_lived: bool,
    /// This emitter's owning session's [`Transcript`].  Every [`Self::emit`]
    /// tees here, so the session's operational trace is written at the emit
    /// seam — independent of who drains the live bus for display, and so a
    /// child muted off a per-exchange bus still records its full trace.
    transcript: Transcript,
    /// The run's [`UsageMeter`], shared by the root and every child.  Every
    /// [`Self::emit`] of a [`Kind::Usage`] tees here too, so the run total is
    /// accumulated at the emit seam regardless of display muting — the same
    /// reasoning that puts the transcript record here, but per-run not
    /// per-session.
    meter: UsageMeter,
}

impl Emitter {
    /// An emitter with a standalone, orphan mailbox and no transcript — for
    /// tests, whose events land nowhere durable.
    pub fn new(tx: BusSender, id: AgentId) -> Self {
        Self::with_mailbox(tx, id, Inbox::new().mailbox())
    }

    /// [`Self::new`] with a caller-supplied mailbox instead of a throwaway
    /// one — what the integration-test harness uses to wire its own inbox
    /// through (see [`BusSender`]'s doc).
    pub fn with_mailbox(tx: BusSender, id: AgentId, mailbox: Mailbox) -> Self {
        Self {
            tx,
            id,
            mailbox,
            session_lived: false,
            transcript: Transcript::none(),
            meter: UsageMeter::default(),
        }
    }

    /// A muted child emitter derived from this (parent) emitter: a dead display
    /// channel and an orphan mailbox, but a live [`Transcript`] *and* the
    /// parent run's [`UsageMeter`].  The headless async child takes this — it
    /// streams nowhere (its receiver is already dropped) yet still records its
    /// own operational trace and tees its usage to the inherited run meter,
    /// because recording and accounting are run properties, not display ones.
    /// Display-muted, accounting-live.
    pub fn muted_child(&self, id: AgentId, transcript: Transcript) -> Self {
        let (tx, _rx) = channel();
        Self {
            tx,
            id,
            mailbox: Inbox::new().mailbox(),
            session_lived: false,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// A sibling emitter for a child session: the same event channel and
    /// session-lived flag, stamped with the child's id, carrying the
    /// **child's own** mailbox and **own** [`Transcript`] but the **shared**
    /// run [`UsageMeter`] — so the child's surface batches land in the child's
    /// box and its events in the child's trace, never the parent's, while its
    /// usage still folds into the one run total.
    pub fn child(&self, id: AgentId, mailbox: Mailbox, transcript: Transcript) -> Self {
        Self {
            tx: self.tx.clone(),
            id,
            mailbox,
            session_lived: self.session_lived,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// Record `kind` to this session's [`Transcript`], fold it into the run
    /// [`UsageMeter`] if it is a [`Kind::Usage`], then send it — in that
    /// order, so recording and accounting never depend on a live receiver
    /// ([`Self::muted_child`]'s whole point).
    pub fn emit(&self, kind: Kind) {
        self.transcript.record(self.id, &kind);
        if let Kind::Usage(u) = &kind {
            self.meter.add(*u);
        }
        let _ = self.tx.send(Event { id: self.id, kind });
    }

    /// The owning session's mailbox, for the `spawn` boundary sink that posts
    /// a deferred surface batch back into this agent's own inbox.
    pub fn mailbox(&self) -> Mailbox {
        self.mailbox.clone()
    }

    /// Whether a detached worker may clone this emitter for a live tab.
    /// True only off a session-lived bus ([`FleetBus::session`]); an async
    /// `agent` reads it to choose a streaming tab over its muted log.
    pub fn is_session_lived(&self) -> bool {
        self.session_lived
    }

    /// This emitter's owning session's [`Transcript`] — for a deferred
    /// callback (a spawn worker's boundary sink) that must outlive the exchange
    /// and so cannot hold a clone of this emitter itself: a `Transcript` is
    /// a durable file handle, not a bus channel end, so holding one long
    /// past this exchange never keeps a `pump`/`drive` completion waiting on a
    /// sender that will not drop (the daemon-task-hang class of bug this
    /// module's [`drain_pass`] doc already guards against).
    pub fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }
}

/// The event channel and its inbox, owned for as long as the host wants a
/// worker→frontend bus to live.  Two lifetimes, one type:
///
/// - [`Self::session`] — minted once at REPL start and held for the whole
///   session.  Each exchange's foreground worker and every detached async child
///   clone its sender (session-lived, so a background child gets a live tab);
///   the idle wait drains it as a third source.
/// - [`Self::per_exchange`] — minted fresh for one exchange (headless, tests), so the
///   channel closes when the exchange's worker finishes.  Its emitters are *not*
///   session-lived: an async child stays muted *on the display* (it never
///   streams to a live tab) — the observable display behaviour headless has
///   always had.  It still records its own `transcript.jsonl`, since recording
///   rides the emitter, not the channel's lifetime.
///
/// Either way [`pump`] borrows the channel — completion is the per-exchange
/// `done` flag, never the channel's lifetime.
pub struct FleetBus {
    tx: BusSender,
    rx: BusReceiver,
    mailbox: Mailbox,
    session_lived: bool,
    /// The one run-scoped [`UsageMeter`] every emitter minted from this bus
    /// shares — the root through [`Self::emitter`], each child through
    /// [`Emitter::child`] / [`Emitter::muted_child`].  [`Self::usage_total`]
    /// reads it for the run total, independent of which sink (if any) drains
    /// the bus.
    meter: UsageMeter,
}

impl FleetBus {
    /// A session-lived bus over `inbox` (the TUI root's inbox).  Emitters
    /// minted from it are session-lived, so detached async children stream.
    pub fn session(inbox: &Inbox) -> Self {
        Self::build(inbox.mailbox(), true)
    }

    /// A per-exchange bus over `inbox` (headless / tests).  Emitters are not
    /// session-lived, so async children stay muted on the display (they still
    /// record their own trace).
    pub fn per_exchange(inbox: &Inbox) -> Self {
        Self::build(inbox.mailbox(), false)
    }

    fn build(mailbox: Mailbox, session_lived: bool) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            mailbox,
            session_lived,
            meter: UsageMeter::default(),
        }
    }

    /// The receiver the exchange's [`Sink`] drains.
    pub(crate) fn rx(&self) -> &BusReceiver {
        &self.rx
    }

    /// An [`Emitter`] stamped with `id`, sharing this bus's sender, root
    /// mailbox, session-lived flag, and run [`UsageMeter`].  The root attend
    /// thread takes one; a child emitter is derived with [`Emitter::child`] /
    /// [`Emitter::muted_child`], inheriting the same meter.
    pub fn emitter(&self, id: AgentId, transcript: Transcript) -> Emitter {
        Emitter {
            tx: self.tx.clone(),
            id,
            mailbox: self.mailbox.clone(),
            session_lived: self.session_lived,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// The run's total usage so far, summed across the root and every child at
    /// the emit seam — the single source of truth for the headless result,
    /// independent of display muting.
    pub fn usage_total(&self) -> Usage {
        self.meter.total()
    }
}

/// How often the completion-aware drain loop wakes to re-check the `done`
/// flag while no event is arriving.  Small enough that an exchange returns
/// promptly after its worker finishes, large enough not to spin.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The verdict of one [`drain_pass`]: the explicit-done completion contract,
/// shared by every driver.
pub(crate) enum Pass {
    /// The worker is done (or the channel disconnected) and the buffered
    /// batch has been handled — render a final frame and return.
    Stop,
    /// The channel went empty and the worker is not done: the loop is idle
    /// until the next event arrives.
    Idle,
    /// The batch cap was reached before the channel emptied: more events are
    /// already queued, so drain again without waiting.
    More,
}

/// One pass of the explicit-done completion contract — the single place that
/// decides when an exchange's event loop ends, shared by the headless default
/// [`Sink::drive`] and the TUI's `tui_loop::ui_loop`.
///
/// Drains up to `max` available events through `handle`, then reports the
/// channel's state. **Completion is `done` being set — the worker finished —
/// never the channel emptying or disconnecting:** a detached worker (a
/// `spawn`ed server, a live background `agent`) may hold a sender clone forever
/// and keep the channel non-empty, but it never decides the exchange is over,
/// because the loop stops on the explicit `done` flag, not on the channel.
/// This is the daemon-task-hang fix, factored so the two drivers cannot drift
/// on it.
///
/// `done` is checked *before each receive*, so the pass ends the moment the
/// worker finishes even while concurrent background producers keep the channel
/// full — it does not wait for a momentarily-empty channel, which under a
/// background flood would never come. On `done` it drains the buffered batch up
/// to `max` (so the caller can render a final frame including the worker's last
/// events) and returns [`Pass::Stop`]; any events still buffered past `max` are
/// left for the caller's next pass — the TUI's exit path runs one final
/// uncapped pass for exactly this reason. `None` `max` drains every buffered event
/// (headless, which has nothing to render between them); `Some(n)` caps one
/// pass so a flood cannot starve the TUI's input poll between passes, reporting
/// [`Pass::More`] so the caller drains again. Disconnect also stops.
pub(crate) fn drain_pass(
    rx: &BusReceiver,
    done: &AtomicBool,
    max: Option<usize>,
    mut handle: impl FnMut(Event),
) -> Pass {
    // Latch `done` once at the top: the worker sets it after `work` returns,
    // so reading it before draining means a finishing worker's already-queued
    // events are still handled in this pass, and the pass cannot loop forever
    // chasing a channel that a background producer keeps non-empty.
    let finished = done.load(Ordering::Acquire);
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            // The batch cap bounds even a `done` drain, so a huge backlog from
            // the finished worker (or background producers) cannot block the
            // final frame; the caller drains the rest from the idle path.
            return if finished { Pass::Stop } else { Pass::More };
        }
        match rx.try_recv() {
            Ok(ev) => {
                handle(ev);
                n += 1;
            }
            // The buffered batch is exhausted. Stop if the worker has
            // finished; otherwise the loop is idle until the next event.
            Err(TryRecvError::Empty) => {
                return if finished { Pass::Stop } else { Pass::Idle };
            }
            Err(TryRecvError::Disconnected) => return Pass::Stop,
        }
    }
}

/// One presentation surface.
///
/// [`Self::handle`] consumes a single event
/// synchronously; [`Self::drive`] drains the channel until the worker signals
/// completion through `done`.  Completion is an explicit control-flow fact —
/// the worker finished — *not* the channel disconnecting: a detached worker
/// (a `spawn`ed server) may outlive the exchange, but it holds bounded deferred
/// surface storage in core, never a clone of this channel's sender, so it
/// cannot keep the loop alive.  The default [`Self::drive`] is what the
/// headless frontend (and the tests) run; it blocks on the channel between
/// passes.  The TUI is *not* a `Sink` — its `ui_loop` drains the bus through
/// the same [`drain_pass`] primitive but on its own ~60 FPS render cadence,
/// polling keys between passes — so the completion contract stays identical
/// across both even though only one of them implements this trait.
pub trait Sink {
    fn handle(&mut self, e: Event);

    fn inbox(&self) -> Inbox {
        Inbox::new()
    }

    /// # Errors
    /// Returns `Err` if an implementation's surface write fails; the default
    /// drain-and-render loop is infallible.
    fn drive(&mut self, rx: &BusReceiver, done: &AtomicBool) -> io::Result<()> {
        loop {
            // The shared completion contract. `None` max drains every buffered
            // event — headless has nothing to render between them, so it never
            // needs the TUI's batch cap.
            match drain_pass(rx, done, None, |ev| self.handle(ev)) {
                Pass::Stop => return Ok(()),
                // Idle (an uncapped pass never reports `More`): block on the
                // channel for the next event, waking each `DRAIN_POLL` to
                // re-check `done`. A detached worker's sender keeps the channel
                // from disconnecting, so the timeout — not disconnect — is what
                // lets the next `done` re-check run.
                Pass::Idle | Pass::More => match rx.recv_timeout(DRAIN_POLL) {
                    Ok(ev) => self.handle(ev),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
            }
        }
    }
}

/// Run `work` on a scoped thread over `bus`'s channel, drive `sink`, join.
///
/// A worker panic is reported through the still-open [`Emitter`] as a
/// final [`Kind::Error`]; the function returns `None` in that case.
///
/// The channel belongs to `bus`, not to `pump`: a session-lived bus keeps it
/// open across exchanges (the TUI, so a background `agent` streams a live tab),
/// while a per-exchange bus closes it when the worker finishes (headless / tests).
/// Either way completion is explicit — the worker sets `done` after `work`
/// returns (or unwinds), and the drain stops on that flag, never on the
/// channel's state.  A detached worker holding a sender clone forever, on
/// either bus, cannot keep the loop — hence the exchange — from ending.
///
/// # Errors
/// Returns `Err` if driving `sink` over the bus fails (propagated from
/// [`Sink::drive`]).
pub fn pump<S, R>(
    sink: &mut S,
    bus: &FleetBus,
    root_id: AgentId,
    transcript: Transcript,
    work: impl Send + FnOnce(&Emitter) -> R,
) -> io::Result<Option<R>>
where
    S: Sink,
    R: Send,
{
    // Declared outside the scope so the borrow into both the worker thread
    // and `drive` outlives the spawned thread's `'env`.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let emit = bus.emitter(root_id, transcript);
    std::thread::scope(|s| -> io::Result<Option<R>> {
        let h = s.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&emit)));
            if let Err(p) = &r {
                let msg = crate::agent::panic_msg(p);
                emit.emit(Kind::Error(format!("{WORKER_PANIC_PREFIX}{msg}")));
            }
            // Signal completion before the worker's `emit` (and its sender
            // clone) drops: the exchange is over because the worker finished.
            done_ref.store(true, Ordering::Release);
            r.ok()
        });
        sink.drive(bus.rx(), done_ref)?;
        Ok(h.join().ok().flatten())
    })
}

#[cfg(test)]
pub(crate) fn dummy_emitter() -> (Emitter, BusReceiver) {
    let (tx, rx) = channel();
    (Emitter::new(tx, 0), rx)
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMessage, Boundary, Emitter, Event, FleetBus, INBOX_SOURCE_CAP, Inbox, Post,
        InboxReject, Item, Kind, MERGE_TEXT_CAP, ParkMode, Pass, Sink, Transcript, channel,
        drain_pass, pump,
    };
    use crate::agent::cancel;
    use crate::provider::Tuning;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc::TryRecvError;

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

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

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

    /// The headless default [`Sink::drive`] and the TUI's `tui_loop::ui_loop`
    /// share one completion contract: [`drain_pass`]. It stops when the worker is
    /// *done*, never when the channel empties or disconnects — so a detached
    /// worker holding a sender clone cannot keep an exchange alive. Pinning the
    /// shared primitive directly is what keeps the two drivers from drifting on
    /// the daemon-task-hang fix.
    #[test]
    fn drain_pass_stops_on_done_with_a_live_detached_sender() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        // A detached holder keeps a sender clone alive forever — the channel
        // never disconnects, exactly as a `spawn`ed server would.
        let holder = tx.clone();

        tx.send(Event {
            id: 0,
            kind: Kind::Step {
                n: 1,
                tuning: Tuning::default(),
            },
        })
        .unwrap();
        tx.send(Event {
            id: 0,
            kind: Kind::Step {
                n: 2,
                tuning: Tuning::default(),
            },
        })
        .unwrap();
        done.store(true, Ordering::Release);

        let mut seen = 0usize;
        // `None` max is the headless drain; `Some(BATCH)` would be the TUI's.
        // Both reach `Stop` here: `done` is set and the buffered events drain.
        assert!(
            matches!(drain_pass(&rx, &done, None, |_| seen += 1), Pass::Stop),
            "must stop once the worker is done"
        );
        assert_eq!(seen, 2, "every buffered event is handled before stopping");
        // The detached sender is still alive — completion did not depend on it.
        assert!(
            holder
                .send(Event {
                    id: 0,
                    kind: Kind::Died
                })
                .is_ok(),
            "the detached sender outlived the stop"
        );
    }

    /// The batch cap bounds one pass (TUI policy: don't let a token flood
    /// starve the input poll), reporting `More`; an empty channel with the
    /// worker not done reports `Idle` (the headless wait state).
    #[test]
    fn drain_pass_caps_batch_as_more_and_reports_idle_when_empty() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        for _ in 0..3 {
            tx.send(Event {
                id: 0,
                kind: Kind::Boundary,
            })
            .unwrap();
        }

        let mut seen = 0usize;
        // Cap below the queue depth: the cap is hit before the channel empties.
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::More),
            "a full batch reports More"
        );
        assert_eq!(seen, 2, "the batch cap bounds one pass");
        // Drain the remainder: the channel empties with the worker not done.
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::Idle),
            "an empty channel with no done reports Idle"
        );
        assert_eq!(seen, 3, "the rest drains on the next pass");
    }

    /// The session-lifetime refinement: a finished foreground exchange must stop
    /// even while the channel is *non-empty*, because concurrent background
    /// producers (a live async `agent`) keep it full and the old
    /// "stop only on a momentarily-empty channel" rule would never fire. On
    /// `done`, `drain_pass` drains the buffered batch (up to the cap) and
    /// returns `Stop`; anything still buffered past the cap is left for the
    /// caller's next pass. Without the fix the foreground exchange would hang
    /// exactly when a background agent is flooding the bus.
    #[test]
    fn drain_pass_stops_on_done_even_while_a_background_producer_floods() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        // A background producer keeps sending — the channel is never empty.
        // A reserved kind (never merged) keeps each send its own queue entry,
        // so this test isolates the batch cap from the merge rule, which has
        // its own coverage below (`bus_queue_token_flood_coalesces_...`).
        let background = tx;
        for _ in 0..200 {
            background
                .send(Event {
                    id: 9,
                    kind: Kind::ToolResult("x".into()),
                })
                .unwrap();
        }
        // The foreground worker finishes while the channel is still full.
        done.store(true, Ordering::Release);

        // A capped pass (the TUI) drains its batch and stops, never `More`-ing
        // forever against the flood; the cap bounds the final frame's work.
        let mut seen = 0usize;
        assert!(
            matches!(drain_pass(&rx, &done, Some(64), |_| seen += 1), Pass::Stop),
            "a finished worker stops the pass even though the channel is non-empty"
        );
        assert_eq!(seen, 64, "the buffered batch is drained up to the cap");
        // The background producer is unaffected — completion never depended on
        // the channel draining or disconnecting.
        assert!(
            background
                .send(Event {
                    id: 9,
                    kind: Kind::Died
                })
                .is_ok(),
            "the background producer outlives the foreground stop"
        );
    }

    /// Completion is the worker finishing, not the channel disconnecting.
    /// A "detached holder" keeps a clone of the worker's [`Emitter`] (hence a
    /// live `Sender`) alive past the worker's return — modelling a `spawn`ed
    /// server that never terminates.  `pump` must still return promptly,
    /// driven by the explicit `done` flag, while that sender is still alive.
    /// Regression for the daemon-task hang.
    #[test]
    fn pump_returns_on_worker_done_not_sender_disconnect() {
        struct CountSink(usize);
        impl Sink for CountSink {
            fn handle(&mut self, _e: Event) {
                self.0 += 1;
            }
        }

        let mut sink = CountSink(0);
        // A session-lived bus, as the TUI uses it: its sender outlives the
        // exchange, so a detached worker's clone never disconnects the channel.
        let bus = FleetBus::session(&Inbox::new());
        // Outlives `pump`: holds an `Emitter` clone whose `Sender` keeps the
        // channel from ever disconnecting, exactly as a detached worker would.
        let holder: Mutex<Option<Emitter>> = Mutex::new(None);

        let t0 = Instant::now();
        let r = pump(&mut sink, &bus, 0, Transcript::none(), |emit| {
            *holder.lock().unwrap() = Some(emit.clone());
            emit.emit(Kind::Step {
                n: 1,
                tuning: Tuning::default(),
            });
            "done"
        })
        .expect("pump returns Ok");

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "pump must return on the explicit done signal, not wait for sender disconnect (took {:?})",
            t0.elapsed()
        );
        assert_eq!(r, Some("done"), "pump returns the worker's value");
        assert_eq!(sink.0, 1, "the worker's one event was delivered");
        // The detached sender is still alive — proof completion did not
        // depend on it dropping.
        assert!(holder.lock().unwrap().is_some());
    }

    /// Tool-boundary drain stops at the first slash-prefixed steering line.
    /// It waits for the exchange boundary like a real [`Post::Command`], but
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

    /// A `Surface` drains at the tool boundary as a [`Item::Surface`] in the
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

    /// The run meter counts a muted child's usage.  Accounting follows the
    /// event, not its emitter: a muted child's display channel is dead (its
    /// receiver dropped, so a sink never sees its [`Kind::Usage`]), yet it shares
    /// the root's run meter through [`Emitter::muted_child`], so `bus.usage_total()` sums
    /// the root *and* the muted child — exactly the headless under-reporting
    /// this fixes.
    #[test]
    fn usage_meter_counts_a_muted_child_on_a_dead_channel() {
        use crate::provider::Usage;

        let root_usage = Usage {
            input: 100,
            output: 20,
            dollars: 0.5,
            ..Usage::default()
        };
        let child_usage = Usage {
            input: 7,
            output: 3,
            dollars: 0.125,
            ..Usage::default()
        };

        let bus = FleetBus::per_exchange(&Inbox::new());
        let root = bus.emitter(0, Transcript::none());
        // The muted child: a fresh dead channel, but the root run's meter.
        let child = root.muted_child(1, Transcript::none());

        root.emit(Kind::Usage(root_usage));
        child.emit(Kind::Usage(child_usage));

        let total = bus.usage_total();
        assert_eq!(total.input, 107, "the muted child's input is counted");
        assert_eq!(total.output, 23, "the muted child's output is counted");
        assert!(
            (total.dollars - 0.625).abs() < f64::EPSILON,
            "the muted child's cost is counted",
        );
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

    // ── the bounded, coalescing bus transport (7a) ─────────────────────────

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
    /// [`Emitter::muted_child`] pattern relies on this to swallow a display
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

    // ── inbox quotas without silent loss (7c) ──────────────────────────────

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
    /// exchange-boundary classification ([`Post::boundary`]).
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
